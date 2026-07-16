//! Scan a Grid's visible rows for clickable spans — URLs, email
//! addresses, and absolute filesystem paths.  Per-frame work, kept
//! cheap on purpose: the focused grid is at most ~100×80 and a
//! single linear pass over each row turns it into a few hundred
//! microseconds on M-series.  No per-byte allocation; the only
//! allocation is the resulting `Vec<LinkRange>` and the per-row
//! line `String` that gets reused across rows.
//!
//! Detection is intentionally conservative — false negatives are
//! preferable to false positives because a wrongly-detected span
//! is a real annoyance (decorates `cargo` or `git/...` as a path
//! when it isn't).  For File kind specifically the pattern match
//! is followed by a `stat()` check (with a small TTL cache) so
//! `/foo` only underlines when `/foo` actually exists.  URL pattern
//! is tightened structurally — we don't open the network just to
//! validate a host.

use crate::grid::Grid;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Three flavours the scanner recognises.  Render attaches a colour
/// per kind; the click handler dispatches per kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkKind {
    /// `http://` or `https://` URL.
    Url,
    /// Absolute path (`/...`) or home-relative path (`~/...`).
    File,
    /// `name@host.tld` — recognised but not actionable (per user req).
    Email,
}

/// One detected span on a viewport row.  Coordinates are
/// **viewport-local** (0..rows × 0..cols), col_end is **inclusive**.
#[derive(Clone, Debug)]
pub struct LinkRange {
    pub row: u16,
    pub col_start: u16,
    pub col_end: u16,
    pub kind: LinkKind,
    /// The raw text covered by the span — saved here so the click
    /// dispatcher doesn't have to re-scan the grid.  Owned.
    pub text: String,
}

/// Options that tune `scan_visible_links` for the calling pane.  All
/// fields default to off so the existing call path stays opt-in.
#[derive(Default, Clone, Copy, Debug)]
pub struct ScanOpts {
    /// claudecode renders to a fixed inner width and hard-newlines
    /// long URLs / paths with a small hanging indent on the next row.
    /// When set, the line builder detects that pattern (prev row ends
    /// flush with the right edge with a URL/path-class char; this row
    /// starts after ≤ 4 leading spaces with a URL/path-class char)
    /// and treats it as a soft-wrap continuation — the leading indent
    /// is stripped so the URL regex sees one contiguous token.  Other
    /// panes leave this off and the heuristic never fires.
    pub cc_mode: bool,
}

/// Walk the visible grid and return every detected span.  Empty grid
/// → empty Vec.  Allocations: one Vec, one per-row scratch String
/// (reused).
///
/// DECAWM soft-wrap handling: consecutive rows where `wrapped_at_view`
/// flags the lower one as a continuation are joined into one
/// **logical line** before pattern scanning, so a URL or path that
/// overflowed the right edge is matched as a single token instead of
/// being silently truncated at the wrap.  Matches that span multiple
/// physical rows are emitted as separate `LinkRange`s (one per row
/// segment) carrying the SAME `text` — the click dispatcher fires the
/// same action regardless of which segment received the click, and
/// the renderer underlines each segment in place.
///
/// cc-mode hard-wrap merge: when `opts.cc_mode` is set, an additional
/// row-pair heuristic catches claudecode's fixed-width hard newlines
/// (see `ScanOpts::cc_mode`).  Continuation rows have their leading
/// hanging-indent cells stripped from the logical line so the URL /
/// path regex isn't broken by the indent's whitespace.
pub fn scan_visible_links(grid: &Grid, view_offset: u16, opts: ScanOpts) -> Vec<LinkRange> {
    let mut out = Vec::new();
    let rows = grid.rows();
    let cols = grid.cols();
    if rows == 0 || cols == 0 {
        return out;
    }
    // Per-frame allocation hot-path note: `scan_visible_links` is
    // called once per pane per render frame.  An earlier version
    // built a per-line `String` and `Vec<char>::from_iter`'d it
    // inside `scan_line_into_matches` — profiling on a 9-pane setup
    // (samply 15 s while typing) showed ~95 % of the main-thread
    // non-idle time inside `Vec::<char>::from_iter → realloc`
    // chains from that path, manifesting as input latency.  The
    // current shape allocates one `chars` buffer up front and
    // reuses it across every logical-line scan within this call —
    // amortised allocs per frame ≈ 1 vs O(logical-lines × panes).
    //
    // Wide-char (CJK / fullwidth / emoji) trail-half cells carry
    // NUL as a sentinel.  We DROP them entirely from `chars` (the
    // lead cell already contributes the codepoint) — otherwise the
    // terminator scan would hit the trail's NUL-as-space and cut
    // every link the moment it crossed a wide char (e.g. paths like
    // `~/Downloads/決算明細_2025-2026.xlsx`).  A parallel `col_map`
    // tracks each kept char's physical column so `locate()` can
    // still project char_pos → (phys_row, col) for hit-test +
    // multi-row emit.  Empty cells (genuine blanks, never a wide
    // lead's trailer) still push as ' ' — they're terminators.
    // In cc-mode a continuation row may contribute fewer than `cols`
    // chars when its leading hanging-indent is stripped — segment
    // bookkeeping tracks the stripped col count so `emit_match()`
    // still picks the right starting col for multi-row spans.
    let line_cap = cols as usize * rows as usize;
    let mut chars: Vec<char> = Vec::with_capacity(line_cap);
    let mut col_map: Vec<u16> = Vec::with_capacity(line_cap);
    let mut segments: Vec<LineSegment> = Vec::with_capacity(8);
    let mut char_offset: usize = 0;
    for r in 0..rows {
        let decawm_cont = r > 0 && grid.wrapped_at_view(view_offset, r);
        let cc_cont = !decawm_cont
            && r > 0
            && opts.cc_mode
            && is_cc_hard_wrap_continuation(grid, view_offset, r - 1, r, cols);
        let is_continuation = decawm_cont || cc_cont;
        if !is_continuation && !segments.is_empty() {
            scan_logical_line(&chars, &col_map, &segments, cols as usize, &mut out);
            chars.clear();
            col_map.clear();
            segments.clear();
            char_offset = 0;
        }
        let col_skip = if cc_cont {
            count_leading_ws(grid, view_offset, r, cols)
        } else {
            0
        };
        segments.push(LineSegment {
            phys_row: r,
            char_offset,
            col_skip,
            cc_zero_indent: cc_cont && col_skip == 0,
        });
        let mut prev_was_wide = false;
        for c in col_skip..cols {
            let ch = grid.cell_at_view(view_offset, c, r).ch;
            if prev_was_wide && ch == '\0' {
                // Trail half of the previous wide char — already
                // represented by the lead's codepoint; skip.
                prev_was_wide = false;
                continue;
            }
            let pushed = if ch == '\0' || ch == ' ' { ' ' } else { ch };
            chars.push(pushed);
            col_map.push(c);
            prev_was_wide = ch != '\0' && crate::grid::char_width(ch) == 2;
        }
        char_offset = chars.len();
    }
    if !segments.is_empty() {
        scan_logical_line(&chars, &col_map, &segments, cols as usize, &mut out);
    }
    out
}

/// cc hard-wrap heuristic: does `r` look like a continuation of the
/// URL/path that the previous row was rendering?  A false negative
/// leaves the URL split as today.  A false positive is USUALLY
/// harmless (the pattern scan just won't match) — except for
/// filesystem paths, where the glued next-row word breaks the stat
/// check on an otherwise-valid single-row path;
/// `retry_file_at_segment_boundaries` recovers that case at emit
/// time.
///
///   - previous row's last non-blank cell column ≥ cols - 2 (touches
///     or near the right edge — claudecode hard-wraps flush right)
///   - that last non-blank cell's char is URL/path-class
///   - current row has 0..=4 leading whitespace cells; ZERO indent
///     (claudecode's input box char-wraps long tokens mid-word with
///     no hanging indent) additionally requires the previous row to
///     be COMPLETELY full — a mid-word char wrap always occupies the
///     last column
///   - current row's first non-blank cell's char is URL/path-class
fn is_cc_hard_wrap_continuation(
    grid: &Grid,
    view_offset: u16,
    prev_row: u16,
    curr_row: u16,
    cols: u16,
) -> bool {
    if cols < 2 {
        return false;
    }
    let mut last_nb_col: Option<u16> = None;
    let mut last_nb_ch = ' ';
    for c in (0..cols).rev() {
        let ch = grid.cell_at_view(view_offset, c, prev_row).ch;
        if ch != '\0' && ch != ' ' {
            last_nb_col = Some(c);
            last_nb_ch = ch;
            break;
        }
    }
    let last_nb_col = match last_nb_col {
        Some(c) => c,
        None => return false,
    };
    if last_nb_col < cols.saturating_sub(2) {
        return false;
    }
    if !is_url_path_class(last_nb_ch) {
        return false;
    }
    // Current row leading whitespace must be 0..=4 cells, followed
    // by a URL/path-class char.  Zero indent is the weakest signal
    // (flush prose looks the same) — only accept it when the prev
    // row is COMPLETELY full, as a mid-word char wrap must be.
    let mut lead = 0u16;
    while lead < cols {
        let ch = grid.cell_at_view(view_offset, lead, curr_row).ch;
        if ch == ' ' || ch == '\0' {
            lead += 1;
        } else {
            break;
        }
    }
    if lead > 4 {
        return false;
    }
    if lead == 0 && last_nb_col != cols - 1 {
        return false;
    }
    if lead >= cols {
        return false;
    }
    let first = grid.cell_at_view(view_offset, lead, curr_row).ch;
    is_url_path_class(first)
}

/// Char class that we consider "could be part of a URL or path
/// continuation".  Used by the cc hard-wrap heuristic to gate the
/// merge; the actual pattern scan still validates structure.
fn is_url_path_class(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(
            c,
            '/' | '.' | '-' | '_' | '~' | '?' | '&' | '=' | '#' | '%' | ':' | '+' | '@' | ','
        )
}

fn count_leading_ws(grid: &Grid, view_offset: u16, row: u16, cols: u16) -> u16 {
    let mut n = 0u16;
    while n < cols {
        let ch = grid.cell_at_view(view_offset, n, row).ch;
        if ch == ' ' || ch == '\0' {
            n += 1;
        } else {
            break;
        }
    }
    n
}

/// One physical row's contribution to a logical (post-soft-wrap-merge)
/// line.  `phys_row` is the viewport row the chars came from;
/// `char_offset` is where in the merged `chars` buffer this row's
/// pushed chars start.  May be smaller than `cols` when the row
/// contains wide-char trail halves (skipped) or has a stripped
/// cc-mode hanging indent.  Physical column for a given char_pos is
/// recovered via the parallel `col_map` slice, NOT linear arithmetic.
struct LineSegment {
    phys_row: u16,
    char_offset: usize,
    /// Number of leading physical columns that were stripped from
    /// this segment before joining the logical line (cc-mode hanging
    /// indent removal).  Used by `emit_match()` to pick the right
    /// starting col on continuation rows of a multi-row span; 0 for
    /// ordinary rows and DECAWM continuations.
    col_skip: u16,
    /// This segment was joined by the cc heuristic's WEAKEST form —
    /// zero-indent continuation (prev row completely full, this row
    /// starts at col 0).  That shape also matches ordinary flush
    /// prose, so matches without an existence oracle (URL, Email)
    /// are not allowed to cross this boundary; File matches may
    /// (stat + segment-boundary retry arbitrate).
    cc_zero_indent: bool,
}

/// Scan a logical (possibly multi-row-merged) line and emit
/// `LinkRange`s, one per **physical row** the match touches.  Single-
/// segment matches collapse to one LinkRange; multi-segment matches
/// fan out (same `text`, different `phys_row`/col_start/col_end),
/// preserving the per-row hit-test + per-row underline model.
fn scan_logical_line(
    chars: &[char],
    col_map: &[u16],
    segments: &[LineSegment],
    cols_per_row: usize,
    out: &mut Vec<LinkRange>,
) {
    if segments.is_empty() {
        return;
    }
    // Pattern scan emits matches directly into `out` — the previous
    // intermediate `row_matches` Vec was a per-call allocation that
    // showed up as ~250 samples in the input-lag profile (15 s, 9
    // panes typing).  emit_match is the only producer, append-only,
    // so passing `out` straight through is safe and saves the alloc.
    scan_line_into_matches(chars, col_map, out, segments, cols_per_row);
}

/// Char-pos → (phys_row, col) projector.  Linear over the small
/// `segments` slice — O(N segments) per lookup, but in practice
/// N ≤ 4 even for very wrapped URLs.  Physical column comes from
/// `col_map[char_pos]` so wide-char trail halves (skipped) and
/// cc-mode stripped indents don't desync the projection.
fn locate(
    segments: &[LineSegment],
    col_map: &[u16],
    char_pos: usize,
    cols_per_row: usize,
) -> Option<(u16, u16)> {
    let col = *col_map.get(char_pos)? as usize;
    if col >= cols_per_row {
        return None;
    }
    for (i, seg) in segments.iter().enumerate() {
        let next_off = segments
            .get(i + 1)
            .map(|s| s.char_offset)
            .unwrap_or(usize::MAX);
        if char_pos < next_off {
            return Some((seg.phys_row, col as u16));
        }
    }
    None
}

/// Original pattern scanner, but the per-row emit step now consults
/// `segments` to fan a multi-row match out into one LinkRange per
/// physical row it covers.  Each per-row LinkRange carries the FULL
/// matched `text` (so click dispatch + hover tooltip are identical
/// across segments).
fn scan_line_into_matches(
    chars: &[char],
    col_map: &[u16],
    out: &mut Vec<LinkRange>,
    segments: &[LineSegment],
    cols_per_row: usize,
) {
    if segments.is_empty() {
        return;
    }
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let c = chars[i];

        // URL: http:// or https://
        if matches_prefix(&chars, i, "http://") || matches_prefix(&chars, i, "https://") {
            let mut end = scan_until_link_terminator(&chars, i);
            // A zero-indent cc join is the weakest merge guess (flush
            // prose looks identical), and URLs have no existence
            // oracle to arbitrate — never let a URL cross one, or a
            // flush-ending URL absorbs the next row's first word.
            if let Some(b) = first_zero_indent_boundary(segments, i, end) {
                end = b;
                while end > i
                    && matches!(
                        chars[end - 1],
                        ',' | '.' | ';' | ':' | ')' | ']' | '}' | '!' | '?'
                    )
                {
                    end -= 1;
                }
            }
            let span = &chars[i..end];
            if looks_like_url(span) {
                let text: String = span.iter().collect();
                emit_match(out, segments, col_map, cols_per_row, i, end, LinkKind::Url, text);
                i = end;
                continue;
            }
        }

        // A segment start is a text boundary even when the merged
        // buffer glues it to the previous row's last char — without
        // this, a path emitted via the boundary retry leaves the
        // NEXT row's own `/Users/...` with an alnum left neighbour
        // and it would be skipped as a mid-word slash.
        let at_seg_start = segments.iter().any(|s| s.char_offset == i);

        if c == '/' && i + 1 < n && (at_seg_start || !is_left_boundary_alnum(&chars, i)) {
            let end = scan_until_path_terminator(&chars, i);
            let span = &chars[i..end];
            if looks_like_path(span) {
                let text: String = span.iter().collect();
                if is_real_path(&text) {
                    emit_match(out, segments, col_map, cols_per_row, i, end, LinkKind::File, text);
                    i = end;
                    continue;
                }
                // rustc / panic output habitually appends `:line:col`.
                let stripped = strip_line_col_suffix(&chars, i, end);
                if stripped < end {
                    let text: String = chars[i..stripped].iter().collect();
                    if is_real_path(&text) {
                        emit_match(
                            out, segments, col_map, cols_per_row, i, stripped,
                            LinkKind::File, text,
                        );
                        i = end;
                        continue;
                    }
                }
                if let Some(b) = retry_file_at_segment_boundaries(chars, segments, i, end) {
                    let text: String = chars[i..b].iter().collect();
                    emit_match(out, segments, col_map, cols_per_row, i, b, LinkKind::File, text);
                    i = b;
                    continue;
                }
            }
        }

        if c == '~'
            && i + 1 < n
            && chars[i + 1] == '/'
            && (at_seg_start || !is_left_boundary_alnum(&chars, i))
        {
            let end = scan_until_path_terminator(&chars, i);
            let span = &chars[i..end];
            if span.len() >= 3 && looks_like_path(span) {
                let text: String = span.iter().collect();
                if is_real_path(&text) {
                    emit_match(out, segments, col_map, cols_per_row, i, end, LinkKind::File, text);
                    i = end;
                    continue;
                }
                if let Some(b) = retry_file_at_segment_boundaries(chars, segments, i, end) {
                    let text: String = chars[i..b].iter().collect();
                    emit_match(out, segments, col_map, cols_per_row, i, b, LinkKind::File, text);
                    i = b;
                    continue;
                }
            }
        }

        // Bare IPv4 (optionally :port and /path).  Emitted as Url so
        // the click dispatcher's OpenLink arm prepends `http://` and
        // hands off to `/usr/bin/open`; Copy keeps the raw displayed
        // text.  The reject rules avoid catching version strings
        // (`v1.2.3.4`, `pkg 1.2.3.4-alpha`, `1.2.3.4.5`), IPs inside
        // longer identifiers, or IPs already scanned as part of an
        // `http://…` URL (prev char is `/` → reject).
        if c.is_ascii_digit() {
            if let Some(end) = try_scan_ipv4_url(chars, i) {
                let text: String = chars[i..end].iter().collect();
                emit_match(
                    out, segments, col_map, cols_per_row, i, end,
                    LinkKind::Url, text,
                );
                i = end;
                continue;
            }
        }

        if c == '@' && i > 0 && i + 1 < n {
            let local_start = scan_back_local(&chars, i);
            let host_end = scan_forward_host(&chars, i + 1);
            if local_start < i
                && host_end > i + 1
                && is_email_host(&chars[i + 1..host_end])
                // Same weak-guess rule as URLs: an address glued out
                // of two flush prose rows is not an address.
                && first_zero_indent_boundary(segments, local_start, host_end).is_none()
            {
                let text: String = chars[local_start..host_end].iter().collect();
                emit_match(
                    out,
                    segments,
                    col_map,
                    cols_per_row,
                    local_start,
                    host_end,
                    LinkKind::Email,
                    text,
                );
                i = host_end;
                continue;
            }
        }

        i += 1;
    }
}

/// The cc hard-wrap merge is a heuristic guess.  When a merged
/// filesystem-path candidate doesn't stat, the glue may have absorbed
/// the FOLLOWING row's first word: the path ended flush at the right
/// edge (which looks exactly like a claudecode hard wrap) and the
/// next row's opening word happens to be path-class — e.g.
/// `.../feedback.md` + next row `fullpath 已交` merges into
/// `.../feedback.mdfullpath`, and the real, existing single-row path
/// is silently lost.  Retry the prefixes that end exactly at a
/// segment boundary, longest first; the first one that exists on
/// disk is the real token.  Returns the char-pos to truncate at.
/// (URL candidates have no existence oracle, so they keep the plain
/// merged behaviour.)
/// Strip a trailing `:line(:col)?` suffix (compiler / panic output
/// like `/…/file.rs:120:5`) from a path span so the stat check sees
/// the bare path.  Returns the new end; unchanged when no such
/// suffix exists.  At most two `:digits` groups are stripped.
fn strip_line_col_suffix(chars: &[char], start: usize, end: usize) -> usize {
    let mut e = end;
    for _ in 0..2 {
        let mut j = e;
        while j > start && chars[j - 1].is_ascii_digit() {
            j -= 1;
        }
        if j < e && j > start && chars[j - 1] == ':' {
            e = j - 1;
        } else {
            break;
        }
    }
    e
}

/// First zero-indent cc boundary strictly inside `(lo, hi)`, if
/// any.  Segments are ascending, so this returns the earliest one.
fn first_zero_indent_boundary(
    segments: &[LineSegment],
    lo: usize,
    hi: usize,
) -> Option<usize> {
    segments
        .iter()
        .filter(|s| s.cc_zero_indent)
        .map(|s| s.char_offset)
        .find(|&b| b > lo && b < hi)
}

fn retry_file_at_segment_boundaries(
    chars: &[char],
    segments: &[LineSegment],
    lo: usize,
    hi: usize,
) -> Option<usize> {
    for seg in segments.iter().rev() {
        let b = seg.char_offset;
        if b <= lo || b >= hi {
            continue;
        }
        let prefix = &chars[lo..b];
        if !looks_like_path(prefix) {
            continue;
        }
        let text: String = prefix.iter().collect();
        if is_real_path(&text) {
            return Some(b);
        }
    }
    None
}

/// Project one `[char_lo, char_hi)` match onto the physical rows it
/// touches and push one `LinkRange` per row.  Single-row matches turn
/// into one LinkRange (unchanged from the legacy per-row scanner);
/// matches spanning N rows turn into N LinkRanges with the same
/// `text` and matching per-row col spans.
fn emit_match(
    out: &mut Vec<LinkRange>,
    segments: &[LineSegment],
    col_map: &[u16],
    cols_per_row: usize,
    char_lo: usize,
    char_hi_exclusive: usize,
    kind: LinkKind,
    text: String,
) {
    if char_lo >= char_hi_exclusive || segments.is_empty() {
        return;
    }
    let (start_row, start_col) =
        match locate(segments, col_map, char_lo, cols_per_row) {
            Some(v) => v,
            None => return,
        };
    let (end_row, end_col) =
        match locate(segments, col_map, char_hi_exclusive - 1, cols_per_row) {
            Some(v) => v,
            None => return,
        };
    if start_row == end_row {
        out.push(LinkRange {
            row: start_row,
            col_start: start_col,
            col_end: end_col,
            kind,
            text,
        });
        return;
    }
    // Multi-row span: emit one LinkRange per physical row the match
    // covers.  The first row runs from `start_col` to the row's right
    // edge; middle / last rows run from their segment's `col_skip`
    // (= start of contributed cells; 0 for DECAWM continuations, >0
    // for cc-mode hanging-indent strips) to the right edge or
    // `end_col`.  Every LinkRange carries the FULL text so click
    // dispatch is identical regardless of which segment was clicked.
    let last_col = cols_per_row.saturating_sub(1) as u16;
    out.push(LinkRange {
        row: start_row,
        col_start: start_col,
        col_end: last_col,
        kind,
        text: text.clone(),
    });
    for seg in segments.iter().skip(1) {
        if seg.phys_row <= start_row || seg.phys_row >= end_row {
            continue;
        }
        out.push(LinkRange {
            row: seg.phys_row,
            col_start: seg.col_skip,
            col_end: last_col,
            kind,
            text: text.clone(),
        });
    }
    let end_col_skip = segments
        .iter()
        .find(|s| s.phys_row == end_row)
        .map(|s| s.col_skip)
        .unwrap_or(0);
    out.push(LinkRange {
        row: end_row,
        col_start: end_col_skip,
        col_end: end_col,
        kind,
        text,
    });
}

/// Legacy single-row scanner.  Kept for tests + callers that already
/// produce a single-row joined `line`; new code paths go through
/// `scan_line_into_matches` to benefit from soft-wrap merge.
#[cfg(test)]
fn scan_line(line: &str, row: u16, out: &mut Vec<LinkRange>) {
    // Single-segment delegation to the production scanner — the
    // legacy duplicate body drifted from `scan_line_into_matches`
    // (it lacked the boundary retry and the line-col suffix strip),
    // which let tests pass against semantics production didn't have.
    // One scanner, zero drift.
    let chars: Vec<char> = line.chars().collect();
    let col_map: Vec<u16> = (0..chars.len() as u16).collect();
    let cols = chars.len().max(1);
    let segments = [LineSegment {
        phys_row: row,
        char_offset: 0,
        col_skip: 0,
        cc_zero_indent: false,
    }];
    scan_line_into_matches(&chars, &col_map, out, &segments, cols);
}

/// True when `chars[start..]` begins with `prefix`.  All known
/// callers pass ASCII-only literals (`http://`, `https://`), so we
/// compare per byte without first re-collecting `prefix` into a
/// `Vec<char>` — that re-collect was the next hot spot after the
/// per-line `chars: Vec<char>` fix (samply showed ~6 × 10⁶ small
/// allocs/sec from this one function with `prefix.chars().collect()`,
/// realloc churn dominating render).
fn matches_prefix(chars: &[char], start: usize, prefix: &str) -> bool {
    let pbytes = prefix.as_bytes();
    if start + pbytes.len() > chars.len() {
        return false;
    }
    debug_assert!(
        prefix.is_ascii(),
        "matches_prefix fast path assumes ASCII prefix: {prefix:?}"
    );
    for (i, &pb) in pbytes.iter().enumerate() {
        if chars[start + i] as u32 != pb as u32 {
            return false;
        }
    }
    true
}

/// True iff the char immediately before `pos` is alphanumeric.  Used
/// to keep `/` / `~/` mid-word from being mistaken for a path start.
/// Position 0 has no neighbour → treated as a clean boundary.
fn is_left_boundary_alnum(chars: &[char], pos: usize) -> bool {
    if pos == 0 {
        return false;
    }
    chars[pos - 1].is_alphanumeric()
}

/// URL-flavoured terminator scan.  URLs on the wire are pure ASCII
/// — IDN hostnames arrive as `xn--` punycode, UTF-8 path bytes as
/// `%XX` — so ANY non-ASCII char terminates.  The permissive char
/// set inside the ASCII range follows RFC 3986 (unreserved +
/// reserved), minus a small "prose delimiter" blacklist:
///
///   - `<` `>` `"` `'` `` ` `` `|` — quotes / markup / pipes
///
/// Parens are kept inside the scan so Wikipedia's
/// `Rust_(programming_language)` reaches the end intact; the trim
/// stage below then arbitrates balanced vs prose-wrapping parens by
/// counting `(` vs `)` — unbalanced trailing paren = prose, stripped;
/// balanced = URL content, kept.
///
/// The old "everything non-whitespace goes" behaviour swallowed CJK
/// prose that abutted a URL — the 2026-07-15 field report was
/// `…/calendar(刷新一下)。` becoming the URL text, since neither `(`
/// nor CJK chars broke the scan.  The strict char class fixes it at
/// the source: `刷` is non-ASCII → scan stops at `(`, then the trim
/// strips the dangling `(`.
fn scan_until_link_terminator(chars: &[char], start: usize) -> usize {
    let mut i = start;
    while i < chars.len() {
        if !is_url_char(chars[i]) {
            break;
        }
        i += 1;
    }
    // Balanced-paren arbitration + prose punctuation trim.  Repeat
    // until nothing fires — sentences end with `link).`, `link!`,
    // `link.`, etc.
    loop {
        if i <= start {
            break;
        }
        let last = chars[i - 1];
        if last == ')' {
            let (opens, closes) = count_parens(&chars[start..i]);
            if closes > opens {
                i -= 1;
                continue;
            }
            break;
        }
        if last == '(' {
            let (opens, closes) = count_parens(&chars[start..i]);
            if opens > closes {
                i -= 1;
                continue;
            }
            break;
        }
        if matches!(last, ',' | '.' | ';' | ':' | ']' | '}' | '!' | '?') {
            i -= 1;
        } else {
            break;
        }
    }
    i
}

fn count_parens(span: &[char]) -> (usize, usize) {
    let mut opens = 0usize;
    let mut closes = 0usize;
    for &c in span {
        if c == '(' {
            opens += 1;
        } else if c == ')' {
            closes += 1;
        }
    }
    (opens, closes)
}

fn is_url_char(c: char) -> bool {
    if !c.is_ascii() {
        return false;
    }
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '-' | '.' | '_' | '~' | ':' | '/' | '?' | '#' | '[' | ']'
                | '@' | '!' | '$' | '&' | '(' | ')' | '*' | '+'
                | ',' | ';' | '=' | '%'
        )
}

/// Try to parse `chars[start..]` as a bare IPv4 address, optionally
/// followed by `:port` and/or `/path`.  Returns the exclusive end
/// index on success.  Conservative — the goal is to catch things a
/// user could Cmd-click to open in a browser (`47.96.114.231`,
/// `192.168.1.1:8080`, `10.0.0.1/status`) without also underlining
/// version strings.
///
/// Reject rules:
///   - preceded by alnum / `.` / `-` / `_` / `@` / `/` / `:` — that
///     shape is a version, hostname component, or already-matched URL
///     tail, not a fresh IP boundary
///   - any octet > 255 or with a leading zero on a 2+ digit run
///     (`010.1.2.3` is a shell escape / rare curiosity, and the
///     leading-zero-reject also kills `1.02.3.4`-style version noise)
///   - after the 4 octets, next char is `.` / alnum / `-` / `_` — a
///     5th component or an identifier-continuation = version string
///     (`1.2.3.4.5`, `1.2.3.4-rc1`, `1.2.3.4beta`)
fn try_scan_ipv4_url(chars: &[char], start: usize) -> Option<usize> {
    if start > 0 {
        let prev = chars[start - 1];
        if prev.is_ascii_alphanumeric()
            || matches!(prev, '.' | '-' | '_' | '@' | '/' | ':')
        {
            return None;
        }
    }
    let mut i = start;
    for octet in 0..4 {
        if octet > 0 {
            if chars.get(i) != Some(&'.') {
                return None;
            }
            i += 1;
        }
        let d0 = i;
        while i < chars.len() && chars[i].is_ascii_digit() && i - d0 < 3 {
            i += 1;
        }
        let digits = &chars[d0..i];
        if digits.is_empty() {
            return None;
        }
        if digits.len() > 1 && digits[0] == '0' {
            return None;
        }
        let val: u32 = digits
            .iter()
            .map(|c| c.to_digit(10).unwrap())
            .fold(0, |a, d| a * 10 + d);
        if val > 255 {
            return None;
        }
    }
    if let Some(&next) = chars.get(i) {
        if next.is_ascii_alphanumeric() || matches!(next, '.' | '-' | '_') {
            return None;
        }
    }
    let ip_end = i;
    if chars.get(i) == Some(&':') {
        let ps = i + 1;
        let mut pe = ps;
        while pe < chars.len() && chars[pe].is_ascii_digit() && pe - ps < 5 {
            pe += 1;
        }
        if pe > ps {
            let port: u32 = chars[ps..pe]
                .iter()
                .map(|c| c.to_digit(10).unwrap())
                .fold(0, |a, d| a * 10 + d);
            if port <= 65535 {
                i = pe;
            }
        }
    }
    if chars.get(i) == Some(&'/') {
        while i < chars.len() && is_url_char(chars[i]) {
            i += 1;
        }
    }
    // Trim like the URL scanner: balanced parens + prose punctuation.
    // The floor is `ip_end`, not `start` — the bare IPv4 body itself
    // must not be shortened by trim (a trailing `.` inside it was
    // already rejected by the octet regex).
    loop {
        if i <= ip_end {
            break;
        }
        let last = chars[i - 1];
        if last == ')' {
            let (o, c) = count_parens(&chars[start..i]);
            if c > o {
                i -= 1;
                continue;
            }
            break;
        }
        if last == '(' {
            let (o, c) = count_parens(&chars[start..i]);
            if o > c {
                i -= 1;
                continue;
            }
            break;
        }
        if matches!(last, ',' | '.' | ';' | ':' | ']' | '}' | '!' | '?') {
            i -= 1;
        } else {
            break;
        }
    }
    Some(i)
}

/// Path-flavoured terminator scan: additionally hard-stops at `(`,
/// `)`, and the fullwidth CJK punctuation family.  CJK prose
/// habitually glues those straight onto a path (`…visibility.md(Ask
/// 12…`, `…plan.md、`) and a filename CONTAINING them is far rarer
/// than prose abutting them (宁可漏不可错).  CJK ideographs / kana in
/// filenames stay linkable; only punctuation terminates.
fn scan_until_path_terminator(chars: &[char], start: usize) -> usize {
    let mut i = start;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() || c == '\0' || (c.is_control() && c != '\t') {
            break;
        }
        if matches!(c, '<' | '>' | '"' | '\'' | '`' | '|') {
            break;
        }
        if matches!(
            c,
            '(' | ')'
                | '\u{3001}' // 、
                | '\u{3002}' // 。
                | '\u{FF08}' // （
                | '\u{FF09}' // ）
                | '\u{FF0C}' // ，
                | '\u{FF1A}' // ：
                | '\u{FF1B}' // ；
                | '\u{FF01}' // ！
                | '\u{FF1F}' // ？
                | '\u{3008}'..='\u{301B}' // 〈〉《》「」『』【】〔〕〖〗〘〙〚〛
                | '\u{201C}' | '\u{201D}' | '\u{2018}' | '\u{2019}' // 弯引号
                | '\u{2026}' // …
        ) {
            break;
        }
        i += 1;
    }
    while i > start {
        let last = chars[i - 1];
        if matches!(last, ',' | '.' | ';' | ':' | ')' | ']' | '}' | '!' | '?') {
            i -= 1;
        } else {
            break;
        }
    }
    i
}

/// Does this slice look enough like a path that it's WORTH the
/// follow-up `stat()` check?  Cheap structural filter that throws
/// out obvious garbage so we don't spend syscalls on it:
///
///   - too short to be plausible (`/`, `~/`)
///   - contains consecutive slashes (`//`, `//./`, `////`)
///   - root + dot-only components (`/.`, `/..`, `/./`)
///   - no actual letter or digit anywhere (only slashes, dots, dashes)
///
/// Real existence is verified by `is_real_path`; this just avoids
/// asking the filesystem about strings that couldn't possibly be a
/// filename a human typed or a tool printed.
fn looks_like_path(chars: &[char]) -> bool {
    if chars.len() < 2 {
        return false;
    }
    // No consecutive slashes anywhere.
    for w in chars.windows(2) {
        if w[0] == '/' && w[1] == '/' {
            return false;
        }
    }
    // Skip the leading `/` or `~/` prefix.
    let body_start = if chars[0] == '~' { 2 } else { 1 };
    if body_start >= chars.len() {
        return false;
    }
    let body = &chars[body_start..];
    // Must contain at least one alphanumeric char somewhere in the body
    // (rules out `/./`, `/..`, `/-/-/`, etc).
    if !body.iter().any(|c| c.is_alphanumeric()) {
        return false;
    }
    // Reject paths whose every component is only dots (`/././.`).
    let mut all_dots = true;
    for seg in body.split(|c| *c == '/') {
        if !seg.is_empty() && !seg.iter().all(|c| *c == '.') {
            all_dots = false;
            break;
        }
    }
    if all_dots {
        return false;
    }
    true
}

/// Stat the candidate path (with `~/` expanded) and cache the
/// verdict for a short window so per-frame scanning doesn't fire a
/// fresh syscall on every visible row.  Returns true iff the path
/// resolves to an existing filesystem entry (file, directory,
/// symlink target — anything `metadata()` is OK with).
fn is_real_path(path: &str) -> bool {
    thread_local! {
        static CACHE: RefCell<HashMap<String, (Instant, bool)>> = RefCell::new(HashMap::new());
    }
    const TTL: Duration = Duration::from_secs(2);
    const CAP: usize = 256;

    let now = Instant::now();
    let cached = CACHE.with(|c| {
        c.borrow()
            .get(path)
            .and_then(|(t, ok)| {
                if now.duration_since(*t) < TTL {
                    Some(*ok)
                } else {
                    None
                }
            })
    });
    if let Some(ok) = cached {
        return ok;
    }
    let ok = path_exists(path);
    CACHE.with(|c| {
        let mut m = c.borrow_mut();
        if m.len() >= CAP {
            m.clear();
        }
        m.insert(path.to_string(), (now, ok));
    });
    ok
}

fn path_exists(path: &str) -> bool {
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        match std::env::var_os("HOME") {
            Some(home) => {
                let mut p = PathBuf::from(home);
                p.push(rest);
                p
            }
            None => return false,
        }
    } else {
        PathBuf::from(path)
    };
    std::fs::symlink_metadata(&expanded).is_ok()
}

/// Structural URL filter.  The `http://` / `https://` prefix is the
/// caller's gate; this checks the rest.  Rejects:
///
///   - hosts that are empty, `localhost`-shaped (no dot, no digits),
///     or just punctuation (`https://.`, `https://-`)
///   - hosts whose first or last byte is a dot or dash
///   - hosts shorter than 3 chars (`https://a` — too short to point
///     anywhere a user typed on purpose)
fn looks_like_url(span: &[char]) -> bool {
    // Find the scheme separator.
    let after_scheme = if matches_prefix(span, 0, "https://") {
        8
    } else if matches_prefix(span, 0, "http://") {
        7
    } else {
        return false;
    };
    if after_scheme >= span.len() {
        return false;
    }
    // Host runs until the next `/`, `?`, `#`, or end.
    let mut host_end = after_scheme;
    while host_end < span.len() {
        let c = span[host_end];
        if c == '/' || c == '?' || c == '#' {
            break;
        }
        host_end += 1;
    }
    let host = &span[after_scheme..host_end];
    if host.len() < 3 {
        return false;
    }
    let first = host[0];
    let last = host[host.len() - 1];
    if first == '.' || first == '-' || last == '.' || last == '-' {
        return false;
    }
    // Must contain at least one dot in the host (no `localhost` etc;
    // the typical false-positive in chat output is `https://x` style
    // examples that don't actually resolve).
    if !host.iter().any(|c| *c == '.') {
        return false;
    }
    // Host characters must be a sane subset.
    if !host.iter().all(|c| {
        c.is_ascii_alphanumeric() || matches!(*c, '.' | '-' | ':')
    }) {
        return false;
    }
    true
}

/// Walk backwards from an `@` to find the start of the local part.
/// Stops as soon as we hit something that can't be in an email local
/// part (whitespace, punctuation we don't allow, etc.).
fn scan_back_local(chars: &[char], at_pos: usize) -> usize {
    let mut i = at_pos;
    while i > 0 {
        let c = chars[i - 1];
        if is_email_local_char(c) {
            i -= 1;
        } else {
            break;
        }
    }
    i
}

/// Walk forward from after `@` for the host part: letters, digits,
/// `-`, `.`.  Stops at the first foreign char.
fn scan_forward_host(chars: &[char], start: usize) -> usize {
    let mut i = start;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
            i += 1;
        } else {
            break;
        }
    }
    // Trim a trailing dot (common in prose: "ping foo@bar.com.").
    if i > start && chars[i - 1] == '.' {
        i -= 1;
    }
    i
}

/// Structural hostname check for the email scanner, distilled from
/// how mature linkifiers avoid the `pkg@1.0.23` false-positive class
/// (npm/cargo version strings, `tag@sha`, `image@digest`, …):
///
///   - dot-separated labels, ≥ 2 of them
///   - every label non-empty, `[a-z0-9-]`, no leading/trailing `-`
///   - the FINAL label (TLD) is purely ALPHABETIC, length ≥ 2 —
///     this is the discriminating rule; no real TLD is numeric
///
/// GitHub's linkifier and commonmark autolinks apply the same TLD
/// constraint; WezTerm's default `\w+@[\w-]+(\.[\w-]+)+` does not
/// and underlines version strings — the exact trap we hit
/// (2026-07-12: `adapter-maestro@1.0.23` underlined).  IP-literal
/// mail hosts are RFC-legal but never appear in prose worth
/// linking; deliberately left unmatched.
fn is_email_host(chars: &[char]) -> bool {
    if chars.is_empty() {
        return false;
    }
    let mut label_count = 0usize;
    let mut last_label_alpha = false;
    let mut last_label_len = 0usize;
    for label in chars.split(|c| *c == '.') {
        if label.is_empty() {
            return false;
        }
        if label[0] == '-' || label[label.len() - 1] == '-' {
            return false;
        }
        if !label
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == '-')
        {
            return false;
        }
        label_count += 1;
        last_label_alpha = label.iter().all(|c| c.is_ascii_alphabetic());
        last_label_len = label.len();
    }
    label_count >= 2 && last_label_alpha && last_label_len >= 2
}

fn is_email_local_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-' | '%')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(s: &str) -> Vec<LinkRange> {
        let mut out = Vec::new();
        scan_line(s, 0, &mut out);
        out
    }

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn url_https_ok() {
        let v = scan("see https://example.com/path for more");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, LinkKind::Url);
        assert_eq!(v[0].text, "https://example.com/path");
        assert_eq!(v[0].col_start, 4);
    }

    #[test]
    fn url_trims_trailing_period() {
        let v = scan("Check out https://example.com/path.");
        assert_eq!(v[0].text, "https://example.com/path");
    }

    #[test]
    fn url_trims_trailing_paren() {
        let v = scan("link (https://example.com/a)");
        assert_eq!(v[0].text, "https://example.com/a");
    }

    #[test]
    fn url_without_dot_in_host_rejected() {
        // `https://localhost` and friends — typical chat example
        // form that shouldn't underline.
        assert!(!looks_like_url(&chars("https://localhost")));
        assert!(!looks_like_url(&chars("https://x")));
    }

    #[test]
    fn url_dot_or_dash_at_host_edge_rejected() {
        assert!(!looks_like_url(&chars("https://.example.com")));
        assert!(!looks_like_url(&chars("https://example.com.")));
        assert!(!looks_like_url(&chars("https://-example.com")));
    }

    #[test]
    fn url_garbage_after_scheme_rejected() {
        assert!(!looks_like_url(&chars("https://")));
        assert!(!looks_like_url(&chars("https:///path")));
    }

    #[test]
    fn absolute_path_stats_real_file() {
        // The test binary itself is guaranteed to exist on disk.
        let bin = std::env::current_exe().unwrap();
        let line = format!("see {} for details", bin.display());
        let v = scan(&line);
        assert_eq!(v.iter().filter(|r| r.kind == LinkKind::File).count(), 1);
        assert_eq!(v[0].kind, LinkKind::File);
        assert_eq!(v[0].text, bin.display().to_string());
    }

    #[test]
    fn absolute_path_nonexistent_is_rejected() {
        // Pattern matches but stat fails → no link.
        let v = scan("see /Users/x/foo_definitely_not_here.txt for details");
        assert_eq!(v.iter().filter(|r| r.kind == LinkKind::File).count(), 0);
    }

    #[test]
    fn double_slash_rejected_by_pattern() {
        assert!(!looks_like_path(&chars("//")));
        assert!(!looks_like_path(&chars("//./")));
        assert!(!looks_like_path(&chars("////")));
        assert!(!looks_like_path(&chars("/x//y")));
    }

    #[test]
    fn dot_only_components_rejected_by_pattern() {
        assert!(!looks_like_path(&chars("/.")));
        assert!(!looks_like_path(&chars("/./")));
        assert!(!looks_like_path(&chars("/../..")));
    }

    #[test]
    fn mid_word_slash_is_not_a_path() {
        // `cargo/Cargo.toml` shouldn't trip the path detector.
        let v = scan("see cargo/Cargo.toml today");
        assert_eq!(v.iter().filter(|r| r.kind == LinkKind::File).count(), 0);
    }

    #[test]
    fn bare_slash_is_not_a_path() {
        let v = scan("only a /");
        assert_eq!(v.iter().filter(|r| r.kind == LinkKind::File).count(), 0);
    }

    #[test]
    fn email_ok() {
        let v = scan("ping lihao@golia.jp today");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, LinkKind::Email);
        assert_eq!(v[0].text, "lihao@golia.jp");
    }

    #[test]
    fn email_at_alone_is_not_an_email() {
        let v = scan("hey @everyone");
        assert_eq!(v.iter().filter(|r| r.kind == LinkKind::Email).count(), 0);
    }

    #[test]
    fn email_trims_trailing_period() {
        let v = scan("contact foo@bar.com.");
        let emails: Vec<_> = v.iter().filter(|r| r.kind == LinkKind::Email).collect();
        assert_eq!(emails.len(), 1);
        assert_eq!(emails[0].text, "foo@bar.com");
    }

    /// 2026-07-12 regression: `name@version` tokens (npm / cargo /
    /// image digests) must NOT match as email — a real mail domain's
    /// TLD is alphabetic, a version's final label is numeric.
    #[test]
    fn version_strings_are_not_emails() {
        let line = "crates.io smix-cli/adapter-maestro@1.0.23, npm @goliapkg/smix@1.0.23,";
        let v = scan(line);
        assert!(
            v.is_empty(),
            "version strings must not produce any link: {v:?}"
        );
        assert!(scan("docker pull app@sha256.0abc").is_empty());
        assert!(scan("pinned pkg@2.x today").is_empty()); // 1-char TLD
        // Real addresses keep matching.
        assert_eq!(scan("mail takagi@golia.jp now").len(), 1);
        assert_eq!(scan("cc user.name+tag@sub-1.example.co").len(), 1);
    }

    #[test]
    fn email_host_label_rules() {
        assert!(is_email_host(&chars("golia.jp")));
        assert!(is_email_host(&chars("sub-1.example.co")));
        assert!(!is_email_host(&chars("1.0.23"))); // numeric TLD
        assert!(!is_email_host(&chars("example"))); // single label
        assert!(!is_email_host(&chars("bar-.com"))); // label ends with '-'
        assert!(!is_email_host(&chars("-bar.com"))); // label starts with '-'
        assert!(!is_email_host(&chars("bar..com"))); // empty label
        assert!(!is_email_host(&chars("bar.c"))); // 1-char TLD
    }

    #[test]
    fn empty_line_emits_nothing() {
        assert!(scan("").is_empty());
        assert!(scan("                ").is_empty());
    }

    // Phase 1 — soft-wrap-aware visible-grid scanning.  A URL or
    // absolute path that overflowed the right edge into a DECAWM
    // continuation row is matched as a single logical token and
    // emitted as one LinkRange per physical row segment.
    #[test]
    fn url_spanning_soft_wrap_emits_per_row_segments() {
        // 10-col grid; URL is 13 chars so it spans row 0 (cols 0..=9)
        // + row 1 (cols 0..=2).  Test the scanner directly on a
        // pre-built logical line + segment table.
        let line = "https://x.com";
        let segments = vec![
            super::LineSegment {
                phys_row: 0,
                char_offset: 0,
                col_skip: 0,
                cc_zero_indent: false,
            },
            super::LineSegment {
                phys_row: 1,
                char_offset: 10,
                col_skip: 0,
                cc_zero_indent: false,
            },
        ];
        let mut out = Vec::new();
        // col_map: chars 0..9 → row-0 cols 0..9; chars 10..12 → row-1 cols 0..2.
        let col_map: Vec<u16> = (0..10).chain(0..3).collect();
        let chars: Vec<char> = line.chars().collect();
        super::scan_line_into_matches(&chars, &col_map, &mut out, &segments, 10);
        // One match, fanned into 2 LinkRanges (one per physical row).
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].kind, LinkKind::Url);
        assert_eq!(out[0].text, "https://x.com");
        assert_eq!(out[0].row, 0);
        assert_eq!(out[0].col_start, 0);
        assert_eq!(out[0].col_end, 9);
        assert_eq!(out[1].kind, LinkKind::Url);
        assert_eq!(out[1].text, "https://x.com");
        assert_eq!(out[1].row, 1);
        assert_eq!(out[1].col_start, 0);
        assert_eq!(out[1].col_end, 2);
    }

    #[test]
    fn single_row_match_still_emits_one_segment() {
        // No wrap: 1 segment, 1 LinkRange (regression guard for the
        // common case after the multi-row refactor).
        let line = "see https://example.com today      ";
        let segments = vec![super::LineSegment {
            phys_row: 7,
            char_offset: 0,
            col_skip: 0,
            cc_zero_indent: false,
        }];
        let mut out = Vec::new();
        let chars: Vec<char> = line.chars().collect();
        let col_map: Vec<u16> = (0..chars.len() as u16).collect();
        super::scan_line_into_matches(&chars, &col_map, &mut out, &segments, chars.len());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].row, 7);
        assert_eq!(out[0].kind, LinkKind::Url);
        assert_eq!(out[0].text, "https://example.com");
    }

    // End-to-end test through the REAL VT parser: feed a long URL +
    // newline to a Terminal and check that DECAWM-wrap flag really
    // gets set on the continuation row AND scan_visible_links picks
    // it up as a multi-row LinkRange.  This is the production path —
    // if it passes but the user's `echo` still shows broken wrap, the
    // bug is in render-side coords or some upstream layer.
    #[test]
    fn scan_visible_links_via_real_parser() {
        use crate::terminal::Terminal;
        const COLS: u16 = 30;
        const ROWS: u16 = 5;
        let mut t = Terminal::new(COLS, ROWS);
        // 35-char URL — overflows 30-col grid, DECAWM should wrap.
        let url = "https://example.com/abc/d.html";
        // Length is exactly 30 chars — fits in one row, won't trigger
        // wrap.  Bump the length: append a longer path.
        let url = format!("{url}/extra/segments/here");
        t.feed(url.as_bytes());
        // Confirm the parser wrote the URL onto row 0 + row 1 AND set
        // the wrap flag on row 1.
        let grid = t.grid();
        assert!(
            grid.row_wrapped(1),
            "row 1 should be flagged as DECAWM continuation"
        );
        let links = scan_visible_links(grid, 0, super::ScanOpts::default());
        assert!(
            links.len() >= 2,
            "expected ≥2 LinkRanges (URL fanned across rows), got {}: {:?}",
            links.len(),
            links
        );
        // Both LinkRanges should carry the FULL URL.
        for l in &links {
            assert_eq!(l.kind, LinkKind::Url);
            assert_eq!(l.text, url, "all fanned LinkRanges share full URL text");
        }
        // First range on row 0, second on row 1.
        assert_eq!(links[0].row, 0);
        assert_eq!(links[1].row, 1);
    }

    // End-to-end test that mirrors how the production renderer drives
    // scan_visible_links: we build a real Grid, fill DECAWM-wrap-style
    // rows with a long URL filling the right edge + continuation on
    // the next row, set the wrap flag, then call scan_visible_links
    // and assert it emits TWO LinkRanges (top + continuation segment),
    // both carrying the full URL.
    #[test]
    fn scan_visible_links_handles_decawm_wrap() {
        use crate::grid::{Cell, Grid};
        const COLS: u16 = 30;
        const ROWS: u16 = 5;
        let mut grid = Grid::new(COLS, ROWS);
        // 35-char URL: spans cols 0..=29 (30 chars) on row 0 then
        // cols 0..=4 (5 chars) on row 1.  DECAWM would mark row 1
        // as the wrap continuation.
        let url = "https://example.com/path/long.htm";
        let url_chars: Vec<char> = url.chars().collect();
        assert_eq!(url_chars.len(), 33);
        // Row 0: cols 0..=29 = url chars 0..=29 (fills row entirely).
        for (c, &ch) in url_chars.iter().take(COLS as usize).enumerate() {
            grid.set_cell(c as u16, 0, Cell { ch, ..Default::default() });
        }
        // Row 1: cols 0..=2 = url chars 30..=32 (3 chars).  Remaining
        // cols stay default (blank).
        for (c, &ch) in url_chars.iter().skip(COLS as usize).enumerate() {
            grid.set_cell(c as u16, 1, Cell { ch, ..Default::default() });
        }
        // Mark row 1 as the wrap continuation of row 0.
        grid.set_row_wrapped(1, true);
        // Scan at view_offset=0 (live grid, top of viewport).
        let links = scan_visible_links(&grid, 0, super::ScanOpts::default());
        // Expect 2 LinkRanges: row 0 [0..=29] + row 1 [0..=2], both
        // carrying the full URL.
        assert_eq!(
            links.len(),
            2,
            "expected 2 LinkRanges (row 0 + row 1), got {}: {:?}",
            links.len(),
            links
        );
        assert_eq!(links[0].kind, LinkKind::Url);
        assert_eq!(links[0].text, url);
        assert_eq!(links[0].row, 0);
        assert_eq!(links[0].col_start, 0);
        assert_eq!(links[0].col_end, 29);
        assert_eq!(links[1].kind, LinkKind::Url);
        assert_eq!(links[1].text, url);
        assert_eq!(links[1].row, 1);
        assert_eq!(links[1].col_start, 0);
        assert_eq!(links[1].col_end, 2);
    }

    /// End-to-end: feed real bytes through the VT parser and scan
    /// the resulting grid for links.  Mirrors the live L2 path
    /// (PTY → Terminal::feed → mirror grid → scan_visible_links →
    /// Cmd-click hit-test + renderer underline pass), so a fix that
    /// passes the unit test above but fails the parser pipeline gets
    /// caught here.
    /// cc-mode hard-wrap merge: build a grid where a URL is broken
    /// across two rows by a HARD newline (no DECAWM wrap flag set),
    /// with a 2-space hanging indent on the continuation row.  Without
    /// `cc_mode`, the scanner sees row 0's URL fragment + row 1's
    /// "/path..." separately and the regex won't match across.  With
    /// `cc_mode`, the line builder strips the indent and the URL
    /// regex picks up the whole token; the LinkRange fans out across
    /// both rows.
    #[test]
    fn cc_mode_merges_hard_wrap_url_across_indent() {
        use crate::grid::{Cell, Grid};
        const COLS: u16 = 20;
        const ROWS: u16 = 5;
        let mut grid = Grid::new(COLS, ROWS);
        // Row 0 (20 cols): "https://example.com/" — fills entire row,
        // last char at col 19 is '/'.
        let row0 = b"https://example.com/";
        for (c, &b) in row0.iter().enumerate() {
            grid.set_cell(c as u16, 0, Cell { ch: b as char, ..Default::default() });
        }
        // Row 1: "  path/to/file.html" — 2-space hanging indent then
        // the URL continuation.  Note: NO wrap flag set.
        let row1 = b"  path/to/file.html";
        for (c, &b) in row1.iter().enumerate() {
            grid.set_cell(c as u16, 1, Cell { ch: b as char, ..Default::default() });
        }
        // Without cc_mode: row 0's URL terminates at the row edge; the
        // regex doesn't reach row 1.  scan_until_link_terminator only
        // sees row 0's chars + row 1's indent spaces → URL ends at
        // row 0.  But row 1 alone has no http:// so no second link.
        let no_cc = scan_visible_links(&grid, 0, ScanOpts::default());
        assert!(
            no_cc.iter().all(|l| l.row == 0),
            "no cc_mode: link should not span to row 1: {no_cc:?}"
        );

        // With cc_mode: heuristic kicks in (row 0 ends at col 19 with
        // '/', row 1 has 2 leading spaces then 'p' alphanum).  Indent
        // stripped → logical line is "https://example.com/path/to/file.html"
        // → URL regex matches whole token → LinkRange fans out.
        let opts = ScanOpts { cc_mode: true };
        let with_cc = scan_visible_links(&grid, 0, opts);
        assert!(
            with_cc.len() >= 2,
            "cc_mode: expected URL to span ≥2 rows after indent strip, got {with_cc:?}"
        );
        let expected = "https://example.com/path/to/file.html";
        for l in &with_cc {
            assert_eq!(l.kind, LinkKind::Url);
            assert_eq!(l.text, expected, "merged URL text mangled: {l:?}");
        }
        // Row 1's segment must start at col 2 (the indent was stripped
        // from the logical line, but locate() adds col_skip back so
        // the physical click target lines up with the visible chars).
        let row1_seg = with_cc.iter().find(|l| l.row == 1).expect("row 1 segment");
        assert_eq!(
            row1_seg.col_start, 2,
            "row 1 LinkRange must start at col 2 (after hanging indent)"
        );
    }

    /// cc-mode does NOT fire when the prev row's last char isn't
    /// URL/path-class — protects against accidentally merging two
    /// unrelated paragraphs.
    #[test]
    fn cc_mode_does_not_merge_when_prev_row_ends_with_punct() {
        use crate::grid::{Cell, Grid};
        const COLS: u16 = 20;
        const ROWS: u16 = 5;
        let mut grid = Grid::new(COLS, ROWS);
        // Row 0 ends with a period (sentence end), not URL/path char.
        let row0 = b"finished the request.";
        for (c, &b) in row0.iter().take(COLS as usize).enumerate() {
            grid.set_cell(c as u16, 0, Cell { ch: b as char, ..Default::default() });
        }
        // Row 1 has indent + URL-looking content.
        let row1 = b"  /some/path.rs";
        for (c, &b) in row1.iter().enumerate() {
            grid.set_cell(c as u16, 1, Cell { ch: b as char, ..Default::default() });
        }
        let with_cc = scan_visible_links(&grid, 0, ScanOpts { cc_mode: true });
        // Heuristic must reject the pair → no merge → row 0 + row 1
        // are scanned independently; path on row 1 has 2-space indent
        // before it but the path itself starts at col 2 — that's fine
        // (its own row's scan will pick it up if it stat()s; here we
        // just assert no merge happened by checking no LinkRange
        // carries text containing "request").
        for l in &with_cc {
            assert!(
                !l.text.contains("finished"),
                "cc_mode wrongly merged a sentence into the next row: {l:?}"
            );
        }
    }

    #[test]
    fn e2e_scan_links_via_parser_finds_url_across_soft_wrap() {
        use crate::terminal::Terminal;
        let cols = 20u16;
        let rows = 5u16;
        let mut t = Terminal::new(cols, rows);
        let url = "https://example.com/some/very/long/path/that/wraps?q=value";
        t.feed(url.as_bytes());
        let grid = t.grid();
        let links = scan_visible_links(grid, 0, super::ScanOpts::default());
        assert!(!links.is_empty(), "no LinkRange emitted for {url:?}");
        for link in &links {
            assert_eq!(link.kind, LinkKind::Url);
            assert_eq!(
                link.text, url,
                "LinkRange text mangled by wrap merge: {:?}",
                link.text
            );
        }
        // The URL is longer than `cols`, so we should see >= 2 rows.
        assert!(
            links.len() >= 2,
            "expected multi-row LinkRange (cols=20, url len {}), got {} segments",
            url.chars().count(),
            links.len()
        );
    }

    /// Wide-char (CJK) trail-halves used to push as ' ' into the
    /// scan buffer, causing `scan_until_link_terminator` to cut a
    /// path on the first CJK boundary.  Regression guard: feed a
    /// path containing CJK chars through the real parser, stat
    /// it via a tempfile so `is_real_path` accepts it, and assert
    /// the FULL path is one contiguous LinkRange covering both the
    /// lead and trail cells of every wide char.
    #[test]
    fn cjk_path_detected_across_wide_char_cells() {
        use crate::terminal::Terminal;
        let dir = std::env::temp_dir().join("marspot-link-cjk-test");
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("決算明細_2025-2026.txt");
        std::fs::write(&path, b"x").expect("write tempfile");
        let path_str = path.to_string_lossy().into_owned();
        // Wide enough to keep the whole path on one row; the input
        // is ASCII `/private/...`-style, no soft-wrap concerns.
        let cols: u16 = (path_str.chars().count() as u16) + 10;
        let mut t = Terminal::new(cols, 3);
        t.feed(format!("see {} for", path_str).as_bytes());
        let grid = t.grid();
        let links = scan_visible_links(grid, 0, super::ScanOpts::default());
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            links.len(),
            1,
            "expected exactly one File LinkRange for the CJK path, got {links:?}"
        );
        let link = &links[0];
        assert_eq!(link.kind, LinkKind::File);
        assert_eq!(
            link.text, path_str,
            "LinkRange text truncated at a CJK boundary"
        );
        // col_start = col of '/'; col_end = col of last 't' in `.txt`.
        // The grid stores each wide char as lead+trail, so col_end -
        // col_start + 1 = total physical cells covered = path length
        // in chars + count of wide chars (each contributes one
        // extra cell vs `.chars().count()`).
        let wide_count = path_str
            .chars()
            .filter(|c| crate::grid::char_width(*c) == 2)
            .count() as u16;
        let expected_span = path_str.chars().count() as u16 + wide_count;
        assert_eq!(
            (link.col_end - link.col_start + 1),
            expected_span,
            "underline span ({}..={}) doesn't cover lead+trail of each wide char (expected {} cells)",
            link.col_start,
            link.col_end,
            expected_span
        );
    }
}

#[cfg(test)]
mod tilde_cjk_tests {
    use super::*;
    use crate::terminal::Terminal;

    /// The `~/` + CJK combination from the 2026-07-03 field report
    /// (`~/Downloads/GOLIA-代表取缔役印.png` not underlined): the `~`
    /// branch + wide-char trail-half handling + `is_real_path`'s
    /// HOME expansion must compose.  The sibling CJK test uses an
    /// absolute path, so it never exercises the tilde branch.
    ///
    /// `set_var("HOME")` is safe under nextest (process per test);
    /// under plain multi-threaded `cargo test` it could race other
    /// tests reading HOME — the project runner is nextest
    /// (bin/test.sh).
    #[test]
    fn tilde_cjk_path_detected() {
        let home = std::env::temp_dir().join("marspot-link-tilde-cjk");
        std::fs::create_dir_all(home.join("Downloads")).unwrap();
        std::fs::write(home.join("Downloads/GOLIA-代表取缔役印.png"), b"x").unwrap();
        // SAFETY: test/example code, single-threaded at this point (state-dir
        // mutations additionally serialized by the suite's state-dir lock).
        unsafe { std::env::set_var("HOME", &home) };
        let s = "~/Downloads/GOLIA-代表取缔役印.png";
        let cols = (s.chars().count() as u16) * 2 + 20;
        let mut t = Terminal::new(cols, 3);
        t.feed(format!("see {s} ok").as_bytes());
        let links = scan_visible_links(t.grid(), 0, ScanOpts::default());
        assert_eq!(links.len(), 1, "got {links:?}");
        assert_eq!(links[0].text, s);
        assert_eq!(links[0].kind, LinkKind::File);
    }
}

#[cfg(test)]
mod cc_merge_false_positive {
    use super::*;
    use crate::grid::{Cell, Grid};

    /// 2026-07-12 second report: three shift+enter-separated real
    /// paths in claudecode's input box; #2 and #3 char-wrap mid-word
    /// at the right edge with ZERO hanging indent ("…roun" / "d-2.md")
    /// — the old 1..=4-indent requirement never merged them, so only
    /// path #1 got a link.  Zero-indent merge (gated on a completely
    /// full prev row) must recover all three; the flush row0/row1
    /// junction also exercises the glue-then-retry path (path #1 ends
    /// flush and path #2 starts at col 0 → merged, stat fails, retry
    /// splits at the boundary).
    #[test]
    fn zero_indent_char_wrap_paths_all_link() {
        use crate::grid::{Cell, Grid};
        let dir = std::env::temp_dir().join(format!(
            "marspot-lnk0-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |name: &str| {
            let p = dir.join(name);
            std::fs::write(&p, b"x").unwrap();
            p.to_string_lossy().into_owned()
        };
        let p1 = mk("smix-feedback-2026-07-12.md");
        let p2 = mk("smix-feedback-2026-07-12-round-2.md");
        let p3 = mk("qa-sim-behavior-verification-plan.md");
        let cols = p1.chars().count() as u16; // p1 exactly fills row 0

        let mut grid = Grid::new(cols, 8);
        let mut put = |row: u16, text: &str| {
            for (c, ch) in text.chars().enumerate() {
                grid.set_cell(c as u16, row, Cell { ch, ..Default::default() });
            }
        };
        let (a2, b2) = p2.split_at(
            p2.char_indices().nth(cols as usize).map(|(i, _)| i).unwrap(),
        );
        let (a3, b3) = p3.split_at(
            p3.char_indices().nth(cols as usize).map(|(i, _)| i).unwrap(),
        );
        put(0, &p1);
        put(1, a2);
        put(2, b2);
        put(3, a3);
        put(4, b3);

        let links = scan_visible_links(&grid, 0, ScanOpts { cc_mode: true });
        for f in [&p1, &p2, &p3] {
            let hits: Vec<_> = links
                .iter()
                .filter(|l| l.kind == LinkKind::File && l.text == **f)
                .collect();
            assert!(
                !hits.is_empty(),
                "path {f} must be detected; got {links:?}"
            );
        }
        let texts: std::collections::HashSet<_> =
            links.iter().map(|l| l.text.clone()).collect();
        assert_eq!(texts.len(), 3, "exactly the three paths: {links:?}");
        for p in [p1, p2, p3] {
            let _ = std::fs::remove_file(dir.join(
                std::path::Path::new(&p).file_name().unwrap(),
            ));
        }
        let _ = std::fs::remove_dir(&dir);
    }

    /// 2026-07-13 report: a wrapped path immediately followed by
    /// `(Ask 12:…` prose lost its link — `(` wasn't a terminator, so
    /// the token became `….md(Ask`, stat failed, and the boundary
    /// retry only had the half-path prefix to offer.  Paths must
    /// hard-stop at `(` and CJK fullwidth punctuation.
    #[test]
    fn path_terminates_at_paren_and_cjk_punct() {
        // Single-row cases through the scan_line path.
        let dir = std::env::temp_dir().join(format!(
            "marspot-lnkp-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("plan.md");
        std::fs::write(&p, b"x").unwrap();
        let ps = p.to_string_lossy().into_owned();
        for suffix in ["(Ask 12", "、后续", "。end", "（全角）", ":120:5"] {
            // ":120:5" exercises the line-col suffix strip; the rest
            // exercise the new hard terminators.
            let line = format!("看 {}{} 即可", ps, suffix);
            let mut out = Vec::new();
            scan_line(&line, 0, &mut out);
            let files: Vec<_> =
                out.iter().filter(|l| l.kind == LinkKind::File).collect();
            assert_eq!(files.len(), 1, "suffix {suffix:?}: {out:?}");
            assert_eq!(files[0].text, ps, "suffix {suffix:?}");
        }
        // `:`+prose glued onto a path is NOT a recognised shape —
        // stays unlinked rather than guessing (宁可漏).
        let mut out = Vec::new();
        scan_line(&format!("看 {}:note 即可", ps), 0, &mut out);
        assert!(out.iter().all(|l| l.kind != LinkKind::File), "{out:?}");
        // Wrapped zero-indent + glued paren — the screenshot shape.
        use crate::grid::{Cell, Grid};
        let cols = ps.chars().count() as u16 - 10;
        let mut grid = Grid::new(cols, 4);
        let split_byte = ps
            .char_indices()
            .nth(cols as usize)
            .map(|(i, _)| i)
            .unwrap();
        let (a, b) = ps.split_at(split_byte);
        for (c, ch) in a.chars().enumerate() {
            grid.set_cell(c as u16, 0, Cell { ch, ..Default::default() });
        }
        for (c, ch) in format!("{}(Ask 12", b).chars().enumerate() {
            grid.set_cell(c as u16, 1, Cell { ch, ..Default::default() });
        }
        let links = scan_visible_links(&grid, 0, ScanOpts { cc_mode: true });
        let files: Vec<_> =
            links.iter().filter(|l| l.kind == LinkKind::File).collect();
        assert!(
            files.iter().any(|l| l.text == ps),
            "wrapped path + glued paren must link: {links:?}"
        );
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir(&dir);
    }

    /// URLs keep the permissive terminator set — parens are legal
    /// inside them (Wikipedia article URLs) and balanced pairs must
    /// survive the trim.
    #[test]
    fn url_keeps_parens_inside() {
        let mut v = Vec::new();
        scan_line(
            "see https://en.wikipedia.org/wiki/Rust_(programming_language) ok",
            0,
            &mut v,
        );
        assert_eq!(v.len(), 1);
        assert_eq!(
            v[0].text,
            "https://en.wikipedia.org/wiki/Rust_(programming_language)"
        );
    }

    /// 2026-07-15 field report: `https://devops.golia.jp/calendar(刷新一下)。`
    /// was underlined as the URL text — `(` and CJK chars weren't
    /// terminators.  Strict ASCII URL char class + balanced-paren trim
    /// cut it back to the real URL.
    #[test]
    fn url_terminates_at_cjk_and_strips_dangling_paren() {
        let mut v = Vec::new();
        scan_line("日历 - https://devops.golia.jp/calendar(刷新一下)。", 0, &mut v);
        let urls: Vec<_> = v.iter().filter(|l| l.kind == LinkKind::Url).collect();
        assert_eq!(urls.len(), 1, "{v:?}");
        assert_eq!(urls[0].text, "https://devops.golia.jp/calendar");
    }

    /// URL directly abutting Japanese prose (no `(`) — same fix, no
    /// dangling ASCII to arbitrate; just non-ASCII terminates.
    #[test]
    fn url_terminates_at_bare_cjk() {
        let mut v = Vec::new();
        scan_line("参考 https://example.com/foo は使えます", 0, &mut v);
        let urls: Vec<_> = v.iter().filter(|l| l.kind == LinkKind::Url).collect();
        assert_eq!(urls.len(), 1, "{v:?}");
        assert_eq!(urls[0].text, "https://example.com/foo");
    }

    /// Prose-wrapping parens still get stripped (long-standing case).
    #[test]
    fn url_in_prose_parens_still_trimmed() {
        let mut v = Vec::new();
        scan_line("(see https://example.com/x)", 0, &mut v);
        let urls: Vec<_> = v.iter().filter(|l| l.kind == LinkKind::Url).collect();
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].text, "https://example.com/x");
    }

    /// 2026-07-16 field ask: bare IPv4 must be recognised as a
    /// clickable Url — OpenLink prepends `http://` in the dispatcher,
    /// Copy keeps the raw displayed text.  Cover the common shapes:
    /// bare, `:port`, `/path`, both.
    #[test]
    fn bare_ipv4_recognised_as_url() {
        for (line, expected) in [
            ("connect 47.96.114.231 now", "47.96.114.231"),
            ("admin 192.168.1.1:8080/status ok", "192.168.1.1:8080/status"),
            ("dashboard 10.0.0.5:3000", "10.0.0.5:3000"),
            ("see 8.8.8.8/dns page", "8.8.8.8/dns"),
        ] {
            let mut v = Vec::new();
            scan_line(line, 0, &mut v);
            let urls: Vec<_> = v.iter().filter(|l| l.kind == LinkKind::Url).collect();
            assert_eq!(urls.len(), 1, "line {line:?}: {v:?}");
            assert_eq!(urls[0].text, expected, "line {line:?}");
        }
    }

    /// IPv4 recogniser must NOT swallow version strings, other
    /// identifiers, or IPs already claimed by an `http://…` URL.
    #[test]
    fn ipv4_rejects_version_strings_and_url_tails() {
        for line in [
            "runtime v1.2.3.4 released",       // preceded by 'v'
            "kernel 1.2.3.4-rc1 in test",      // trailing -rc1
            "matrix 1.2.3.4.5 dot",            // 5th component
            "big 999.1.1.1 not valid",         // octet > 255
            "cargo pkg 1.0.23 pinned",         // not 4 octets
            "leading 010.0.0.1 zero rejected", // leading zero
            "http://1.2.3.4/foo done",         // already inside a URL
        ] {
            let mut v = Vec::new();
            scan_line(line, 0, &mut v);
            let ips: Vec<_> = v
                .iter()
                .filter(|l| l.kind == LinkKind::Url && !l.text.starts_with("http"))
                .collect();
            assert!(
                ips.is_empty(),
                "line {line:?} must not produce a bare-IP Url: {v:?}"
            );
        }
    }

    /// Prose-wrapping parens around the IP get stripped by the same
    /// balanced-paren trim as URLs.
    #[test]
    fn ipv4_in_prose_parens_trimmed() {
        let mut v = Vec::new();
        scan_line("(see 47.96.114.231:8080)", 0, &mut v);
        let urls: Vec<_> = v.iter().filter(|l| l.kind == LinkKind::Url).collect();
        assert_eq!(urls.len(), 1, "{v:?}");
        assert_eq!(urls[0].text, "47.96.114.231:8080");
    }

    /// The zero-indent merge must NOT let a flush-ending URL absorb
    /// the next prose row (URLs have no existence oracle).
    #[test]
    fn zero_indent_does_not_glue_urls() {
        use crate::grid::{Cell, Grid};
        const COLS: u16 = 30;
        let url = format!("https://example.com/{}", "a".repeat(10)); // 30 chars
        assert_eq!(url.chars().count(), COLS as usize);
        let mut grid = Grid::new(COLS, 4);
        for (c, ch) in url.chars().enumerate() {
            grid.set_cell(c as u16, 0, Cell { ch, ..Default::default() });
        }
        for (c, ch) in "and more prose".chars().enumerate() {
            grid.set_cell(c as u16, 1, Cell { ch, ..Default::default() });
        }
        let links = scan_visible_links(&grid, 0, ScanOpts { cc_mode: true });
        assert_eq!(links.len(), 1, "{links:?}");
        assert_eq!(links[0].kind, LinkKind::Url);
        assert_eq!(links[0].text, url, "URL must stop at the row edge");
        assert_eq!(links[0].row, 0);
    }

    /// 2026-07-12 regression: a real path that ends flush at the
    /// right edge, followed by a prose row starting with a
    /// path-class word ("fullpath 已交…"), tripped the cc hard-wrap
    /// heuristic — the merge produced `…feedback.mdfullpath`, the
    /// stat failed, and the perfectly valid single-row path lost its
    /// link.  The emit-time segment-boundary retry must recover it.
    #[test]
    fn flush_right_path_followed_by_prose_still_links() {
        // A REAL file; the grid is sized so the path exactly fills
        // row 0 (flush right = what trips the cc merge heuristic).
        let dir = std::env::temp_dir().join(format!(
            "marspot-lnk-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("feedback.md");
        std::fs::write(&path, b"x").unwrap();
        let path_s = path.to_string_lossy().into_owned();
        let cols = path_s.chars().count() as u16;

        let mut grid = Grid::new(cols, 4);
        for (c, ch) in path_s.chars().enumerate() {
            grid.set_cell(c as u16, 0, Cell { ch, ..Default::default() });
        }
        // Continuation-looking prose row: 1-space indent + alnum word.
        for (c, ch) in " fullpath done".chars().enumerate() {
            grid.set_cell(c as u16, 1, Cell { ch, ..Default::default() });
        }

        let links = scan_visible_links(&grid, 0, ScanOpts { cc_mode: true });
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
        assert_eq!(
            links.len(),
            1,
            "flush-right real path must survive the cc merge: {links:?}"
        );
        assert_eq!(links[0].kind, LinkKind::File);
        assert_eq!(links[0].text, path_s);
        assert_eq!(links[0].row, 0);
    }
}

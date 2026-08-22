//! marspot-linkify — clickable-span detection over a terminal cell
//! surface (stone tier; see the crate manifest for scope).
//!
//! Input comes exclusively through [`CellSource`]; output is
//! [`LinkRange`]s in viewport coordinates.  Detection is
//! deliberately conservative — false negatives beat false positives
//! (a wrongly-underlined `cargo/Cargo.toml` is a daily annoyance; a
//! missed link costs one manual copy).  For `File` spans a `stat()`
//! (with a small TTL cache) arbitrates; URLs are structurally
//! validated instead.

use std::path::PathBuf;

/// The cell surface a scan reads.  Coordinates are viewport-local
/// (`0..cols() × 0..rows()`).  Implementors decide what a "row" is —
/// marspot hands in its grid at a scrollback view offset; a test
/// hands in a `Vec` of strings.
pub trait CellSource {
    fn cols(&self) -> u16;
    fn rows(&self) -> u16;
    /// Character at (col, row).  `'\0'` marks both genuinely empty
    /// cells and the trailing filler cell of a wide (2-column)
    /// character; [`CellSource::is_wide`] on the PRECEDING char
    /// disambiguates.
    fn char_at(&self, col: u16, row: u16) -> char;
    /// Is `row` a soft-wrap (DECAWM) continuation of the row above?
    fn is_soft_wrap_continuation(&self, row: u16) -> bool;
    /// Cursor position `(col, row)` — anchors the input-box
    /// exemption (a box containing the caret is an active composer).
    fn cursor(&self) -> (u16, u16);
    /// Does `ch` occupy two columns on this surface?
    fn is_wide(&self, ch: char) -> bool;
}

/// Five flavours the scanner recognises.  Render attaches a colour
/// per kind; the click handler dispatches per kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkKind {
    /// `http://` or `https://` URL.
    Url,
    /// Absolute path (`/...`) or home-relative path (`~/...`).
    File,
    /// `name@host.tld` — Cmd-click sends `mailto:` to `open(1)`.
    Email,
    /// Bare IPv4 or IPv6 (with optional `:port` / `/path` for IPv4,
    /// or bracketed form for IPv6).  Cmd-click prepends `http://`
    /// (defaults to port 80).  Primary intent is copy-friendly
    /// identification — a URL with `http://` prefix always wins over
    /// this because the URL scanner runs first.
    Ip,
    /// Canonical UUID `8-4-4-4-12` hex.  Copy-only in the menu — a
    /// UUID isn't openable, but it's the token you most often want
    /// off a log line.
    Uuid,
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
    /// Fixed-width TUIs (claude code and friends) render to a fixed
    /// inner width and hard-newline long URLs / paths with a small
    /// hanging indent on the next row.  When set, the line builder
    /// detects that pattern (prev row ends at/near the right edge
    /// with a URL/path-class char; this row starts after ≤ 4 leading
    /// spaces with a URL/path-class char) and treats it as a
    /// soft-wrap continuation — the leading indent is stripped so
    /// the pattern scan sees one contiguous token.  Also enables the
    /// input-box exemption (rows inside the bottom-most rounded box
    /// that looks like an active composer are not scanned).
    pub tui_mode: bool,
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
/// cc-mode hard-wrap merge: when `opts.tui_mode` is set, an additional
/// row-pair heuristic catches claudecode's fixed-width hard newlines
/// (see `ScanOpts::cc_mode`).  Continuation rows have their leading
/// hanging-indent cells stripped from the logical line so the URL /
/// path regex isn't broken by the indent's whitespace.
///
/// cc-mode input-box exemption: also when `opts.tui_mode` is set, rows
/// belonging to claudecode's bottom-most rounded box (`╭…╮` / `╰…╯`,
/// inclusive) are excluded from scanning entirely.  Mid-typing text
/// in the composer shouldn't flash underlined as the user types a
/// partial URL / path, and right-clicks in the composer shouldn't
/// hit a link menu.  The exemption is structural (based on grid
/// content), so it's inert on non-claudecode grids.
pub fn scan_visible_links<S: CellSource>(src: &S, opts: ScanOpts) -> Vec<LinkRange> {
    scan_visible_links_with(src, opts, &FsOracle)
}

/// [`scan_visible_links`] with an explicit path oracle.  The render
/// loop uses this with an async, cached oracle so the scan never
/// blocks on the filesystem; everything else can keep the blocking
/// default.
pub fn scan_visible_links_with<S: CellSource>(
    src: &S,
    opts: ScanOpts,
    oracle: &dyn PathOracle,
) -> Vec<LinkRange> {
    let mut out = Vec::new();
    let rows = src.rows();
    let cols = src.cols();
    if rows == 0 || cols == 0 {
        return out;
    }
    let exempt_range: Option<(u16, u16)> = if opts.tui_mode {
        find_input_box_rows(src, rows, cols)
    } else {
        None
    };
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
    // Vertical rules seen while walking, for the table-cell pass
    // below.  Collected here rather than in a sweep of its own: this
    // loop already reads every cell once, and a second full pass over
    // the grid per pane per frame is exactly the kind of cost this
    // scanner has been profiled to avoid.  Empty on prose.
    let mut rule_cols: Vec<(u16, u16)> = Vec::new();
    for r in 0..rows {
        // Rows inside the claudecode composer box are hard-skipped:
        // flush any in-flight logical line above, then jump past this
        // row without contributing any chars.  The `chars`/`col_map`/
        // `segments` buffers are cleared so the row after the box
        // starts a fresh line, not a phantom continuation.
        if let Some((lo, hi)) = exempt_range {
            if r >= lo && r <= hi {
                if !segments.is_empty() {
                    scan_logical_line(&chars, &col_map, &segments, cols as usize, &mut out, oracle);
                    chars.clear();
                    col_map.clear();
                    segments.clear();
                    char_offset = 0;
                }
                continue;
            }
        }
        let decawm_cont = r > 0 && src.is_soft_wrap_continuation(r);
        let cc_cont = !decawm_cont
            && r > 0
            && opts.tui_mode
            && is_hard_wrap_continuation(src, r - 1, r, cols);
        let is_continuation = decawm_cont || cc_cont;
        if !is_continuation && !segments.is_empty() {
            scan_logical_line(&chars, &col_map, &segments, cols as usize, &mut out, oracle);
            chars.clear();
            col_map.clear();
            segments.clear();
            char_offset = 0;
        }
        // A cc hard-wrap continuation glues to the PREVIOUS row's
        // last content cell — the trailing blank cells between that
        // cell and the pane edge are rendering padding, not text.
        // Left in the buffer they terminate the merged token at the
        // seam (the whole point of merging was to cross it).
        if cc_cont {
            while chars.last() == Some(&' ') {
                chars.pop();
                col_map.pop();
            }
            char_offset = chars.len();
        }
        let col_skip = if cc_cont {
            count_leading_ws(src, r, cols)
        } else {
            0
        };
        segments.push(LineSegment {
            phys_row: r,
            char_offset,
            cc_zero_indent: cc_cont && col_skip == 0,
        });
        let mut prev_was_wide = false;
        for c in col_skip..cols {
            let ch = src.char_at(c, r);
            if prev_was_wide && ch == '\0' {
                // Trail half of the previous wide char — already
                // represented by the lead's codepoint; skip.
                prev_was_wide = false;
                continue;
            }
            let pushed = if ch == '\0' || ch == ' ' { ' ' } else { ch };
            if opts.tui_mode && is_vertical_rule(pushed) {
                rule_cols.push((r, c));
            }
            chars.push(pushed);
            col_map.push(c);
            prev_was_wide = ch != '\0' && src.is_wide(ch);
        }
        char_offset = chars.len();
    }
    if !segments.is_empty() {
        scan_logical_line(&chars, &col_map, &segments, cols as usize, &mut out, oracle);
    }
    if opts.tui_mode && rule_cols.len() >= 4 {
        let before = out.len();
        scan_table_cells(src, cols, &rule_cols, &mut out, oracle);
        // Nothing crossed a cell boundary — leave the row pass's
        // output exactly as it was, sort and all.
        if out.len() != before {
            drop_shadowed_ranges(&mut out);
        }
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
fn is_hard_wrap_continuation<S: CellSource>(
    src: &S,
    prev_row: u16,
    curr_row: u16,
    cols: u16,
) -> bool {
    // The whole row is the band: content spans 0..cols.
    is_hard_wrap_continuation_in(src, prev_row, curr_row, 0, cols)
}

/// The same heuristic over an arbitrary column band `left..right`.
///
/// A table cell is a band whose edges are the cell's borders rather
/// than the pane's, and everything the heuristic asks — "did the
/// previous row run out of room?", "does this row take up where it
/// left off?" — is asked of those edges instead.  The full-row form
/// above is the `0..cols` case of exactly this.
fn is_hard_wrap_continuation_in<S: CellSource>(
    src: &S,
    prev_row: u16,
    curr_row: u16,
    left: u16,
    right: u16,
) -> bool {
    if right < left + 2 {
        return false;
    }
    let cols = right;
    let mut last_nb_col: Option<u16> = None;
    let mut last_nb_ch = ' ';
    for c in (left..right).rev() {
        let ch = src.char_at(c, prev_row);
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
    // Flush requirement: within 2 cols of the right edge — except a
    // `/`-ending row, which relaxes to 8 cols.  claudecode's `⎿ `
    // indent blocks wrap paths at a fixed CONTENT width a few cells
    // short of the pane edge, so a wrapped path's first row never
    // passed the strict test and the path stayed split (2026-07-18
    // field report).  A prose row ending in `/` is rare enough that
    // the wider window stays conservative; File candidates keep the
    // stat + boundary-retry arbitration either way.
    // Does the row end in the middle of a *path*?  Walk back over the
    // trailing url/path-class run and look for a separator: a token
    // carrying `/` that runs to the end of the row is a path the
    // renderer cut, not a word that happened to finish there.
    //
    // This is the same observation the `/`-ending case above was
    // built on, generalised.  That one only caught a break landing
    // exactly on a separator; a break one character later — inside
    // `…-e18-ve` / `rdict.md` — fell back to 2 cells of slack, missed
    // by one, and the link stopped at the last directory that
    // happened to exist on disk (2026-08-16 field report).  Which
    // character the wrap lands on is not something the path controls.
    let trailing_is_path = {
        let mut c = last_nb_col;
        let mut has_sep = false;
        loop {
            let ch = src.char_at(c, prev_row);
            if !is_url_path_class(ch) {
                break;
            }
            if ch == '/' {
                has_sep = true;
                break;
            }
            if c == left {
                break;
            }
            c -= 1;
        }
        has_sep
    };
    let flush_slack: u16 = if last_nb_ch == '/' || trailing_is_path { 8 } else { 2 };
    if last_nb_col < cols.saturating_sub(flush_slack) {
        return false;
    }
    if !is_url_path_class(last_nb_ch) {
        return false;
    }
    // Current row leading whitespace must be 0..=4 cells, followed
    // by a URL/path-class char.  Zero indent is the weakest signal
    // (flush prose looks the same) — only accept it when the prev
    // row is COMPLETELY full, as a mid-word char wrap must be.
    let mut lead = left;
    while lead < right {
        let ch = src.char_at(lead, curr_row);
        if ch == ' ' || ch == '\0' {
            lead += 1;
        } else {
            break;
        }
    }
    if lead - left > 4 {
        return false;
    }
    if lead == left && last_nb_col != right - 1 {
        return false;
    }
    if lead >= right {
        return false;
    }
    // A wrap cuts a token in half, so the row below opens with the
    // REST of that token.  A lone `-` / `*` / `1.` followed by a
    // blank is not a token tail — it is a list marker the renderer
    // put there.  Geometry cannot see the difference (a nested list
    // item carries the same 1..=4 cell indent a hanging wrap does,
    // and the item above can end flush at the edge on a path char),
    // and joining a marker can only ever bolt one punctuation char
    // onto the row above: the 2026-08-18 field report was a bullet's
    // URL coming out as `…/village/index-`, having swallowed the `-`
    // that opened the NEXT bullet.
    if starts_list_marker(src, curr_row, lead, right) {
        return false;
    }
    let first = src.char_at(lead, curr_row);
    is_url_path_class(first)
}

/// Does `row` open a list item at `from` — a bullet (`-`, `*`, `+`,
/// `>`, …) or an ordered marker (`1.`, `2)`) standing alone before a
/// blank?  Anything longer than a marker, or not followed by a
/// blank, is ordinary text: a wrapped token resumes as one run of
/// characters, it does not spell a one-character word.
fn starts_list_marker<S: CellSource>(src: &S, row: u16, from: u16, right: u16) -> bool {
    let mut tok = ['\0'; 6];
    let mut n = 0usize;
    let mut c = from;
    while c < right && n < tok.len() {
        let ch = src.char_at(c, row);
        if ch == ' ' || ch == '\0' {
            break;
        }
        tok[n] = ch;
        n += 1;
        c += 1;
    }
    // Ran to the buffer's end without hitting a blank — too long to
    // be a marker, and the cell after `c` was never examined.
    if n == 0 || n == tok.len() {
        return false;
    }
    if n == 1 {
        return !tok[0].is_alphanumeric();
    }
    tok[..n - 1].iter().all(|c| c.is_ascii_digit()) && matches!(tok[n - 1], '.' | ')')
}

/// The horizontal rules a table draws between its rows.  A row made
/// only of these (plus the junctions and blanks) separates two table
/// rows, so text above it and text below it belong to different cells
/// however aligned they look.
fn is_horizontal_rule(c: char) -> bool {
    matches!(c, '\u{2500}' | '\u{2501}' | '\u{2550}' | '\u{253C}' | '\u{254B}'
                | '\u{252C}' | '\u{2534}' | '\u{251C}' | '\u{2524}' | '\u{256A}'
                | '\u{256C}' | '\u{2566}' | '\u{2569}' | '\u{2560}' | '\u{2563}'
                | '\u{250C}' | '\u{2510}' | '\u{2514}' | '\u{2518}'
                | '\u{256D}' | '\u{256E}' | '\u{256F}' | '\u{2570}' | '-' | '=')
        || is_vertical_rule(c)
}

/// Is this row a separator between two table rows?
fn is_rule_row<S: CellSource>(src: &S, row: u16, left: u16, right: u16) -> bool {
    let mut saw_rule = false;
    for c in left..right {
        let ch = src.char_at(c, row);
        if ch == ' ' || ch == '\0' {
            continue;
        }
        if !is_horizontal_rule(ch) {
            return false;
        }
        saw_rule = true;
    }
    saw_rule
}

/// Does this row's cell start a fresh link rather than continue one?
///
/// The geometry test cannot tell "the cell wrapped mid-token" from
/// "the next table row's cell happens to be flush too" — a column
/// sized by its longest URL makes every row look full.  A scheme
/// (`https://`, `file://`, …) at the start of the continuation is the
/// giveaway: wrapped text resumes mid-token, it does not begin a new
/// address.
fn starts_new_scheme<S: CellSource>(src: &S, row: u16, from: u16, right: u16) -> bool {
    let mut seen = 0usize;
    let mut buf = ['\0'; 10];
    let mut c = from;
    while c < right && seen < buf.len() {
        let ch = src.char_at(c, row);
        if ch == '\0' || (ch == ' ' && seen == 0) {
            c += 1;
            continue;
        }
        buf[seen] = ch;
        seen += 1;
        c += 1;
    }
    let head: String = buf[..seen].iter().collect();
    let Some(pos) = head.find("://") else { return false };
    pos > 0 && head[..pos].chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '.' || c == '-')
}

/// Vertical rules a table draws between its cells.  Box-drawing light
/// / heavy / double, plus the ASCII pipe that markdown renderers and
/// `column -t` emit.
fn is_vertical_rule(c: char) -> bool {
    matches!(c, '\u{2502}' | '\u{2503}' | '\u{2506}' | '\u{2507}'
                | '\u{250A}' | '\u{250B}' | '\u{2551}' | '|')
}

/// Table-cell pass: a URL (or path) that a table wrapped across
/// several rows of ONE cell.
///
/// The row-level merge cannot see this.  It asks whether the previous
/// row ran out of room at the PANE's right edge, and a table cell runs
/// out of room at its own border — several columns short, with the
/// border glyph itself sitting where the heuristic looks for the last
/// character of the token.  So a URL in a table stayed cut at the cell
/// boundary, and the visible link was whatever prefix fit in the first
/// row (2026-07-28 field report).
///
/// What this does instead: find blocks of consecutive rows that share
/// at least two vertical rules (that is what makes them a table), and
/// scan each column band between two rules as its own logical line,
/// joined across rows by the same continuation test measured against
/// the BAND's edges.
///
/// Only multi-row matches are emitted.  A band that produced a single
/// row's worth of match adds nothing the row pass did not already
/// find, and emitting it again would be a duplicate the hit-test has
/// to arbitrate.  Whatever the row pass found on those same cells —
/// necessarily the truncated prefix — is dropped by
/// `drop_shadowed_ranges` in favour of the whole thing.
fn scan_table_cells<S: CellSource>(
    src: &S,
    cols: u16,
    rule_cols: &[(u16, u16)],
    out: &mut Vec<LinkRange>,
    oracle: &dyn PathOracle,
) {
    if rule_cols.len() < 4 {
        return;
    }
    // rule_cols is (row, col), pushed in row-major order by the scan.
    let mut row_start = 0usize;
    let mut block_rows: Vec<u16> = Vec::new();
    let mut shared: Vec<u16> = Vec::new();
    let mut scratch: Vec<u16> = Vec::new();
    while row_start < rule_cols.len() {
        let row = rule_cols[row_start].0;
        let mut row_end = row_start;
        while row_end < rule_cols.len() && rule_cols[row_end].0 == row {
            row_end += 1;
        }
        let this_row: &[(u16, u16)] = &rule_cols[row_start..row_end];
        let contiguous = block_rows.last().is_some_and(|r| r + 1 == row);
        if contiguous {
            scratch.clear();
            scratch.extend(
                shared
                    .iter()
                    .copied()
                    .filter(|c| this_row.iter().any(|(_, rc)| rc == c)),
            );
            if scratch.len() >= 2 {
                std::mem::swap(&mut shared, &mut scratch);
                block_rows.push(row);
                row_start = row_end;
                continue;
            }
        }
        // This row does not extend the block — close what we have.
        if block_rows.len() >= 2 && shared.len() >= 2 {
            scan_table_block(src, &block_rows, &shared, cols, out, oracle);
        }
        block_rows.clear();
        shared.clear();
        block_rows.push(row);
        shared.extend(this_row.iter().map(|(_, c)| *c));
        row_start = row_end;
    }
    if block_rows.len() >= 2 && shared.len() >= 2 {
        scan_table_block(src, &block_rows, &shared, cols, out, oracle);
    }
}

/// One table block: scan every column band between adjacent rules.
fn scan_table_block<S: CellSource>(
    src: &S,
    block_rows: &[u16],
    rules: &[u16],
    cols: u16,
    out: &mut Vec<LinkRange>,
    oracle: &dyn PathOracle,
) {
    let mut chars: Vec<char> = Vec::new();
    let mut col_map: Vec<u16> = Vec::new();
    let mut segments: Vec<LineSegment> = Vec::new();
    for w in rules.windows(2) {
        let (left, right) = (w[0] + 1, w[1]);
        if right <= left + 1 {
            continue;
        }
        chars.clear();
        col_map.clear();
        segments.clear();
        let mut prev_row: Option<u16> = None;
        for &r in block_rows {
            if is_rule_row(src, r, left, right) {
                // A separator between table rows: whatever follows is
                // a different cell, so nothing crosses it.
                flush_table_line(&chars, &col_map, &segments, cols, out, oracle);
                chars.clear();
                col_map.clear();
                segments.clear();
                prev_row = None;
                continue;
            }
            let joins = prev_row.is_some_and(|p| {
                is_hard_wrap_continuation_in(src, p, r, left, right)
                    && !starts_new_scheme(src, r, left, right)
            });
            if !joins {
                flush_table_line(&chars, &col_map, &segments, cols, out, oracle);
                chars.clear();
                col_map.clear();
                segments.clear();
            } else {
                // The cell pads its content out to the border; those
                // blanks are layout, not text, and would terminate the
                // token at the very seam we are crossing.
                while chars.last() == Some(&' ') {
                    chars.pop();
                    col_map.pop();
                }
            }
            let mut col_skip = left;
            if joins {
                while col_skip < right {
                    let ch = src.char_at(col_skip, r);
                    if ch == ' ' || ch == '\0' {
                        col_skip += 1;
                    } else {
                        break;
                    }
                }
            }
            segments.push(LineSegment {
                phys_row: r,
                char_offset: chars.len(),
                cc_zero_indent: false,
            });
            let mut prev_was_wide = false;
            for c in col_skip..right {
                let ch = src.char_at(c, r);
                if prev_was_wide && ch == '\0' {
                    prev_was_wide = false;
                    continue;
                }
                chars.push(if ch == '\0' || ch == ' ' { ' ' } else { ch });
                col_map.push(c);
                prev_was_wide = ch != '\0' && src.is_wide(ch);
            }
            prev_row = Some(r);
        }
        flush_table_line(&chars, &col_map, &segments, cols, out, oracle);
    }
}

/// Scan a band's logical line, but keep only matches that actually
/// crossed a row boundary — see `scan_table_cells`.
fn flush_table_line(
    chars: &[char],
    col_map: &[u16],
    segments: &[LineSegment],
    cols: u16,
    out: &mut Vec<LinkRange>,
    oracle: &dyn PathOracle,
) {
    if segments.len() < 2 {
        return;
    }
    let before = out.len();
    scan_logical_line(chars, col_map, segments, cols as usize, out, oracle);
    // A match confined to one row is one the row pass already had.
    let mut i = before;
    while i < out.len() {
        let same_text_rows = out[before..]
            .iter()
            .filter(|l| l.text == out[i].text)
            .count();
        if same_text_rows < 2 {
            out.remove(i);
        } else {
            i += 1;
        }
    }
}

/// Two ranges on one row that cover overlapping cells are the same
/// link seen twice — the row pass's truncated prefix and the table
/// pass's whole token.  Keep the longer text; it is the one the click
/// should open.
fn drop_shadowed_ranges(out: &mut Vec<LinkRange>) {
    // In place, no second Vec: this runs inside the per-frame scan,
    // where the module's whole allocation budget is one reused
    // buffer.  Sorted by (row, col_start, longest first), a range is
    // shadowed exactly when it starts at or before the furthest
    // column any kept range on that row already reaches.
    out.sort_by(|a, b| {
        a.row
            .cmp(&b.row)
            .then(a.col_start.cmp(&b.col_start))
            .then(b.text.len().cmp(&a.text.len()))
    });
    let mut row = u16::MAX;
    let mut reach = 0u16;
    out.retain(|l| {
        if l.row != row {
            row = l.row;
            reach = l.col_end;
            return true;
        }
        if l.col_start <= reach {
            return false;
        }
        reach = reach.max(l.col_end);
        true
    });
}

/// Locate claudecode's composer in the visible grid, as an inclusive
/// `(top_row, bottom_row)` range to exclude from link scanning.
///
/// **This used to look for a rounded box** (`╭…╮` / `╰…╯`) and return
/// `None` when it found none.  That has now failed twice, for the
/// same reason both times: the thing it keyed on was *decoration*,
/// and claudecode redecorates.  v2.1.212 dropped the box, which made
/// the bottom-most box the welcome banner (fixed then by requiring
/// the caret or a bottom-hugging position).  The current version
/// draws no box at all — two grey `─` rules with the prompt between
/// them — so corner detection finds nothing, the exemption never
/// fires, and the composer gets scanned like body text: a path
/// underlines itself while you are still typing it.
///
/// So the anchor is the **caret**.  A caret is protocol, not
/// styling; no redesign can remove it, and in a claudecode pane it
/// lives in the composer.  The composer is bottom-anchored in every
/// version we have seen, so:
///
///   - the caret must sit in the bottom third of the view — higher
///     than that and it is somewhere in the output, not the input,
///     and exempting downward from it would swallow real text;
///   - the range runs from the caret's row to the last row: below
///     the composer there is only claudecode's own footer;
///   - it widens *upward* to the nearest separator row (a run of box
///     horizontals, which covers both the `─` rules of today and the
///     `╭` of the old box) so a multi-line prompt is covered whole,
///     not just the line the caret happens to be on.  The search is
///     bounded, and finding nothing simply means the caret's row
///     alone — under-exempting is recoverable, swallowing output is
///     not.
///
/// The legacy corner scan is kept as a fallback for the one case the
/// caret cannot speak for: a scrolled-back view, where the caret is
/// outside the window entirely.
fn find_input_box_rows<S: CellSource>(
    src: &S,
    rows: u16,
    cols: u16,
) -> Option<(u16, u16)> {
    let (_, caret_row) = src.cursor();
    if caret_row < rows {
        // "Near the bottom", expressed as a distance rather than a
        // fraction: the composer is at most `MAX_COMPOSER_ROWS` tall,
        // and never more than half the view — a fraction alone
        // degenerates on short panes, where a third of ten rows is
        // three and the composer is half the screen.
        let reach = MAX_COMPOSER_ROWS.min(rows / 2);
        if caret_row + reach >= rows {
            let mut top = caret_row;
            let mut r = caret_row;
            let mut walked = 0u16;
            while r > 0 && walked < MAX_COMPOSER_ROWS {
                r -= 1;
                walked += 1;
                if is_separator_row(src, r, cols) {
                    top = r;
                    break;
                }
            }
            return Some((top, rows - 1));
        }
    }
    find_rounded_box_rows(src, rows, cols)
}

/// How far above the caret to look for the composer's opening rule.
/// Generous enough for a multi-line prompt, short enough that a
/// missing separator cannot eat a screenful of output.
const MAX_COMPOSER_ROWS: u16 = 12;

/// A row that is drawn rule, not text: box horizontals and corners
/// with nothing else on it.  Both claudecode chromes qualify — the
/// old `╭────╮` and the current bare `────`.
fn is_separator_row<S: CellSource>(src: &S, row: u16, cols: u16) -> bool {
    let mut rule = 0u16;
    let mut other = 0u16;
    for c in 0..cols {
        match src.char_at(c, row) {
            ' ' | '\0' => {}
            '─' | '━' | '═' | '╭' | '╮' | '╰' | '╯' | '┌' | '┐' | '└' | '┘' | '│' | '├' | '┤'
            | '┬' | '┴' | '┼' => rule += 1,
            _ => other += 1,
        }
    }
    // Half the width of rule glyphs and nothing else: the composer's
    // divider spans most of the pane, while a table row of the same
    // glyphs carries text between them.
    other == 0 && rule >= cols / 2
}

/// The pre-2026-08 detection, kept for the scrolled-back case where
/// the caret is not in view.  Returns the bottom-most rounded box.
fn find_rounded_box_rows<S: CellSource>(
    src: &S,
    rows: u16,
    cols: u16,
) -> Option<(u16, u16)> {
    let mut bottom: Option<u16> = None;
    for r in (0..rows).rev() {
        if row_contains_any(src, r, cols, &['╰', '╯']) {
            bottom = Some(r);
            break;
        }
    }
    let bottom = bottom?;
    if bottom == 0 {
        return None;
    }
    let mut top: Option<u16> = None;
    for r in (0..bottom).rev() {
        if row_contains_any(src, r, cols, &['╭', '╮']) {
            top = Some(r);
            break;
        }
    }
    let top = top?;
    // 2026-07-18 field regression — the bottom-most rounded box is
    // only the COMPOSER when it looks like an active input area:
    // it either contains the cursor row (the user's caret lives in
    // the composer) or hugs the bottom of the viewport.  claudecode
    // v2.1.212 dropped the composer's box entirely, which made the
    // bottom-most box the WELCOME BANNER at the top of the screen —
    // exempting it swallowed the banner's email/path links.
    let (_, cursor_row) = src.cursor();
    let has_cursor = cursor_row >= top && cursor_row <= bottom;
    let hugs_bottom = bottom + 4 >= rows;
    if !has_cursor && !hugs_bottom {
        return None;
    }
    Some((top, bottom))
}

fn row_contains_any<S: CellSource>(
    src: &S,
    row: u16,
    cols: u16,
    targets: &[char],
) -> bool {
    for c in 0..cols {
        let ch = src.char_at(c, row);
        if targets.contains(&ch) {
            return true;
        }
    }
    false
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

fn count_leading_ws<S: CellSource>(src: &S, row: u16, cols: u16) -> u16 {
    let mut n = 0u16;
    while n < cols {
        let ch = src.char_at(n, row);
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
    oracle: &dyn PathOracle,
) {
    if segments.is_empty() {
        return;
    }
    // Pattern scan emits matches directly into `out` — the previous
    // intermediate `row_matches` Vec was a per-call allocation that
    // showed up as ~250 samples in the input-lag profile (15 s, 9
    // panes typing).  emit_match is the only producer, append-only,
    // so passing `out` straight through is safe and saves the alloc.
    scan_line_into_matches(chars, col_map, out, segments, cols_per_row, oracle);
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
    oracle: &dyn PathOracle,
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
            let end = scan_path_candidate(&chars, i);
            // Greedy scan, filesystem arbitration — `resolve_path_end`
            // is the whole decision.  The seam retry stays separate:
            // its candidates are cc hard-wrap boundaries, not prose
            // marks, and a seam prefix that happens to be a real
            // *directory* must not win over the file the line points
            // at, so it is tried only after every prose cut has.
            if let Some(b) = resolve_path_end(&chars, i, end, oracle) {
                let text = unquote_path(&chars[i..b].iter().collect::<String>());
                emit_match(out, segments, col_map, cols_per_row, i, b, LinkKind::File, text);
                i = b.max(i + 1);
                continue;
            }
            // No `looks_like_path` guard on the whole span here.  The
            // span NOT looking like a path is precisely the case this
            // retry exists for: rows merged at a wrap seam produce
            // things like `…/join-bugs.sql/Users/…/describe/` — one
            // real path with another glued to its tail — and asking
            // whether the concatenation looks like a path answers no,
            // which is right and which is why the answer must not
            // gate the retry.  `retry_file_at_segment_boundaries`
            // applies `looks_like_path` to each candidate PREFIX,
            // where the question is the one worth asking.
            //
            // 2026-08-19 field report: three consecutive absolute
            // paths, each ending within the 8-column slack of the
            // right edge, merged into one logical line; the retry had
            // the correct answer at every step and never got asked.
            {
                if let Some(b) = retry_file_at_segment_boundaries(chars, segments, i, end, oracle) {
                    let text = unquote_path(&chars[i..b].iter().collect::<String>());
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
            let end = scan_path_candidate(&chars, i);
            if end - i >= 3 {
                if let Some(b) = resolve_path_end(&chars, i, end, oracle) {
                    let text = unquote_path(&chars[i..b].iter().collect::<String>());
                    emit_match(out, segments, col_map, cols_per_row, i, b, LinkKind::File, text);
                    i = b.max(i + 1);
                    continue;
                }
                if looks_like_path(&chars[i..end]) {
                    if let Some(b) = retry_file_at_segment_boundaries(chars, segments, i, end, oracle) {
                        let text = unquote_path(&chars[i..b].iter().collect::<String>());
                        emit_match(out, segments, col_map, cols_per_row, i, b, LinkKind::File, text);
                        i = b;
                        continue;
                    }
                }
            }
        }

        // Bare IPv4 (optionally :port and /path).  Emitted as `Ip` so
        // the click dispatcher's OpenLink arm prepends `http://` and
        // hands off to `/usr/bin/open`; Copy keeps the raw displayed
        // text.  Reject rules kill version strings (`v1.2.3.4`,
        // `1.2.3.4.5`, `1.2.3.4-rc1`), IPs inside longer identifiers,
        // and IPs already inside an `http://…` URL (prev char is `/`).
        if c.is_ascii_digit() {
            if let Some(end) = try_scan_ipv4_url(chars, i) {
                let text: String = chars[i..end].iter().collect();
                emit_match(
                    out, segments, col_map, cols_per_row, i, end,
                    LinkKind::Ip, text,
                );
                i = end;
                continue;
            }
        }

        // IPv6 — bracketed form (`[::1]:8080/foo`) triggers on `[`;
        // bare form (`2001:db8::1`, `::1`, or full 8-group) triggers
        // on hex or `:`.  `::` OR exactly 8 groups is required for
        // bare form so time strings (`12:34:56`) don't match.
        if c == '[' {
            if let Some(end) = try_scan_ipv6(chars, i) {
                let text: String = chars[i..end].iter().collect();
                emit_match(
                    out, segments, col_map, cols_per_row, i, end,
                    LinkKind::Ip, text,
                );
                i = end;
                continue;
            }
        }
        if c.is_ascii_hexdigit() || c == ':' {
            if let Some(end) = try_scan_ipv6(chars, i) {
                let text: String = chars[i..end].iter().collect();
                emit_match(
                    out, segments, col_map, cols_per_row, i, end,
                    LinkKind::Ip, text,
                );
                i = end;
                continue;
            }
        }

        // UUID — canonical `8-4-4-4-12` hex.  Triggers on hex digit;
        // the shape is rigid enough that false-positive risk is
        // negligible without further gating.
        if c.is_ascii_hexdigit() {
            if let Some(end) = try_scan_uuid(chars, i) {
                let text: String = chars[i..end].iter().collect();
                emit_match(
                    out, segments, col_map, cols_per_row, i, end,
                    LinkKind::Uuid, text,
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
    oracle: &dyn PathOracle,
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
        if is_real_path(oracle, &text) {
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
    // One LinkRange per physical row the match touches, and each
    // row's span is the cells it ACTUALLY covers — read back from
    // `col_map`, the same projection `locate()` uses.
    //
    // The earlier shape ran every row but the last out to the pane's
    // right edge, on the reasoning that a wrapped line fills its row.
    // That holds for a DECAWM soft wrap and nothing else: a cc-mode
    // hard wrap stops a few columns short, and a table cell stops at
    // its border — where the underline then ran on through the border
    // and out the far side of the table (2026-07-28, visible the
    // moment table cells started merging).  Reading the columns back
    // is both simpler and right in all three cases; for the soft wrap
    // it produces the identical answer, because there the last char
    // really is in the last column.
    //
    // `cols_per_row` still bounds the projection: a column at or past
    // the row width is not a cell anyone can click.
    for (i, seg) in segments.iter().enumerate() {
        let seg_lo = seg.char_offset;
        let seg_hi = segments
            .get(i + 1)
            .map(|s| s.char_offset)
            .unwrap_or(col_map.len());
        let lo = seg_lo.max(char_lo);
        let hi = seg_hi.min(char_hi_exclusive);
        if lo >= hi {
            continue;
        }
        let (Some(&c0), Some(&c1)) = (col_map.get(lo), col_map.get(hi - 1)) else {
            continue;
        };
        if c0 as usize >= cols_per_row || c1 < c0 {
            continue;
        }
        let c1 = c1.min(cols_per_row.saturating_sub(1) as u16);
        out.push(LinkRange {
            row: seg.phys_row,
            col_start: c0,
            col_end: c1,
            kind,
            // Every segment carries the FULL text, so click dispatch
            // is the same wherever the user hit it.
            text: text.clone(),
        });
    }
}

/// Scan one already-joined line of text.  The public single-line
/// entry point for callers without a cell surface (log viewers,
/// notification text, tests); the full-surface path is
/// [`scan_visible_links`].
pub fn scan_text_line(line: &str, row: u16) -> Vec<LinkRange> {
    let mut out = Vec::new();
    scan_line(line, row, &mut out);
    out
}

/// Legacy single-row scanner.  Kept for tests + callers that already
/// produce a single-row joined `line`; new code paths go through
/// `scan_line_into_matches` to benefit from soft-wrap merge.
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
        cc_zero_indent: false,
    }];
    scan_line_into_matches(&chars, &col_map, out, &segments, cols, &FsOracle);
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

/// Try to parse `chars[start..]` as an IPv6 address.  Two shapes:
///
///   - Bracketed: `[<ipv6>]` with optional `:port` and `/path` —
///     unambiguous, no false-positive risk
///   - Bare: `<ipv6>` — requires either `::` or the full 8-group form
///     so time strings (`12:34:56`) don't trip it
///
/// Validation delegates to `std::net::Ipv6Addr::from_str`; the local
/// gate just carves out the candidate token from the character stream
/// and enforces the `::`-or-8-group rule for the bare form.
fn try_scan_ipv6(chars: &[char], start: usize) -> Option<usize> {
    if start > 0 {
        let prev = chars[start - 1];
        if prev.is_ascii_alphanumeric()
            || matches!(prev, '.' | ':' | '-' | '_' | '@' | '/')
        {
            return None;
        }
    }
    if chars.get(start) == Some(&'[') {
        // Bracketed form.  Body is hex + `:` + `.` (for the IPv4-
        // mapped `::ffff:1.2.3.4` shape); anything else in the
        // brackets means it's not an IPv6 literal.
        let body_start = start + 1;
        let mut body_end = body_start;
        while body_end < chars.len() && chars[body_end] != ']' {
            let c = chars[body_end];
            if !(c.is_ascii_hexdigit() || c == ':' || c == '.') {
                return None;
            }
            body_end += 1;
        }
        if chars.get(body_end) != Some(&']') {
            return None;
        }
        let body: String = chars[body_start..body_end].iter().collect();
        body.parse::<std::net::Ipv6Addr>().ok()?;
        let mut i = body_end + 1;
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
            // Trim prose punctuation from the path tail.
            while i > body_end + 1 {
                let last = chars[i - 1];
                if matches!(last, ',' | '.' | ';' | ':' | ')' | '}' | '!' | '?') {
                    i -= 1;
                } else {
                    break;
                }
            }
        }
        return Some(i);
    }
    // Bare form: sweep hex + `:` + `.`, then validate + apply the
    // discriminator (contains `::` OR exactly 7 colons = 8 groups).
    let mut i = start;
    while i < chars.len() {
        let c = chars[i];
        if !(c.is_ascii_hexdigit() || c == ':' || c == '.') {
            break;
        }
        i += 1;
    }
    if i == start {
        return None;
    }
    // Right boundary — the token must not slide into an identifier.
    if let Some(&next) = chars.get(i) {
        if next.is_ascii_alphanumeric() || matches!(next, '.' | '-' | '_') {
            return None;
        }
    }
    let body: String = chars[start..i].iter().collect();
    let colon_count = body.chars().filter(|&c| c == ':').count();
    // `::` compression is the sharpest IPv6 signal; the 8-group form
    // (7 colons) is the other unambiguous shape.  Everything else
    // falls back to being possibly-time / possibly-URL-port fragment
    // and is rejected here (the URL scanner already ran).
    if !body.contains("::") && colon_count != 7 {
        return None;
    }
    body.parse::<std::net::Ipv6Addr>().ok()?;
    Some(i)
}

/// Canonical UUID: 8-4-4-4-12 hex with dashes.  Anchored at word
/// boundaries so partial matches inside longer identifiers don't
/// trip.  Emits raw text; the click dispatcher offers Copy only.
fn try_scan_uuid(chars: &[char], start: usize) -> Option<usize> {
    if start > 0 {
        let prev = chars[start - 1];
        if prev.is_ascii_alphanumeric() || matches!(prev, '-' | '_' | '.' | ':' | '@' | '/') {
            return None;
        }
    }
    let seg_lens = [8usize, 4, 4, 4, 12];
    let mut i = start;
    for (idx, &want) in seg_lens.iter().enumerate() {
        if idx > 0 {
            if chars.get(i) != Some(&'-') {
                return None;
            }
            i += 1;
        }
        let s0 = i;
        while i < chars.len() && chars[i].is_ascii_hexdigit() && i - s0 < want {
            i += 1;
        }
        if i - s0 != want {
            return None;
        }
    }
    if let Some(&next) = chars.get(i) {
        // `/` — a uuid-named PATH COMPONENT (claude scratchpads,
        // session dirs) is not a standalone UUID token; claiming it
        // here splits the surrounding path into garbage links
        // (2026-07-18 field report).  Mirrors the `/` in the
        // left-boundary reject set.
        if next.is_ascii_alphanumeric() || matches!(next, '-' | '_' | '.' | '/') {
            return None;
        }
    }
    Some(i)
}

/// Widest run that could still be one filesystem path.
///
/// Stops only where a path genuinely **cannot** continue: whitespace,
/// control characters, and the quoting / redirection metacharacters a
/// shell would need escaped anyway.
///
/// Punctuation is deliberately NOT a stop — not ASCII `(`, not the
/// fullwidth CJK family.  It used to be, on the reasoning that CJK
/// prose glues those onto paths far more often than filenames contain
/// them (宁可漏不可错).  That reasoning is sound only for a scanner
/// with no way to check its answer, and this one has the best check
/// there is: **a path link is only ever emitted for a path that
/// exists on disk.**  Guessing narrow could only ever lose true
/// positives — which it did: a Japanese document named
/// `株主総会議事録（役員報酬改定・20260805）.pdf` was cut at `（`,
/// and the line lost its link entirely (2026-08-05 report).
///
/// So: scan greedily, and let [`resolve_path_end`] ask the filesystem
/// where the name really ended.
fn scan_path_candidate(chars: &[char], start: usize) -> usize {
    let mut i = start;
    while i < chars.len() {
        let c = chars[i];
        // A quoted run, or an escaped space.  A path with a space in
        // it is not rare — it is what `~/Library/Safari/"Favicon
        // Cache"` looks like the moment anyone pastes a command that
        // touches one — and stopping at the quote leaves the link
        // ending mid-name (2026-08-09 report).  Both shells' spellings
        // are accepted; [`unquote_path`] takes them back off before
        // anything is asked of the filesystem.
        if (c == '"' || c == '\'') && i > start {
            match closing_quote(chars, i) {
                Some(close) => {
                    i = close + 1;
                    continue;
                }
                // An unmatched quote is prose, not a name.
                None => break,
            }
        }
        if c == '\\' && i + 1 < chars.len() && chars[i + 1] == ' ' {
            i += 2;
            continue;
        }
        if c.is_whitespace() || c == '\0' || (c.is_control() && c != '\t') {
            break;
        }
        if matches!(c, '<' | '>' | '"' | '\'' | '`' | '|') {
            break;
        }
        i += 1;
    }
    i
}

/// The partner of the quote at `open`, if it is on this line and
/// close enough to be one.
///
/// Bounded because an unpaired quote is common in prose (`don't`,
/// `"as we said"`) and an unbounded search would happily pair one
/// with another sentence's, swallowing the line between them.  The
/// bound is generous — names with spaces are still names, not
/// paragraphs — and anything it lets through still has to survive
/// `stat`.
fn closing_quote(chars: &[char], open: usize) -> Option<usize> {
    const MAX_QUOTED_LEN: usize = 96;
    let q = chars[open];
    let hi = (open + 1 + MAX_QUOTED_LEN).min(chars.len());
    (open + 1..hi).find(|&j| chars[j] == q)
}

/// Take shell quoting back off a candidate before the filesystem is
/// asked about it.
///
/// The screen shows `~/Library/Safari/"Favicon Cache"`; the thing on
/// disk is `~/Library/Safari/Favicon Cache`.  The link's *span* stays
/// on what is drawn — that is what the user points at — while its
/// target is what this returns.
fn unquote_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = it.next() {
        match c {
            // Inside single quotes a backslash is literal, as in sh.
            '\\' if quote != Some('\'') => {
                if let Some(n) = it.next() {
                    out.push(n);
                }
            }
            '"' | '\'' if quote.is_none() => quote = Some(c),
            c if Some(c) == quote => quote = None,
            other => out.push(other),
        }
    }
    out
}

/// Could prose have started here?
///
/// Every such position is a candidate end for the path — nothing
/// more.  Being generous costs one `stat` that the cache mostly
/// absorbs; being stingy costs a link the user can see is missing.
///
/// `:` is the one mark deliberately left out.  `path:120:5` is
/// already arbitrated by [`strip_line_col_suffix`], and the rest of
/// that family — `path:note`, `host:path` — was settled as 宁可漏 by
/// the 2026-07-13 report.  Cutting there would reach past what this
/// change is about: the greedy scan exists so that punctuation stops
/// **terminating** names, not so that every mark becomes a place to
/// chop one.
fn is_prose_cut_point(c: char) -> bool {
    matches!(
        c,
        ',' | '.' | ';' | '!' | '?' | '(' | ')' | '[' | ']' | '{' | '}'
            // A quote both opens a name-with-spaces and closes one, so
            // it is also where a name can end and prose resume.  The
            // greedy scan pairs them; this is what lets the arbitration
            // back out when the pairing was wrong.
            | '"' | '\''
    ) || matches!(
        c,
        '\u{3001}'..='\u{3003}'   // 、。〃
            | '\u{3008}'..='\u{301F}' // 〈〉《》「」『』【】〔〕〖〗〘〙〚〛〜〝〞
            | '\u{FF01}'            // ！
            | '\u{FF08}' | '\u{FF09}' // （）
            | '\u{FF0C}'            // ，
            | '\u{FF1A}' | '\u{FF1B}' // ：；
            | '\u{FF1F}'            // ？
            | '\u{2018}' | '\u{2019}' | '\u{201C}' | '\u{201D}' // 弯引号
            | '\u{2026}'            // …
            | '\u{30FB}'            // ・
    )
}

/// Most candidate ends we will `stat` for one token.
///
/// This runs on the render path — `build_instances` scans every
/// visible pane every frame — so the greedy scan has to come with a
/// ceiling.  A real path glued to prose resolves within the first two
/// or three tries; a line of pure punctuation is what the cap is for.
const MAX_PATH_CANDIDATES: usize = 24;

/// Trim the trailing marks that end a sentence rather than a name.
///
/// Tried as a *variant* of each candidate, never instead of it: a
/// filename really ending in one of these is legal, and the untrimmed
/// form is offered to the filesystem first.
fn trim_sentence_tail(chars: &[char], lo: usize, mut end: usize) -> usize {
    while end > lo
        && matches!(chars[end - 1], ',' | '.' | ';' | ':' | ')' | ']' | '}' | '!' | '?')
    {
        end -= 1;
    }
    end
}

/// Where did the path actually end?  Ask the disk, longest first.
///
/// The scan is deliberately greedy, so this is the whole arbitration:
/// each candidate end is offered to `is_real_path`, longest first, and
/// the first one that exists wins.  Longest-first matters — when both
/// a file and the directory prefixing it exist, the line is pointing
/// at the file.
///
/// Cannot invent a link: prose does not name files that exist.
fn resolve_path_end(
    chars: &[char],
    lo: usize,
    hi: usize,
    oracle: &dyn PathOracle,
) -> Option<usize> {
    let mut tried = 0usize;
    let mut last: Option<usize> = None;
    let consider = |end: usize, tried: &mut usize, last: &mut Option<usize>| -> bool {
        if end <= lo || *last == Some(end) || *tried >= MAX_PATH_CANDIDATES {
            return false;
        }
        *last = Some(end);
        *tried += 1;
        let span = &chars[lo..end];
        if !looks_like_path(span) {
            return false;
        }
        let text: String = span.iter().collect();
        is_real_path(oracle, &unquote_path(&text))
    };
    // The whole token, then the whole token minus its sentence tail.
    if consider(hi, &mut tried, &mut last) {
        return Some(hi);
    }
    let trimmed = trim_sentence_tail(chars, lo, hi);
    if trimmed < hi && consider(trimmed, &mut tried, &mut last) {
        return Some(trimmed);
    }
    // rustc / panic output habitually appends `:line(:col)`.
    let stripped = strip_line_col_suffix(chars, lo, hi);
    if stripped < hi && consider(stripped, &mut tried, &mut last) {
        return Some(stripped);
    }
    // Then every point prose could have started, longest first.
    for cut in (lo + 1..hi).rev() {
        if tried >= MAX_PATH_CANDIDATES {
            break;
        }
        if !is_prose_cut_point(chars[cut]) {
            continue;
        }
        let end = trim_sentence_tail(chars, lo, cut);
        if consider(end, &mut tried, &mut last) {
            return Some(end);
        }
    }
    None
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

/// Ask the oracle whether the candidate path is real.  Only
/// `Exists` makes a link: `Unknown` — the async oracle has not
/// resolved this path yet — reads exactly like `Missing` here, so a
/// path the oracle has not seen simply is not underlined on this
/// frame.  It becomes a link on a later frame, once the answer
/// lands.  That collapse is the whole point of the three-state
/// verdict: the scanner needs no notion of "pending".
fn is_real_path(oracle: &dyn PathOracle, path: &str) -> bool {
    matches!(oracle.probe(path), PathVerdict::Exists)
}

/// Turn a link's text into a path the OS will accept.
///
/// `~` is shell syntax, not filesystem syntax: nothing below the shell
/// expands it.  This matters beyond the existence check — whoever ACTS
/// on a file link (opening it, revealing it) has to hand the same
/// expanded path to the OS, or the link resolves here and fails there.
/// That was live until 2026-08-21: every `~/…` file link stat'ed fine,
/// underlined, and then did nothing when opened, because
/// `/usr/bin/open` took `~` for a directory name and looked for it
/// under the process's cwd.
///
/// Returns `None` only when `~/` is present and `HOME` is not — there
/// is no sensible path to hand back in that case.
pub fn expand_user_path(path: &str) -> Option<PathBuf> {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(|home| {
            let mut p = PathBuf::from(home);
            p.push(rest);
            p
        }),
        None => Some(PathBuf::from(path)),
    }
}

/// What the oracle knows about one candidate path.
///
/// `Unknown` exists so an oracle can answer without touching the
/// filesystem.  `scan_visible_links` runs inside `build_instances`,
/// once per pane per frame; an oracle that blocks on `lstat` there
/// puts a syscall — and a page-cache miss, and whatever the disk is
/// doing — on the render thread.  On 2026-08-22 that cost a live
/// 14-pane window frames of 2.1 s to 13.6 s (`l2.loop.stall`,
/// build-bound), with 79.5 % of render self-time in `lstat` under
/// this call.  An async oracle answers `Unknown` on a miss, queues
/// the probe, and the link appears a frame or two later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathVerdict {
    /// The path resolves to a filesystem entry.
    Exists,
    /// The path was checked and does not resolve.
    Missing,
    /// Not checked yet.  Never blocks; treated as "no link, for now".
    Unknown,
}

/// Decides whether a candidate path is real.  Injected so the stone
/// crate itself performs no I/O: the scanner is pure over
/// (cells, opts, oracle), which is also what makes it testable
/// without a filesystem.
pub trait PathOracle {
    fn probe(&self, path: &str) -> PathVerdict;
}

/// The blocking oracle: one `symlink_metadata` per call, no cache.
///
/// Correct for one-shot callers (tests, `scan_text_line`, snapshot
/// rendering) where a handful of stats is cheaper than any cache.
/// NOT for the render loop — see [`PathVerdict::Unknown`].
pub struct FsOracle;

impl PathOracle for FsOracle {
    fn probe(&self, path: &str) -> PathVerdict {
        match expand_user_path(path) {
            Some(expanded) if std::fs::symlink_metadata(&expanded).is_ok() => PathVerdict::Exists,
            _ => PathVerdict::Missing,
        }
    }
}

/// An oracle that answers `Unknown` for everything — no link is ever
/// a file link.  For callers that want URL/email/IP detection with
/// zero filesystem access.
pub struct NoFsOracle;

impl PathOracle for NoFsOracle {
    fn probe(&self, _path: &str) -> PathVerdict {
        PathVerdict::Unknown
    }
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
    // Host characters must be a sane subset.
    if !host.iter().all(|c| {
        c.is_ascii_alphanumeric() || matches!(*c, '.' | '-' | ':')
    }) {
        return false;
    }
    // Split off an explicit port so the name and the port can be
    // judged separately — which is the whole of the rule below.
    let (name, port) = match host.iter().position(|c| *c == ':') {
        Some(i) => (&host[..i], Some(&host[i + 1..])),
        None => (host, None),
    };
    if name.is_empty() {
        return false;
    }
    let port_ok = match port {
        // `http://host:port` — the placeholder people actually write —
        // stays rejected, because `port` is not a number.
        Some(p) => !p.is_empty() && p.len() <= 5 && p.iter().all(|c| c.is_ascii_digit()),
        None => false,
    };
    // A dotted name is the ordinary case.  An explicit numeric port is
    // the other one, and it is what a dev terminal is full of:
    // `localhost:6014`, `myserver:8080`.  A port is what separates a
    // real address from the `https://x` placeholders this filter exists
    // to reject — those never carry one, and the `http://host:port`
    // people actually type fails the digits test above.
    //
    // 2026-08-07 report: `http://localhost:6014/tools/documents` went
    // unlinked.  The rule was written as "no dot, no digits" and
    // implemented as "no dot" — the digits half was never there.
    //
    // Bare `https://localhost` stays rejected.  That was settled
    // separately (`url_without_dot_in_host_rejected`: it is the shape
    // chat examples take), and a port is all this report needs.
    name.iter().any(|c| *c == '.') || port_ok
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

    /// A path wrapped mid-token, with the break landing on an
    /// ordinary character rather than on a separator.
    ///
    /// The merge used to demand the first row end within 2 cells of
    /// the pane edge, relaxing to 8 only when the row ended in `/`.
    /// claudecode's `●`/`⎿` blocks wrap at a content width several
    /// cells short of the pane, so where the break lands decides
    /// whether the path survives — and the path does not get a say in
    /// that.  Here it landed inside `…-e18-ve` / `rdict.md`: the
    /// halves stayed separate, the first half named nothing that
    /// exists, and the link fell back to the last directory that did
    /// (`~/workspace/labs/lab36-continus/`), which is exactly what the
    /// user saw underlined (2026-08-16).
    ///
    /// Swept across the gap widths a real pane produces, because a
    /// fixture at one width only proves that width.
    #[test]
    fn a_path_wrapped_mid_token_is_still_one_path() {
        let dir = std::env::temp_dir().join("marspot-linkify-wrap-test");
        let deep = dir.join("reports");
        if std::fs::create_dir_all(&deep).is_err() {
            return; // no temp dir: nothing to assert against
        }
        let file = deep.join("2026-08-16-e18-verdict.md");
        if std::fs::write(&file, b"x").is_err() {
            return;
        }
        let full = file.to_string_lossy().into_owned();
        // Cut the path mid-token, two characters before the end.
        let cut = full.len() - 8;
        let r0 = format!("● Write({}", &full[..cut]);
        let r1 = format!("  {})", &full[cut..]);
        let width = r0.chars().count() as u16;

        // Gaps of 0..=7 columns between the text and the pane edge.
        // Seven is what the slack buys; the case that prompted this
        // was a gap of two, which the old slack of 2 already refused
        // (it tolerated one), so the margin is real rather than
        // fitted to the one report.
        for gap in 0..=7u16 {
            let cols = width + gap;
            let mut src = StrSource::new(&[r0.as_str(), r1.as_str(), ""], cols);
            src.cursor = (0, 2);
            let links = scan_visible_links(&src, ScanOpts { tui_mode: true });
            assert!(
                links.iter().any(|l| l.text == full),
                "gap {gap}: the two halves must rejoin into {full:?}, got {:?}",
                links.iter().map(|l| &l.text).collect::<Vec<_>>(),
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// claudecode's composer as it is drawn **today** — two grey
    /// rules with the prompt between them, no rounded corners
    /// anywhere.  Captured off the wire from a live pane
    /// (2026-08-14): `…/effort ␛[1B ─────… ␛[1B ❯ ␛[6G␛[K ␛[1B
    /// ─────… ⏵⏵ bypass permissions on`.
    ///
    /// The path being typed must not underline itself, and the path
    /// in the output *above* the composer must still be a link —
    /// exempting the composer is not permission to stop scanning.
    #[test]
    fn the_composer_is_exempt_even_when_it_has_no_box() {
        // Real paths on both sides: a span only survives the scan if
        // it exists on disk, so a fixture of invented names would
        // pass whether or not the exemption worked.
        let rule: String = "─".repeat(40);
        let rows = [
            "wrote /etc/hosts just now",
            "",
            "  Ran 1 shell command",
            "",
            "some more output",
            rule.as_str(),
            "❯ /usr/bin",
            rule.as_str(),
            "⏵⏵ bypass permissions on (shift+tab to cycle)",
        ];
        let mut src = StrSource::new(&rows, 46);
        src.cursor = (10, 6); // caret in the prompt row
        let links = scan_visible_links(&src, ScanOpts { tui_mode: true });
        assert!(
            links.iter().all(|l| l.row != 2),
            "the composer must not be scanned: {links:?}",
        );
        assert!(
            links.iter().any(|l| l.row == 0 && l.text.contains("/etc/hosts")),
            "output above the composer is still linkable: {links:?}",
        );
    }

    /// The shape claudecode shipped in v2.1.212 — no box, no rules,
    /// just the prompt line.  There is nothing above the caret to
    /// widen to, so the caret's own row is the exemption, and the
    /// bounded upward walk must not swallow the output above it.
    #[test]
    fn a_composer_with_no_chrome_at_all_still_exempts_its_own_row() {
        let rows = [
            "see /etc/hosts for details",
            "and /usr/share too",
            "❯ /usr/bin",
        ];
        let mut src = StrSource::new(&rows, 40);
        src.cursor = (10, 2);
        let links = scan_visible_links(&src, ScanOpts { tui_mode: true });
        assert!(links.iter().all(|l| l.row != 2), "caret row exempt: {links:?}");
        assert!(
            links.iter().filter(|l| l.row == 0 || l.row == 1).count() >= 2,
            "both output rows keep their links: {links:?}",
        );
    }

    /// A caret parked up in the output — which happens mid-repaint —
    /// must not exempt everything below it.  This is the failure the
    /// bottom-third guard exists to prevent, and it is worse than the
    /// bug being fixed: it would silently drop links from real text.
    #[test]
    fn a_caret_in_the_output_does_not_blank_the_rows_below_it() {
        let rows = [
            "line one",
            "see /etc/hosts here",
            "see /usr/bin here",
            "see /usr/lib here",
            "see /var/log here",
            "see /usr/share here",
        ];
        let mut src = StrSource::new(&rows, 30);
        src.cursor = (0, 1); // top of the view, nowhere near a composer
        let links = scan_visible_links(&src, ScanOpts { tui_mode: true });
        for r in 1..=5u16 {
            assert!(
                links.iter().any(|l| l.row == r),
                "row {r} lost its link to a bogus exemption: {links:?}",
            );
        }
    }

    /// A separator row is drawn rule and nothing else.  A table row
    /// built from the same glyphs carries text between them and must
    /// not be mistaken for one, or the upward walk would stop early
    /// and leave part of a multi-line prompt scanned.
    #[test]
    fn a_table_row_is_not_a_separator() {
        let src = StrSource::new(&["│ name │ size │", "──────────────"], 15);
        assert!(!is_separator_row(&src, 0, 15), "text between rules is a table");
        assert!(is_separator_row(&src, 1, 15), "a bare rule is a separator");
    }

    /// Minimal CellSource over plain rows of text — what a test needs
    /// and nothing more.  Wide chars are "everything above ASCII that
    /// the terminal would render double-width"; for tests the CJK +
    /// fullwidth ranges suffice.
    pub(crate) struct StrSource {
        pub rows: Vec<Vec<char>>,
        pub cols: u16,
        pub cursor: (u16, u16),
        pub soft_wrapped: Vec<bool>,
    }
    impl StrSource {
        pub fn new(rows: &[&str], cols: u16) -> Self {
            let rows: Vec<Vec<char>> = rows
                .iter()
                .map(|r| {
                    let mut v: Vec<char> = r.chars().collect();
                    v.truncate(cols as usize);
                    while v.len() < cols as usize {
                        v.push('\0');
                    }
                    v
                })
                .collect();
            let n = rows.len();
            Self { rows, cols, cursor: (0, 0), soft_wrapped: vec![false; n] }
        }
    }
    impl CellSource for StrSource {
        fn cols(&self) -> u16 { self.cols }
        fn rows(&self) -> u16 { self.rows.len() as u16 }
        fn char_at(&self, col: u16, row: u16) -> char {
            self.rows[row as usize][col as usize]
        }
        fn is_soft_wrap_continuation(&self, row: u16) -> bool {
            self.soft_wrapped[row as usize]
        }
        fn cursor(&self) -> (u16, u16) { self.cursor }
        fn is_wide(&self, ch: char) -> bool {
            matches!(ch,
                '\u{1100}'..='\u{115F}' | '\u{2E80}'..='\u{A4CF}'
                | '\u{AC00}'..='\u{D7A3}' | '\u{F900}'..='\u{FAFF}'
                | '\u{FE30}'..='\u{FE4F}' | '\u{FF00}'..='\u{FF60}'
                | '\u{FFE0}'..='\u{FFE6}' | '\u{1F300}'..='\u{1FAFF}')
        }
    }

    /// 2026-07-28 field report: a URL inside a markdown table came
    /// out linked only as far as the first row of its cell.  The row
    /// merge asks whether the previous row ran out of room at the
    /// PANE edge; a table cell runs out at its own border, several
    /// columns short, with the border glyph itself sitting where the
    /// heuristic looks for the last character of the token.
    #[test]
    fn a_url_wrapped_across_one_table_cell_is_one_link() {
        // Two columns; the URL fills the right cell over three rows.
        // ASCII only: `StrSource` stores one char per column, so a
        // wide glyph here would misalign the rules against the rows
        // below it — a fixture artefact, not something a real grid
        // does (there the trail half occupies its own column).
        let src = StrSource::new(
            &[
                "│ file      │ Source link                     │",
                "│ detect.   │ https://raw.githubusercontent.c │",
                "│ caffemo   │ om/WeChatCV/opencv_3rdparty/a8b │",
                "│ del       │ 69ccc/detect.caffemodel         │",
                "├───────────┼─────────────────────────────────┤",
                "│ detect.p  │ same     /detect.prototxt       │",
            ],
            47,
        );
        let links = scan_visible_links(&src, ScanOpts { tui_mode: true });
        let full = "https://raw.githubusercontent.com/WeChatCV/opencv_3rdparty/a8b69ccc/detect.caffemodel";
        let url_rows: Vec<u16> = links
            .iter()
            .filter(|l| l.kind == LinkKind::Url && l.text == full)
            .map(|l| l.row)
            .collect();
        assert_eq!(
            url_rows,
            vec![1, 2, 3],
            "the URL should be one link underlined on all three of its rows, got {links:?}"
        );
        // …and the truncated prefix the row pass finds on row 1 must
        // not survive alongside it, or the click opens the wrong URL.
        let on_row_1: Vec<&str> = links
            .iter()
            .filter(|l| l.row == 1)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(on_row_1, vec![full], "row 1 must carry ONE link");

        // The underline stays inside the cell.  A multi-row span used
        // to run every row but its last out to the pane's right edge
        // — fine for a soft wrap, but here it drew straight through
        // the cell border and out the other side of the table.
        // Layout: `│ ` + 9-wide cell + ` │ ` + 31-wide cell + ` │`,
        // so the right cell's text lives in columns 14..=44 and its
        // border sits at 46.
        for l in links.iter().filter(|l| l.text == full) {
            assert!(
                l.col_start >= 14 && l.col_end <= 44,
                "row {} underlines {}..={}, outside the cell's 14..=44: {l:?}",
                l.row,
                l.col_start,
                l.col_end
            );
        }
    }

    /// The join is per cell, not per row.  Two table rows whose cells
    /// are both flush look exactly like one wrapped cell — which is
    /// what a column sized by its longest URL always looks like — so
    /// the fresh scheme on the second row is what has to stop it.
    #[test]
    fn table_cells_do_not_bleed_into_each_other() {
        let src = StrSource::new(
            &[
                "│ https://a.example/one │ plain words   │",
                "│ https://b.example/two │ more words    │",
            ],
            40,
        );
        let links = scan_visible_links(&src, ScanOpts { tui_mode: true });
        let mut texts: Vec<&str> = links.iter().map(|l| l.text.as_str()).collect();
        texts.sort_unstable();
        texts.dedup();
        assert_eq!(
            texts,
            vec!["https://a.example/one", "https://b.example/two"],
            "each cell keeps its own link, got {links:?}"
        );
    }

    /// Trait-path smoke: soft-wrap merge + tui_mode input-box
    /// exemption through `scan_visible_links` over a plain
    /// `StrSource` — the exact surface an external consumer sees.
    #[test]
    fn cellsource_scan_merges_soft_wrap_and_exempts_composer() {
        let mut src = StrSource::new(
            &[
                "see https://example.c",
                "om/long/path now ok  ",
                "                     ",
                "╭───────────────────╮",
                "│ > https://foo.com │",
                "╰───────────────────╯",
            ],
            21,
        );
        src.soft_wrapped[1] = true;
        src.cursor = (4, 4);
        let links = scan_visible_links(&src, ScanOpts { tui_mode: true });
        assert_eq!(links.len(), 2, "{links:?}"); // one URL fanned over 2 rows
        assert!(links.iter().all(|l| l.text == "https://example.com/long/path"));
        assert_eq!((links[0].row, links[1].row), (0, 1));
        // Without tui_mode the composer URL is scanned too.
        let all = scan_visible_links(&src, ScanOpts::default());
        assert!(all.iter().any(|l| l.text == "https://foo.com"), "{all:?}");
    }

    /// 2026-08-18 field report: the URL ending one bullet came out
    /// with the NEXT bullet's `-` glued to its tail.  The item above
    /// ends flush at the pane edge on a path character, and a nested
    /// item's indent is indistinguishable from a hanging wrap indent
    /// — geometry says "continuation", the marker says otherwise.
    #[test]
    fn a_following_list_marker_is_not_a_wrap_continuation() {
        let head = "- 村子 → http://192.168.50.20:6031/index.html?page=pages/village/index";
        let cols = head.chars().count() as u16; // flush at the edge
        for next in [
            "  - 阿云的屋 → http://a.example/x", // the field report, verbatim
            "- room -> http://a.example/x",      // no indent
            "  - room -> http://a.example/x",    // hanging-indent shape
            "  * room -> http://a.example/x",
            "  2. room -> http://a.example/x",
        ] {
            let src = StrSource::new(&[head, next], cols);
            let links = scan_visible_links(&src, ScanOpts { tui_mode: true });
            assert!(
                links.iter().any(|l| l.text
                    == "http://192.168.50.20:6031/index.html?page=pages/village/index"),
                "next={next:?} must not extend the bullet above, got {links:?}"
            );
        }
    }

    /// The marker gate must not cost the merge its real job: a row
    /// that resumes mid-token still joins, marker chars or not.
    #[test]
    fn a_mid_token_continuation_still_joins_after_the_marker_gate() {
        let head = "- 村子 → http://192.168.50.20:6031/index.html?page=pages/village/index";
        let cols = head.chars().count() as u16;
        let src = StrSource::new(&[head, "  -more/parts here"], cols);
        let links = scan_visible_links(&src, ScanOpts { tui_mode: true });
        assert!(
            links.iter().any(|l| l.text
                == "http://192.168.50.20:6031/index.html?page=pages/village/index-more/parts"),
            "a token tail that merely starts with `-` still belongs to the row above, got {links:?}"
        );
    }

    /// Three complete paths in a row, each ending a few columns shy
    /// of the right edge, must stay three links.
    ///
    /// 2026-08-19 field report: a `ls`-style listing inside a TUI (two
    /// spaces of indent, absolute paths) came out with the first and
    /// second entries unlinked and the third linked.  The merge is
    /// working as designed — a row ending within the path slack of the
    /// edge, followed by an indented row, is exactly the shape of a
    /// wrapped path — so the three rows become one logical line whose
    /// text is `…/a.sql/…/bb//…/ccc/`.  Nothing is wrong with that;
    /// the seam retry exists to take such a line apart again.
    ///
    /// What was wrong: the retry sat behind `looks_like_path(whole
    /// span)`, and the whole span is a concatenation of paths, which
    /// does not look like a path.  The guard therefore rejected
    /// precisely the input the retry was written for, and it had the
    /// right answer at every step without ever being asked.
    #[test]
    fn consecutive_near_flush_paths_stay_separate_links() {
        let dir = std::env::temp_dir().join("marspot-linkify-seam-rows");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bb")).expect("mkdir bb");
        std::fs::create_dir_all(dir.join("ccc")).expect("mkdir ccc");
        std::fs::write(dir.join("a.sql"), b"x").expect("write a.sql");
        let d = format!("{}/", dir.to_string_lossy());

        let rows = vec![
            format!("  {d}a.sql"),
            format!("  {d}bb/"),
            format!("  {d}ccc/"),
        ];
        // Two columns wider than the longest row: every row now ends
        // inside the slack, which is what triggers the merge.
        let cols = (rows.iter().map(|r| r.chars().count()).max().unwrap() + 2) as u16;
        let refs: Vec<&str> = rows.iter().map(|s| s.as_str()).collect();
        let src = StrSource::new(&refs, cols);

        let mut texts: Vec<String> = scan_visible_links(&src, ScanOpts { tui_mode: true })
            .into_iter()
            .map(|l| l.text)
            .collect();
        texts.sort();
        texts.dedup();
        assert_eq!(
            texts,
            vec![
                format!("{d}a.sql"),
                format!("{d}bb/"),
                format!("{d}ccc/"),
            ],
            "each row is a complete path and must keep its own link"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

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


    /// 2026-08-02 field report: a `⏺` summary ending
    /// `…/sentori-feedback-reply-b-section-2.md;memory 已记(…)` showed
    /// no link at all.  The path was real, the wrap merge was right —
    /// the sentence simply resumed straight after the `;`, so the
    /// scanner stat'ed `…-2.md;memory` and got nothing.  Chinese
    /// written with ASCII punctuation puts no space after the mark, so
    /// this is not an edge case in this codebase; it is most lines.
    #[test]
    fn prose_resuming_after_a_mark_does_not_swallow_the_path() {
        let bin = std::env::current_exe().unwrap();
        let p = bin.display().to_string();
        for line in [
            format!("- 回执:{p};memory 已记"),
            format!("- 回执:{p},另见下条"),
            format!("见 {p}!下一条"),
        ] {
            let v = scan(&line);
            let files: Vec<&LinkRange> = v.iter().filter(|r| r.kind == LinkKind::File).collect();
            assert_eq!(files.len(), 1, "no link in {line:?}");
            assert_eq!(files[0].text, p, "wrong span in {line:?}");
        }
    }

    /// …and the mark is only a *candidate* end.  A filename that
    /// really contains one is legal, and the full span is stat'ed
    /// before any cut is tried, so it still wins outright.
    #[test]
    fn a_filename_that_really_contains_a_mark_still_wins() {
        let p = std::env::temp_dir().join(format!(
            "marspot-linkify-a;b,c-{}.txt",
            std::process::id()
        ));
        std::fs::write(&p, b"x").unwrap();
        let text = p.display().to_string();
        let v = scan(&format!("see {text} today"));
        let files: Vec<&LinkRange> = v.iter().filter(|r| r.kind == LinkKind::File).collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].text, text, "the whole name, marks and all");
        let _ = std::fs::remove_file(&p);
    }

    /// 2026-08-05 report: a Japanese document path went unlinked —
    /// and the design lesson behind the fix.
    ///
    /// `株主総会議事録（役員報酬改定・20260805）.pdf`.  The scan used to
    /// stop at `（`, because CJK prose glues brackets onto paths
    /// constantly and a filename containing them was judged rarer
    /// (宁可漏不可错).  But that trade only makes sense for a scanner
    /// that cannot check its answer, and this one can: **a path link
    /// is only emitted for a path that exists.**  Guessing narrow
    /// could therefore only ever lose true positives.
    ///
    /// So the scan is greedy and the filesystem decides.  This test
    /// pins the reported case; `punctuation_inside_a_real_name_is_not
    /// _a_terminator` pins the principle it is an instance of.
    #[test]
    fn a_filename_with_fullwidth_brackets_is_one_link() {
        let dir = std::env::temp_dir().join(format!(
            "marspot-linkify-brackets-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("株主総会議事録（役員報酬改定・20260805）.pdf");
        std::fs::write(&p, b"x").unwrap();
        let text = p.display().to_string();

        // Exactly the reported line: prose in ASCII parens glued to
        // the end, a fullwidth stop after that.
        let v = scan(&format!("一页收好了。{text}(docx 同目录同名)。"));
        let files: Vec<&LinkRange> = v.iter().filter(|r| r.kind == LinkKind::File).collect();
        assert_eq!(files.len(), 1, "no link in the reported line");
        assert_eq!(files[0].text, text, "the whole name, brackets and all");

        // The same shape naming a file that does NOT exist stays
        // unlinked — the extension is not a licence to guess.
        let ghost = dir.join("株主総会議事録（不存在）.pdf");
        let v = scan(&format!("见 {}", ghost.display()));
        assert!(
            v.iter().all(|r| r.kind != LinkKind::File),
            "a bracket group that names nothing must not become a link"
        );

        // And a genuine prose parenthetical after a real directory
        // still links only the directory.
        let v = scan(&format!("见 {}（说明）的东西", dir.display()));
        let files: Vec<&LinkRange> = v.iter().filter(|r| r.kind == LinkKind::File).collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].text, dir.display().to_string(), "prose stays prose");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2026-08-07 report: `http://localhost:6014/tools/documents` was
    /// not a link.
    ///
    /// The host filter demanded a dot, so every dev-server URL a
    /// terminal is full of failed it.  The rule was *written* as "no
    /// dot, no digits" and *implemented* as "no dot" — the digits half
    /// had never been there.
    ///
    /// There is no filesystem oracle for URLs, so the structure has to
    /// carry the decision — here, an explicit numeric port.  That is
    /// what separates a real address from the `https://x` placeholders
    /// this filter rejects, and it is all this report needs: bare
    /// `https://localhost` is left rejected, as `url_without_dot_in
    /// _host_rejected` settled.
    #[test]
    fn a_dev_server_url_is_a_link_and_a_placeholder_is_not() {
        for good in [
            "http://localhost:6014/tools/documents",
            "http://localhost:3000",
            "http://myserver:8080/api",
            "http://127.0.0.1:6014/x",
            "https://example.com/a",
        ] {
            let v = scan(&format!("打开 {good} 看看"));
            let urls: Vec<&LinkRange> =
                v.iter().filter(|r| r.kind == LinkKind::Url).collect();
            assert_eq!(urls.len(), 1, "no link for {good:?}: {v:?}");
            assert_eq!(urls[0].text, good);
        }
        for bad in [
            "https://x",
            "https://foo",
            // Settled separately, and left alone: without a port this
            // is the shape chat examples take.
            "https://localhost",
            // The placeholder people actually write — `port` is not a
            // number, so it is still not an address.
            "http://host:port",
            "https://.",
            "https://-",
        ] {
            let v = scan(&format!("比如 {bad} 之类"));
            assert!(
                v.iter().all(|r| r.kind != LinkKind::Url),
                "{bad:?} must stay unlinked: {v:?}"
            );
        }
    }

    /// The principle, not the instance: **no punctuation terminates a
    /// path**, because the filesystem is a better judge than any table
    /// of characters.
    ///
    /// Each of these names contains a character the old scan treated
    /// as a hard stop, so none of them could be linked at all — the
    /// span was cut inside the name, the prefix did not exist, and the
    /// line lost its link.  Each is also followed by prose glued on
    /// with the *same* character, which is the case that table was
    /// protecting against; the greedy scan gets both right by asking.
    #[test]
    fn punctuation_inside_a_real_name_is_not_a_terminator() {
        let dir = std::env::temp_dir().join(format!(
            "marspot-linkify-punct-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // (name, the prose glued straight onto it)
        let cases = [
            ("plan(v2).md", "、然后再说"),
            ("a,b.txt", ",这句话继续"),
            ("sec；1.md", ";memory 已记"),
            ("note【草稿】.md", "。下一步"),
        ];
        for (name, tail) in cases {
            let p = dir.join(name);
            std::fs::write(&p, b"x").unwrap();
            let text = p.display().to_string();
            let v = scan(&format!("见 {text}{tail}"));
            let files: Vec<&LinkRange> =
                v.iter().filter(|r| r.kind == LinkKind::File).collect();
            assert_eq!(files.len(), 1, "no link for {name:?}");
            assert_eq!(
                files[0].text, text,
                "{name:?} lost part of its name to the prose after it"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A name with a space in it, spelled the way a shell spells it.
    ///
    /// 2026-08-09 report: `rm -rf ~/Library/Safari/"Favicon Cache"`
    /// linked nothing.  The scan stopped at the quote, so the
    /// candidate was `~/Library/Safari/` — a real directory, but not
    /// the thing the line points at, and not what the user was
    /// pointing their cursor at either.
    ///
    /// Both spellings are exercised, and both halves of the contract:
    /// the **span** covers what is drawn (quotes and all — that is
    /// what the user clicks), the **text** is what opens.
    #[test]
    fn a_quoted_or_escaped_space_is_part_of_the_name() {
        let dir = std::env::temp_dir().join(format!(
            "marspot-linkify-space-{}",
            std::process::id()
        ));
        let inner = dir.join("Favicon Cache");
        std::fs::create_dir_all(&inner).unwrap();
        let real = inner.display().to_string();
        let parent = dir.display().to_string();

        // Each spelling, and what the drawn text is.
        let drawn = [
            format!("{parent}/\"Favicon Cache\""),
            format!("{parent}/'Favicon Cache'"),
            format!("{parent}/Favicon\\ Cache"),
        ];
        for d in drawn {
            let line = format!("rm -rf {d}");
            let v = scan(&line);
            let files: Vec<&LinkRange> =
                v.iter().filter(|r| r.kind == LinkKind::File).collect();
            assert_eq!(files.len(), 1, "no link for {d:?}");
            assert_eq!(
                files[0].text, real,
                "{d:?} must open the real path, not the quoted spelling"
            );
            // The span ends where the drawn text ends — including the
            // closing quote, so the whole thing underlines rather than
            // stopping mid-name.
            let last = line.chars().count() as u16 - 1;
            assert!(
                files[0].col_end >= last,
                "{d:?}: underline stops at col {} but the name runs to {last}",
                files[0].col_end
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unpaired quote is prose, not the start of a name — and the
    /// arbitration has to be able to back out of a pairing that
    /// swallowed too much.
    #[test]
    fn an_unpaired_quote_does_not_swallow_the_line() {
        let dir = std::env::temp_dir().join(format!(
            "marspot-linkify-quote-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.display().to_string();
        // The directory is real; the quote after it opens nothing.
        let v = scan(&format!("cd {d}/\" then he said \"hello\" and left"));
        let files: Vec<&LinkRange> =
            v.iter().filter(|r| r.kind == LinkKind::File).collect();
        for f in &files {
            assert!(
                !f.text.contains("hello"),
                "a quote pairing swallowed the sentence: {:?}",
                f.text
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The greedy scan runs on the render path, so it comes with a
    /// ceiling: a token that is nothing but cut points must not turn
    /// into a `stat` storm, and must still link nothing.
    #[test]
    fn a_token_of_pure_punctuation_is_bounded_and_links_nothing() {
        let junk: String = std::iter::repeat("/a、").take(200).collect();
        let v = scan(&format!("见 {junk} 完"));
        assert!(
            v.iter().all(|r| r.kind != LinkKind::File),
            "nothing here exists, so nothing here is a link"
        );
    }

    /// The field report's actual shape: the path hard-wrapped at the
    /// pane edge *and* the sentence resumed after the `;`.  Both
    /// repairs have to hold at once — the merge to reach the second
    /// row, the cut to drop what follows.
    #[test]
    fn a_wrapped_path_with_prose_glued_to_its_end_is_one_link() {
        let p = std::env::temp_dir().join(format!(
            "marspot-linkify-wrapped-reply-b-section-2-{}.md",
            std::process::id()
        ));
        std::fs::write(&p, b"x").unwrap();
        let text = p.display().to_string();
        // Split the path so the first row ends flush at the right
        // edge, which is what claudecode's fixed-width wrap does.
        let split = text.len() - 12;
        let head = format!("  - note:{}", &text[..split]);
        let cols = head.chars().count() as u16;
        let tail = format!("  {};memory noted", &text[split..]);
        let src = StrSource::new(&[&head, &tail], cols);
        let links = scan_visible_links(&src, ScanOpts { tui_mode: true });
        let files: Vec<&LinkRange> = links.iter().filter(|l| l.kind == LinkKind::File).collect();
        assert!(!files.is_empty(), "wrapped path found no link at all");
        assert!(
            files.iter().all(|l| l.text == text),
            "every segment carries the whole path: {:?}",
            files.iter().map(|l| &l.text).collect::<Vec<_>>()
        );
        // One segment per physical row it crosses.
        assert_eq!(files.len(), 2, "both rows underline");
        let _ = std::fs::remove_file(&p);
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
                cc_zero_indent: false,
            },
            super::LineSegment {
                phys_row: 1,
                char_offset: 10,
                cc_zero_indent: false,
            },
        ];
        let mut out = Vec::new();
        // col_map: chars 0..9 → row-0 cols 0..9; chars 10..12 → row-1 cols 0..2.
        let col_map: Vec<u16> = (0..10).chain(0..3).collect();
        let chars: Vec<char> = line.chars().collect();
        super::scan_line_into_matches(&chars, &col_map, &mut out, &segments, 10, &super::FsOracle);
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
            cc_zero_indent: false,
        }];
        let mut out = Vec::new();
        let chars: Vec<char> = line.chars().collect();
        let col_map: Vec<u16> = (0..chars.len() as u16).collect();
        super::scan_line_into_matches(
            &chars,
            &col_map,
            &mut out,
            &segments,
            chars.len(),
            &super::FsOracle,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].row, 7);
        assert_eq!(out[0].kind, LinkKind::Url);
        assert_eq!(out[0].text, "https://example.com");
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


    /// 2026-07-16 field ask: bare IPv4 must be recognised as its own
    /// `Ip` kind — OpenLink prepends `http://` (defaults to port 80),
    /// Copy keeps the raw displayed text.  Cover the common shapes:
    /// bare, `:port`, `/path`, both.
    #[test]
    fn bare_ipv4_recognised_as_ip() {
        for (line, expected) in [
            ("connect 47.96.114.231 now", "47.96.114.231"),
            ("admin 192.168.1.1:8080/status ok", "192.168.1.1:8080/status"),
            ("dashboard 10.0.0.5:3000", "10.0.0.5:3000"),
            ("see 8.8.8.8/dns page", "8.8.8.8/dns"),
        ] {
            let mut v = Vec::new();
            scan_line(line, 0, &mut v);
            let ips: Vec<_> = v.iter().filter(|l| l.kind == LinkKind::Ip).collect();
            assert_eq!(ips.len(), 1, "line {line:?}: {v:?}");
            assert_eq!(ips[0].text, expected, "line {line:?}");
        }
    }


    /// URL always wins over Ip — if `http://…` is present, the URL
    /// scanner consumes the string first and the IP scanner never
    /// sees the digits (per user: "如果他形成了 url 就以 url 为准").
    #[test]
    fn url_with_scheme_wins_over_ip() {
        let mut v = Vec::new();
        scan_line("visit http://47.96.114.231:8080/foo done", 0, &mut v);
        assert_eq!(v.len(), 1, "{v:?}");
        assert_eq!(v[0].kind, LinkKind::Url);
        assert_eq!(v[0].text, "http://47.96.114.231:8080/foo");
    }


    /// IPv4 recogniser must NOT swallow version strings, other
    /// identifiers, or IPs inside an `http://…` URL's tail.
    #[test]
    fn ipv4_rejects_version_strings_and_url_tails() {
        for line in [
            "runtime v1.2.3.4 released",       // preceded by 'v'
            "kernel 1.2.3.4-rc1 in test",      // trailing -rc1
            "matrix 1.2.3.4.5 dot",            // 5th component
            "big 999.1.1.1 not valid",         // octet > 255
            "cargo pkg 1.0.23 pinned",         // not 4 octets
            "leading 010.0.0.1 zero rejected", // leading zero
        ] {
            let mut v = Vec::new();
            scan_line(line, 0, &mut v);
            let ips: Vec<_> = v.iter().filter(|l| l.kind == LinkKind::Ip).collect();
            assert!(
                ips.is_empty(),
                "line {line:?} must not produce a bare-IP: {v:?}"
            );
        }
    }


    /// Prose-wrapping parens around the IP get stripped by the same
    /// balanced-paren trim as URLs.
    #[test]
    fn ipv4_in_prose_parens_trimmed() {
        let mut v = Vec::new();
        scan_line("(see 47.96.114.231:8080)", 0, &mut v);
        let ips: Vec<_> = v.iter().filter(|l| l.kind == LinkKind::Ip).collect();
        assert_eq!(ips.len(), 1, "{v:?}");
        assert_eq!(ips[0].text, "47.96.114.231:8080");
    }


    /// IPv6 — the three shapes we support: bracketed with port/path,
    /// bare with `::` compression, bare full 8-group form.
    #[test]
    fn ipv6_recognised_as_ip() {
        for (line, expected) in [
            ("localhost ::1 test", "::1"),
            ("connect 2001:db8::1 done", "2001:db8::1"),
            ("bracketed [fe80::1]:8080/foo now", "[fe80::1]:8080/foo"),
            ("bracketed [::1]:22 ssh", "[::1]:22"),
            (
                "full 2001:0db8:85a3:0000:0000:8a2e:0370:7334 ipv6",
                "2001:0db8:85a3:0000:0000:8a2e:0370:7334",
            ),
        ] {
            let mut v = Vec::new();
            scan_line(line, 0, &mut v);
            let ips: Vec<_> = v.iter().filter(|l| l.kind == LinkKind::Ip).collect();
            assert_eq!(ips.len(), 1, "line {line:?}: {v:?}");
            assert_eq!(ips[0].text, expected, "line {line:?}");
        }
    }


    /// Time-of-day, hex identifiers, and other colon-bearing tokens
    /// must NOT match as IPv6 — no `::`, not 8 groups → reject.
    #[test]
    fn ipv6_rejects_time_and_hex_ids() {
        for line in [
            "logged 12:34:56 now",             // time
            "hex abc:def:123 label",           // 3 groups, no ::
            "sha 1a2b3c4d:5e6f7a8b thing",     // 2 groups
            "commit deadbeef today",           // no colon
        ] {
            let mut v = Vec::new();
            scan_line(line, 0, &mut v);
            let ips: Vec<_> = v.iter().filter(|l| l.kind == LinkKind::Ip).collect();
            assert!(ips.is_empty(), "line {line:?}: {v:?}");
        }
    }


    /// UUID canonical form — hex + dashes at fixed positions.
    #[test]
    fn uuid_recognised_as_uuid() {
        for line in [
            "session 550e8400-e29b-41d4-a716-446655440000 opened",
            "trace-id: f47ac10b-58cc-4372-a567-0e02b2c3d479 end",
            "start 00000000-0000-0000-0000-000000000000 nil",
        ] {
            let mut v = Vec::new();
            scan_line(line, 0, &mut v);
            let uuids: Vec<_> = v.iter().filter(|l| l.kind == LinkKind::Uuid).collect();
            assert_eq!(uuids.len(), 1, "line {line:?}: {v:?}");
            assert_eq!(uuids[0].text.len(), 36, "line {line:?}");
        }
    }


    /// UUID reject: wrong segment lengths, non-hex chars, or
    /// left-boundary-inside-identifier.
    #[test]
    fn uuid_rejects_wrong_shapes() {
        for line in [
            "abc123-def456-789012-345678-901234567890 wrong-lens", // 6-6-6-6-12
            "session id-550e8400-e29b-41d4-a716-446655440000 inside", // preceded by '-'
            "not 550e8400e29b41d4a716446655440000 dashless",       // no dashes
            "gh 550e8400-e29b-41d4-a716-44665544000g bad-hex",     // 'g' not hex
        ] {
            let mut v = Vec::new();
            scan_line(line, 0, &mut v);
            let uuids: Vec<_> = v.iter().filter(|l| l.kind == LinkKind::Uuid).collect();
            assert!(uuids.is_empty(), "line {line:?}: {v:?}");
        }
    }


    /// A uuid followed by `/` is a path component, never a Uuid link;
    /// a standalone uuid still links.
    #[test]
    fn uuid_component_in_path_rejected_standalone_still_links() {
        let mut v = Vec::new();
        scan_line("id 3dbde79c-ab6e-43bd-8143-c448617e1d69/scratch x", 0, &mut v);
        assert!(
            v.iter().all(|l| l.kind != LinkKind::Uuid),
            "{v:?}"
        );
        let mut v2 = Vec::new();
        scan_line("id 3dbde79c-ab6e-43bd-8143-c448617e1d69 done", 0, &mut v2);
        assert_eq!(
            v2.iter().filter(|l| l.kind == LinkKind::Uuid).count(),
            1,
            "{v2:?}"
        );
    }
}



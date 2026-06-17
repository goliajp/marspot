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
pub fn scan_visible_links(grid: &Grid, view_offset: u16) -> Vec<LinkRange> {
    let mut out = Vec::new();
    let rows = grid.rows();
    let cols = grid.cols();
    if rows == 0 || cols == 0 {
        return out;
    }
    // Build logical lines by walking viewport rows top→bottom and
    // joining each row that's flagged as a soft-wrap continuation of
    // the row above it.  We keep enough info to project a char index
    // inside the logical line back to its physical (row, col).
    let mut line = String::with_capacity(cols as usize * 4);
    // Per-row segment: where in `line` does row `phys_row`'s chars
    // start, and what physical col does it start at (always 0 for
    // continuation rows, only the first segment can start mid-row —
    // but here it always starts at col 0 because we read whole rows).
    let mut segments: Vec<LineSegment> = Vec::with_capacity(8);
    for r in 0..rows {
        let is_continuation = r > 0 && grid.wrapped_at_view(view_offset, r);
        if !is_continuation && !segments.is_empty() {
            scan_logical_line(&line, &segments, out.as_mut());
            line.clear();
            segments.clear();
        }
        segments.push(LineSegment {
            phys_row: r,
            char_offset: line.chars().count(),
        });
        for c in 0..cols {
            let ch = grid.cell_at_view(view_offset, c, r).ch;
            line.push(if ch == '\0' || ch == ' ' { ' ' } else { ch });
        }
    }
    if !segments.is_empty() {
        scan_logical_line(&line, &segments, out.as_mut());
    }
    out
}

/// One physical row's contribution to a logical (post-soft-wrap-merge)
/// line.  `phys_row` is the viewport row the chars came from;
/// `char_offset` is where in the merged `line` String this row's chars
/// start (in `chars().count()` units, NOT bytes — pattern scanning is
/// char-indexed throughout).  Each row contributes exactly `cols`
/// chars (the loop writes one char per grid cell).
struct LineSegment {
    phys_row: u16,
    char_offset: usize,
}

/// Scan a logical (possibly multi-row-merged) line and emit
/// `LinkRange`s, one per **physical row** the match touches.  Single-
/// segment matches collapse to one LinkRange; multi-segment matches
/// fan out (same `text`, different `phys_row`/col_start/col_end),
/// preserving the per-row hit-test + per-row underline model.
fn scan_logical_line(line: &str, segments: &[LineSegment], out: &mut Vec<LinkRange>) {
    if segments.is_empty() {
        return;
    }
    // Pattern scan produces matches as `(char_lo, char_hi_exclusive,
    // kind, text)`; project each to one or more LinkRange row spans.
    let mut row_matches: Vec<LinkRange> = Vec::new();
    scan_line_into_matches(line, &mut row_matches, segments);
    out.extend(row_matches.drain(..));
}

/// Char-pos → (phys_row, col) projector.  Assumes each segment has
/// exactly `cols` chars (true: outer loop always writes one char per
/// grid cell).  Linear over the small `segments` slice — O(N segments)
/// per lookup, but in practice N ≤ 4 even for very wrapped URLs.
fn locate(segments: &[LineSegment], char_pos: usize, cols_per_row: usize) -> Option<(u16, u16)> {
    for (i, seg) in segments.iter().enumerate() {
        let next_off = segments
            .get(i + 1)
            .map(|s| s.char_offset)
            .unwrap_or(usize::MAX);
        if char_pos < next_off {
            let col = char_pos - seg.char_offset;
            if col < cols_per_row {
                return Some((seg.phys_row, col as u16));
            }
        }
    }
    None
}

/// Original pattern scanner, but the per-row emit step now consults
/// `segments` to fan a multi-row match out into one LinkRange per
/// physical row it covers.  Each per-row LinkRange carries the FULL
/// matched `text` (so click dispatch + hover tooltip are identical
/// across segments).
fn scan_line_into_matches(line: &str, out: &mut Vec<LinkRange>, segments: &[LineSegment]) {
    if segments.is_empty() {
        return;
    }
    // cols-per-row: the contribution width of every segment.  Derive
    // from segment offsets so we don't have to thread `cols` through.
    let cols_per_row = if segments.len() >= 2 {
        segments[1].char_offset - segments[0].char_offset
    } else {
        line.chars().count()
    };
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let c = chars[i];

        // URL: http:// or https://
        if matches_prefix(&chars, i, "http://") || matches_prefix(&chars, i, "https://") {
            let end = scan_until_link_terminator(&chars, i);
            let span = &chars[i..end];
            if looks_like_url(span) {
                let text: String = span.iter().collect();
                emit_match(out, segments, cols_per_row, i, end, LinkKind::Url, text);
                i = end;
                continue;
            }
        }

        if c == '/' && i + 1 < n && !is_left_boundary_alnum(&chars, i) {
            let end = scan_until_link_terminator(&chars, i);
            let span = &chars[i..end];
            if looks_like_path(span) {
                let text: String = span.iter().collect();
                if is_real_path(&text) {
                    emit_match(out, segments, cols_per_row, i, end, LinkKind::File, text);
                    i = end;
                    continue;
                }
            }
        }

        if c == '~' && i + 1 < n && chars[i + 1] == '/' && !is_left_boundary_alnum(&chars, i) {
            let end = scan_until_link_terminator(&chars, i);
            let span = &chars[i..end];
            if span.len() >= 3 && looks_like_path(span) {
                let text: String = span.iter().collect();
                if is_real_path(&text) {
                    emit_match(out, segments, cols_per_row, i, end, LinkKind::File, text);
                    i = end;
                    continue;
                }
            }
        }

        if c == '@' && i > 0 && i + 1 < n {
            let local_start = scan_back_local(&chars, i);
            let host_end = scan_forward_host(&chars, i + 1);
            if local_start < i && host_end > i + 1 && has_dot_in(&chars, i + 1, host_end) {
                let text: String = chars[local_start..host_end].iter().collect();
                emit_match(
                    out,
                    segments,
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

/// Project one `[char_lo, char_hi)` match onto the physical rows it
/// touches and push one `LinkRange` per row.  Single-row matches turn
/// into one LinkRange (unchanged from the legacy per-row scanner);
/// matches spanning N rows turn into N LinkRanges with the same
/// `text` and matching per-row col spans.
fn emit_match(
    out: &mut Vec<LinkRange>,
    segments: &[LineSegment],
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
        match locate(segments, char_lo, cols_per_row) {
            Some(v) => v,
            None => return,
        };
    let (end_row, end_col) =
        match locate(segments, char_hi_exclusive - 1, cols_per_row) {
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
    // edge; middle rows run the full width; the last row runs from 0
    // to `end_col`.  Every LinkRange carries the FULL text so click
    // dispatch is identical regardless of which segment was clicked.
    let last_col = cols_per_row.saturating_sub(1) as u16;
    out.push(LinkRange {
        row: start_row,
        col_start: start_col,
        col_end: last_col,
        kind,
        text: text.clone(),
    });
    for mid in (start_row + 1)..end_row {
        out.push(LinkRange {
            row: mid,
            col_start: 0,
            col_end: last_col,
            kind,
            text: text.clone(),
        });
    }
    out.push(LinkRange {
        row: end_row,
        col_start: 0,
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
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let c = chars[i];

        // URL: http:// or https://
        if matches_prefix(&chars, i, "http://") || matches_prefix(&chars, i, "https://") {
            let end = scan_until_link_terminator(&chars, i);
            let span = &chars[i..end];
            if looks_like_url(span) {
                let text: String = span.iter().collect();
                out.push(LinkRange {
                    row,
                    col_start: i as u16,
                    col_end: (end - 1) as u16,
                    kind: LinkKind::Url,
                    text,
                });
                i = end;
                continue;
            }
        }

        // Absolute path: `/` not adjacent to an alnum on the left
        // (rules out e.g. `cargo/Cargo.toml` mid-word `/`).  Pattern
        // must look like a real path AND `stat()` (or its cached
        // verdict) must say it exists — otherwise a bare `//`,
        // `//./`, or `/something_that_does_not_exist` is left as
        // plain text instead of inviting a wasted click.
        if c == '/' && i + 1 < n && !is_left_boundary_alnum(&chars, i) {
            let end = scan_until_link_terminator(&chars, i);
            let span = &chars[i..end];
            if looks_like_path(span) {
                let text: String = span.iter().collect();
                if is_real_path(&text) {
                    out.push(LinkRange {
                        row,
                        col_start: i as u16,
                        col_end: (end - 1) as u16,
                        kind: LinkKind::File,
                        text,
                    });
                    i = end;
                    continue;
                }
            }
        }

        // Home-relative path: `~/...` — same stat-check.
        if c == '~' && i + 1 < n && chars[i + 1] == '/' && !is_left_boundary_alnum(&chars, i) {
            let end = scan_until_link_terminator(&chars, i);
            let span = &chars[i..end];
            if span.len() >= 3 && looks_like_path(span) {
                let text: String = span.iter().collect();
                if is_real_path(&text) {
                    out.push(LinkRange {
                        row,
                        col_start: i as u16,
                        col_end: (end - 1) as u16,
                        kind: LinkKind::File,
                        text,
                    });
                    i = end;
                    continue;
                }
            }
        }

        // Email: <local>@<host>.<tld>.  Backtrack from a candidate
        // `@` so we don't have to scan forward looking for the start.
        if c == '@' && i > 0 && i + 1 < n {
            let local_start = scan_back_local(&chars, i);
            let host_end = scan_forward_host(&chars, i + 1);
            if local_start < i && host_end > i + 1 && has_dot_in(&chars, i + 1, host_end) {
                let text: String = chars[local_start..host_end].iter().collect();
                out.push(LinkRange {
                    row,
                    col_start: local_start as u16,
                    col_end: (host_end - 1) as u16,
                    kind: LinkKind::Email,
                    text,
                });
                i = host_end;
                continue;
            }
        }

        i += 1;
    }
}

/// True when `chars[start..]` begins with `prefix`.
fn matches_prefix(chars: &[char], start: usize, prefix: &str) -> bool {
    let pchars: Vec<char> = prefix.chars().collect();
    if chars.len() < start + pchars.len() {
        return false;
    }
    for (i, p) in pchars.iter().enumerate() {
        if chars[start + i] != *p {
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

/// Walk forward from `start` until we hit a character that almost
/// certainly terminates a link span.  Conservative: stop at
/// whitespace, control chars, balanced-pair closers, common
/// punctuation that follows links in prose.
fn scan_until_link_terminator(chars: &[char], start: usize) -> usize {
    let mut i = start;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() || c == '\0' || (c.is_control() && c != '\t') {
            break;
        }
        // Hard terminators that almost never belong inside a link.
        if matches!(c, '<' | '>' | '"' | '\'' | '`' | '|') {
            break;
        }
        i += 1;
    }
    // Trim trailing punctuation that usually belongs to surrounding
    // prose, not the link itself.  Repeat — sentences end with
    // `...".`, `link).`, etc.
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

fn has_dot_in(chars: &[char], lo: usize, hi: usize) -> bool {
    chars[lo..hi].iter().any(|c| *c == '.')
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
            },
            super::LineSegment {
                phys_row: 1,
                char_offset: 10,
            },
        ];
        let mut out = Vec::new();
        super::scan_line_into_matches(line, &mut out, &segments);
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
        }];
        let mut out = Vec::new();
        super::scan_line_into_matches(line, &mut out, &segments);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].row, 7);
        assert_eq!(out[0].kind, LinkKind::Url);
        assert_eq!(out[0].text, "https://example.com");
    }
}

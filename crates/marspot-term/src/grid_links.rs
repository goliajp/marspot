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
//! when it isn't).

use crate::grid::Grid;

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
pub fn scan_visible_links(grid: &Grid, view_offset: u16) -> Vec<LinkRange> {
    let mut out = Vec::new();
    let rows = grid.rows();
    let cols = grid.cols();
    let mut line = String::with_capacity(cols as usize);
    for r in 0..rows {
        line.clear();
        for c in 0..cols {
            let ch = grid.cell_at_view(view_offset, c, r).ch;
            line.push(if ch == '\0' || ch == ' ' { ' ' } else { ch });
        }
        scan_line(&line, r, &mut out);
    }
    out
}

/// Scan one row's joined text.  Char-indexed; the col positions we
/// emit are 1:1 with the line chars because the caller built `line`
/// with one char per grid cell.
fn scan_line(line: &str, row: u16, out: &mut Vec<LinkRange>) {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let c = chars[i];

        // URL: http:// or https://
        if matches_prefix(&chars, i, "http://") || matches_prefix(&chars, i, "https://") {
            let end = scan_until_link_terminator(&chars, i);
            if end - i >= 10 {
                // shortest plausible URL is http://x.y → 10 chars
                let text: String = chars[i..end].iter().collect();
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
        // (rules out e.g. `cargo/Cargo.toml` mid-word `/`), and the
        // span must include at least one more `/` or a `.` so we
        // don't decorate a bare `/`.
        if c == '/' && i + 1 < n && !is_left_boundary_alnum(&chars, i) {
            let end = scan_until_link_terminator(&chars, i);
            if end > i + 1 && looks_like_path(&chars[i..end]) {
                let text: String = chars[i..end].iter().collect();
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

        // Home-relative path: `~/...`
        if c == '~' && i + 1 < n && chars[i + 1] == '/' && !is_left_boundary_alnum(&chars, i) {
            let end = scan_until_link_terminator(&chars, i);
            if end > i + 2 {
                let text: String = chars[i..end].iter().collect();
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

/// Does this slice look enough like a path to decorate?  Avoids the
/// "bare `/`" and "single-letter `/x`" false positives.
fn looks_like_path(chars: &[char]) -> bool {
    if chars.len() < 2 {
        return false;
    }
    // Must contain at least one extra `/` or a `.` to qualify.
    chars[1..].iter().any(|c| *c == '/' || *c == '.')
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

    #[test]
    fn url_https() {
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
    fn absolute_path() {
        let v = scan("see /Users/x/foo.txt for details");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, LinkKind::File);
        assert_eq!(v[0].text, "/Users/x/foo.txt");
    }

    #[test]
    fn home_path() {
        let v = scan("at ~/.zshrc line 5");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, LinkKind::File);
        assert_eq!(v[0].text, "~/.zshrc");
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
        assert_eq!(v.len(), 0);
    }

    #[test]
    fn email() {
        let v = scan("ping lihao@golia.jp today");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, LinkKind::Email);
        assert_eq!(v[0].text, "lihao@golia.jp");
    }

    #[test]
    fn email_at_alone_is_not_an_email() {
        let v = scan("hey @everyone");
        assert_eq!(v.len(), 0);
    }

    #[test]
    fn email_trims_trailing_period() {
        let v = scan("contact foo@bar.com.");
        assert_eq!(v[0].text, "foo@bar.com");
    }

    #[test]
    fn url_path_email_mixed() {
        let v = scan(
            "visit https://x.com or read /etc/hosts or ping a@b.co",
        );
        assert_eq!(v.len(), 3);
        assert!(v.iter().any(|r| r.kind == LinkKind::Url));
        assert!(v.iter().any(|r| r.kind == LinkKind::File));
        assert!(v.iter().any(|r| r.kind == LinkKind::Email));
    }

    #[test]
    fn empty_line_emits_nothing() {
        assert!(scan("").is_empty());
        assert!(scan("                ").is_empty());
    }
}

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

mod addr;
mod email;
mod input_box;
mod line;
mod oracle;
mod path;
mod seam;
mod sink;
mod table;
mod url;
mod wrap;

pub use oracle::{FsOracle, NoFsOracle, PathOracle, PathVerdict, expand_user_path};
pub use sink::LinkScratch;

use input_box::find_input_box_rows;
use line::{LineSegment, scan_line_into_matches, scan_logical_line};
use sink::{Sink, TableScratch, TextBufs};
use table::{drop_shadowed_ranges, is_vertical_rule, scan_table_cells};
use wrap::{count_leading_ws, is_hard_wrap_continuation};

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
    /// What the span actually names, when that is not what is drawn.
    ///
    /// A relative path is written `src/main.rs` and means
    /// `<cwd>/src/main.rs`; the underline has to cover the four
    /// characters the user can see, and Open / Copy have to act on the
    /// file.  `None` when the two are the same.
    pub target: Option<String>,
}

/// Options that tune `scan_visible_links` for the calling pane.  All
/// fields default to off so the existing call path stays opt-in.
#[derive(Default, Clone, Copy, Debug)]
pub struct ScanOpts<'a> {
    /// The pane's working directory, when it is known.  Without it a
    /// relative path cannot be resolved and none are matched — a bare
    /// `src/main.rs` names nothing on its own.
    pub cwd: Option<&'a str>,
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
/// → empty Vec.  Allocates its buffers afresh on every call; the
/// per-frame path is [`scan_visible_links_into`], which reuses them.
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
    scan_visible_links_into(src, opts, oracle, &mut LinkScratch::default(), &mut out);
    #[cfg(test)]
    tests::check_reuse(src, opts, oracle, &out);
    out
}

/// [`scan_visible_links_with`] into caller-owned storage: the result
/// replaces the contents of `out`, and the ranges already in it are
/// overwritten in place, so a scan of similar content on the next
/// frame reuses every string it needs.  This is the per-frame path.
pub fn scan_visible_links_into<S: CellSource>(
    src: &S,
    opts: ScanOpts,
    oracle: &dyn PathOracle,
    scratch: &mut LinkScratch,
    out: &mut Vec<LinkRange>,
) {
    let LinkScratch { chars, col_map, segments, rule_cols, table, text, targets, spare } = scratch;
    out.append(spare);
    let mut sink = Sink { out, len: 0, targets };
    scan_into(src, opts, oracle, chars, col_map, segments, rule_cols, table, text, &mut sink);
    let len = sink.len;
    spare.extend(out.drain(len..));
}

#[allow(clippy::too_many_arguments)]
fn scan_into<S: CellSource>(
    src: &S,
    opts: ScanOpts,
    oracle: &dyn PathOracle,
    chars: &mut Vec<char>,
    col_map: &mut Vec<u16>,
    segments: &mut Vec<LineSegment>,
    rule_cols: &mut Vec<(u16, u16)>,
    table: &mut TableScratch,
    bufs: &mut TextBufs,
    out: &mut Sink,
) {
    let rows = src.rows();
    let cols = src.cols();
    if rows == 0 || cols == 0 {
        return;
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
    // chains from that path, manifesting as input latency.  Now one
    // `chars` buffer serves every logical line, and it lives in the
    // caller's `LinkScratch`, so a warm frame allocates nothing.
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
    chars.clear();
    col_map.clear();
    segments.clear();
    rule_cols.clear();
    chars.reserve(line_cap);
    col_map.reserve(line_cap);
    let mut char_offset: usize = 0;
    // Vertical rules seen while walking, for the table-cell pass
    // below.  Collected here rather than in a sweep of its own: this
    // loop already reads every cell once, and a second full pass over
    // the grid per pane per frame is exactly the kind of cost this
    // scanner has been profiled to avoid.  Empty on prose.
    for r in 0..rows {
        // Rows inside the claudecode composer box are hard-skipped:
        // flush any in-flight logical line above, then jump past this
        // row without contributing any chars.  The `chars`/`col_map`/
        // `segments` buffers are cleared so the row after the box
        // starts a fresh line, not a phantom continuation.
        if let Some((lo, hi)) = exempt_range
            && r >= lo && r <= hi {
                if !segments.is_empty() {
                    scan_logical_line(chars, col_map, segments, cols as usize, out, oracle, opts.cwd, bufs);
                    chars.clear();
                    col_map.clear();
                    segments.clear();
                    char_offset = 0;
                }
                continue;
            }
        let decawm_cont = r > 0 && src.is_soft_wrap_continuation(r);
        let cc_cont = !decawm_cont
            && r > 0
            && opts.tui_mode
            && is_hard_wrap_continuation(src, r - 1, r, cols);
        let is_continuation = decawm_cont || cc_cont;
        if !is_continuation && !segments.is_empty() {
            scan_logical_line(chars, col_map, segments, cols as usize, out, oracle, opts.cwd, bufs);
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
            cc_seam: cc_cont,
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
        scan_logical_line(chars, col_map, segments, cols as usize, out, oracle, opts.cwd, bufs);
    }
    if opts.tui_mode && rule_cols.len() >= 4 {
        let before = out.len;
        scan_table_cells(src, cols, rule_cols, out, oracle, opts.cwd, table, bufs);
        // Nothing crossed a cell boundary — leave the row pass's
        // output exactly as it was, sort and all.
        if out.len != before {
            drop_shadowed_ranges(out);
        }
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
        cc_seam: false,
    }];
    let mut targets = Vec::new();
    let len = out.len();
    let mut sink = Sink { out, len, targets: &mut targets };
    let mut bufs = TextBufs::default();
    scan_line_into_matches(&chars, &col_map, &mut sink, &segments, cols, &FsOracle, None, &mut bufs);
    let len = sink.len;
    out.truncate(len);
}

#[cfg(test)]
mod tests;

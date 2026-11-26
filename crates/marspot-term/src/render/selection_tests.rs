use super::grid_selection_text;
use crate::grid::{Cell, Grid};

fn write_row(grid: &mut Grid, row: u16, s: &str) {
    for (c, ch) in s.chars().enumerate() {
        grid.set_cell(
            c as u16,
            row,
            Cell {
                ch,
                ..Default::default()
            },
        );
    }
}

// cell_at_view maps abs 0 → bottom grid row (rows-1).  grid_selection_text
// calls it with viewport_row = rows-1, so a selection abs N reads grid row
// rows-1-N.  Put text on the bottom row → abs 0.
#[test]
fn single_row_selection_spans_columns() {
    let mut grid = Grid::new(10, 3);
    write_row(&mut grid, 2, "hello");
    // anchor/focus on the same row (abs 0), columns 0..=4.  Either order
    // of the two ends must yield the same span — this is the case that
    // used to collapse to an empty range and copy nothing.
    assert_eq!(
        grid_selection_text(&grid, (0, 0), (4, 0), false).as_deref(),
        Some("hello")
    );
    assert_eq!(
        grid_selection_text(&grid, (4, 0), (0, 0), false).as_deref(),
        Some("hello")
    );
}

#[test]
fn multi_row_selection_joins_with_newline() {
    let mut grid = Grid::new(10, 3);
    write_row(&mut grid, 1, "foo");
    write_row(&mut grid, 2, "bar");
    // Top row (abs 1) from its start, bottom row (abs 0) through col 2.
    assert_eq!(
        grid_selection_text(&grid, (0, 1), (2, 0), false).as_deref(),
        Some("foo\nbar")
    );
}

// Phase 1 — DECAWM soft-wrap aware selection.  A row that the
// parser flagged as a continuation of the row above (`row_wrapped`
// set) is merged into the previous row without an intervening `\n`,
// and the previous row keeps its trailing chars (no trim) because
// they were real content forced down by the overflow.
#[test]
fn soft_wrap_continuation_merges_without_newline() {
    let mut grid = Grid::new(5, 3);
    // Top row (abs 2) fills col 0..=4 with "hello"; the parser
    // flagged the row BELOW as the wrap continuation.
    write_row(&mut grid, 0, "hello");
    // Continuation row (abs 1) carries "world" at cols 0..=4.
    write_row(&mut grid, 1, "world");
    // The full-width content row (abs 0) is empty (logical line
    // ends after "helloworld").
    // Mark the wrap chain: rows 1 and 2 (live row indices) are
    // continuations of the row physically above.  set_row_wrapped
    // takes live-row coords (0 = top live row).
    grid.set_row_wrapped(1, true);
    // Select across both wrapped rows.  Expected: merged into a
    // single token "helloworld" with no `\n`.
    assert_eq!(
        grid_selection_text(&grid, (0, 2), (4, 1), false).as_deref(),
        Some("helloworld")
    );
}

// Regression for the "L3_EXECV_INVOKE log 里 control_stream_fd=N…
// RESUMED" copy-paste case (2026-06-17): user selects across two
// soft-wrapped rows, but the bot row ends mid-row.  Pre-fix, the
// wrap-merge required col_hi=cols-1 on the current row, so the
// bot row failed the gate, the merge fell through, the prev row
// was trim_end + `\n` joined — user saw a phantom newline and
// lost the prev row's trailing chars.
#[test]
fn soft_wrap_merges_when_bot_row_ends_mid_row() {
    let mut grid = Grid::new(5, 3);
    write_row(&mut grid, 0, "hello");
    write_row(&mut grid, 1, "world");
    grid.set_row_wrapped(1, true);
    // Select hello..wor (top from col 0, bot up to col 2 only).
    assert_eq!(
        grid_selection_text(&grid, (0, 2), (2, 1), false).as_deref(),
        Some("hellowor")
    );
}

// Same wrap-merge semantics for a TOP row that starts mid-row.
#[test]
fn soft_wrap_merges_when_top_row_starts_mid_row() {
    let mut grid = Grid::new(5, 3);
    write_row(&mut grid, 0, "hello");
    write_row(&mut grid, 1, "world");
    grid.set_row_wrapped(1, true);
    // Select llo + world (top from col 2 onwards).
    assert_eq!(
        grid_selection_text(&grid, (2, 2), (4, 1), false).as_deref(),
        Some("lloworld")
    );
}

// Trailing real spaces inside a wrapped row must NOT be trim'd:
// they're real content that overflowed.
#[test]
fn soft_wrap_preserves_trailing_spaces_in_prev_row() {
    let mut grid = Grid::new(5, 3);
    // "ab " + "cd" on the row above; the trailing space is real
    // (parser would only wrap if col=cols-1 was occupied).  For
    // this scenario assume col 2 is a real space, col 3+4 are
    // "ab" – so write_row writes "ab cd" across 5 cols.
    write_row(&mut grid, 0, "ab cd");
    write_row(&mut grid, 1, "next!");
    grid.set_row_wrapped(1, true);
    // Whole selection.
    assert_eq!(
        grid_selection_text(&grid, (0, 2), (4, 1), false).as_deref(),
        Some("ab cdnext!")
    );
}

/// End-to-end test: feed real bytes through the VT parser, then
/// run `grid_selection_text` against the resulting grid.  Mirrors
/// the actual L3 pipeline (PTY byte stream → Terminal::feed →
/// grid → on Cmd-C reply with grid_selection_text), so a fix in
/// render.rs that passes unit tests but fails the live pipeline
/// gets caught here instead of by the user copy-pasting.
fn select_text_via_parser(cols: u16, rows: u16, bytes: &[u8]) -> Option<String> {
    let mut t = crate::terminal::Terminal::new(cols, rows);
    t.feed(bytes);
    let grid = t.grid();
    // Whole visible viewport: (col 0, abs rows-1) → (cols-1, abs 0).
    let top_abs = (rows - 1) as u32;
    let bot_abs = 0u32;
    grid_selection_text(grid, (0, top_abs), (cols - 1, bot_abs), false)
}

#[test]
fn e2e_soft_wrap_long_ascii_has_no_phantom_newline() {
    // 200 'a's at 20×3 forces 10 rows of soft-wrap (well over the
    // viewport).  Grid's bottom 3 rows hold the tail; the wrap
    // continuation flag is set by the parser on every row after
    // the first overflow.  Whole-viewport copy must produce a
    // contiguous run of 'a's with no `\n` interrupting it.
    let bytes = "a".repeat(200);
    let out = select_text_via_parser(20, 3, bytes.as_bytes()).expect("non-empty selection");
    assert!(!out.contains('\n'), "phantom \\n in: {out:?}");
    assert!(out.chars().all(|c| c == 'a'), "non-'a' char in: {out:?}");
}

#[test]
fn e2e_hard_newline_inserts_logical_break() {
    // Three short lines separated by CR/LF — the parser sets no
    // wrap flag, so each line boundary is a real logical newline
    // and selection should preserve it.  Validates the negative
    // case so we don't accidentally swallow real newlines along
    // with the soft-wrap fix.
    let bytes = b"abc\r\ndef\r\nghi";
    let out = select_text_via_parser(20, 3, bytes).expect("non-empty selection");
    assert_eq!(out, "abc\ndef\nghi");
}

#[test]
fn e2e_long_url_across_soft_wrap_is_one_logical_line() {
    // URL that overflows the column width; selection must yield
    // the URL un-broken — same input grid_links::scan_visible_
    // links uses to detect the multi-row LinkRange.
    let cols = 20u16;
    let url = "https://example.com/some/very/long/path/that/wraps?q=value";
    let out = select_text_via_parser(cols, 5, url.as_bytes()).expect("non-empty selection");
    let normalised: String = out.chars().filter(|c| !c.is_whitespace()).collect();
    assert_eq!(normalised, url, "URL mangled by wrap: {out:?}");
}

#[test]
fn logical_newline_still_breaks() {
    let mut grid = Grid::new(10, 3);
    // Same row-mapping convention as `multi_row_selection_joins_
    // _with_newline`: write into live rows 1+2 so they map to abs
    // 1 (top) and abs 0 (bottom) under last_view_row=2.
    write_row(&mut grid, 1, "abc");
    write_row(&mut grid, 2, "def");
    // No wrap flag set — these are two logical lines, joined with `\n`.
    assert_eq!(
        grid_selection_text(&grid, (0, 1), (2, 0), false).as_deref(),
        Some("abc\ndef")
    );
}

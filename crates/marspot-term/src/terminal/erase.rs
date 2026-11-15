//! Blanking cells: erase in line and display, and the blank a
//! fill leaves behind.

use crate::grid::{Cell, CellAttrs, Grid};

pub(super) fn blank_with(attrs: CellAttrs) -> Cell {
    // BCE inherits ONLY the background colour — that is the whole of
    // the xterm contract.  Stamping the full SGR state into erase /
    // scroll-fill blanks let UNDERLINE leak: printing a `\e[4m` URL
    // that wrapped on the bottom row scrolled a fresh row into
    // existence while underline was still on, so every blank in it
    // was born underlined and the tail of the line dragged a rule to
    // the window edge (2026-07-29 field report — the omz update
    // banner grew stray horizontal lines through its links and logo).
    // bold/italic/underline/reverse/dim are glyph properties; a blank
    // has no glyph, and reverse on a blank would even paint a solid
    // fg-coloured block no real terminal shows on clear.
    Cell {
        ch: ' ',
        attrs: CellAttrs {
            bg: attrs.bg,
            ..CellAttrs::default()
        },
    }
}

pub(super) fn fill_range(grid: &mut Grid, start: u32, end_exclusive: u32, attrs: CellAttrs) {
    if start >= end_exclusive {
        return;
    }
    let cols = grid.cols() as u32;
    let blank = blank_with(attrs);
    if start == 0 && end_exclusive >= cols * grid.rows() as u32 {
        // the whole screen: the table of attributes starts over with it
        grid.clear_all(blank);
        return;
    }
    // ─── Wide-pair boundary repair ───────────────────────────────────
    //
    // A wide grapheme (CJK, emoji, our Ambiguous=Wide set: ① ★ ▲ etc.)
    // occupies TWO adjacent grid cells: a `lead` carrying the glyph
    // and a `trail` carrying `\0` as a sentinel.  If `fill_range`
    // straddles a wide pair, leaving only one half intact creates a
    // ghost: the renderer keeps painting the (still-present) lead's
    // wide glyph over the (now-blank) trail slot, or a stale `\0`
    // trail next to a blank lead reads as a phantom space.  This was
    // the 2026-06-15 "横线残留" report — claudecode emitted EL on a
    // row containing a box-drawing wide sequence and a fragment of
    // the old line survived the wipe.
    //
    // Fix both edges:
    //
    //   left:  if `start` is a wide TRAIL (the cell just before on
    //          the same row is a wide lead), pull `start` back by 1
    //          so the orphan lead gets blanked too.
    //
    //   right: if `end-1` is a wide LEAD, its trail sits at `end`.
    //          Extend `end` by 1 so the orphan trail is reset.
    //
    // Both checks skip when the boundary falls on a row break
    // (col == 0) — wide pairs can't straddle rows (the parser breaks
    // them at the last column).
    let mut start = start;
    let mut end_exclusive = end_exclusive;
    if start > 0 && !start.is_multiple_of(cols) {
        let prev_col = ((start - 1) % cols) as u16;
        let row = ((start - 1) / cols) as u16;
        let prev = grid.cell(prev_col, row);
        if crate::grid::char_width(prev.ch) == 2 {
            start -= 1;
        }
    }
    let total = cols * grid.rows() as u32;
    if end_exclusive < total && !end_exclusive.is_multiple_of(cols) {
        let last_col = ((end_exclusive - 1) % cols) as u16;
        let row = ((end_exclusive - 1) / cols) as u16;
        let last = grid.cell(last_col, row);
        if crate::grid::char_width(last.ch) == 2 {
            end_exclusive += 1;
        }
    }
    for idx in start..end_exclusive {
        let col = (idx % cols) as u16;
        let row = (idx / cols) as u16;
        grid.set_cell(col, row, blank);
    }
}

pub(super) fn erase_in_display(
    grid: &mut Grid,
    col: u16,
    row: u16,
    cols: u16,
    rows: u16,
    mode: u16,
    attrs: CellAttrs,
) {
    let total = cols as u32 * rows as u32;
    let cursor_idx = row as u32 * cols as u32 + col as u32;
    match mode {
        0 => fill_range(grid, cursor_idx, total, attrs),
        1 => fill_range(grid, 0, cursor_idx + 1, attrs),
        2 => fill_range(grid, 0, total, attrs),
        // 3 — erase scrollback.  Handled by the caller, which is where
        // the scrollback is; nothing reaches here with it.
        _ => {}
    }
}

pub(super) fn erase_in_line(grid: &mut Grid, col: u16, row: u16, cols: u16, mode: u16, attrs: CellAttrs) {
    let row_start = row as u32 * cols as u32;
    let row_end = row_start + cols as u32;
    let cursor_idx = row_start + col as u32;
    match mode {
        0 => fill_range(grid, cursor_idx, row_end, attrs),
        1 => fill_range(grid, row_start, cursor_idx + 1, attrs),
        2 => fill_range(grid, row_start, row_end, attrs),
        _ => {}
    }
}

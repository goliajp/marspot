//! Terminal emulator: bridges parsed VT events to grid mutations.
//!
//! `Terminal` owns a `Grid` (screen state) and a `Parser` (byte→event
//! state machine).  Bytes go in via `feed()`; the parser tokenizes them
//! and dispatches to `Handler`, which applies the semantic effect on the
//! grid (move cursor, print glyph, etc.).
//!
//! This module currently handles:
//! - Printable characters, CR, LF, BS routed to the corresponding `Grid` ops
//! - CSI cursor movement: CUU (A) / CUD (B) / CUF (C) / CUB (D) / CUP (H)
//!   / HVP (f) / HPA (G) / VPA (d)
//!
//! Phase 1.1.3+ will layer in erase, SGR attributes, scrolling, and more.

use crate::grid::{char_width, Cell, CellAttrs, Color, Grid};
use crate::parser::{Parser, ParserCallbacks};

pub struct Terminal {
    grid: Grid,
    parser: Parser,
    /// Current SGR state — every printed glyph (and every BCE-erased cell)
    /// is stamped with this snapshot.  Persists across `feed` calls.
    attrs: CellAttrs,
}

impl Terminal {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            grid: Grid::new(cols, rows),
            parser: Parser::new(),
            attrs: CellAttrs::default(),
        }
    }

    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.grid.resize(cols, rows);
    }

    pub fn current_attrs(&self) -> CellAttrs {
        self.attrs
    }

    /// Feed bytes from the PTY through the parser, applying their effects to
    /// the grid.  Splits the &mut self borrow so the parser (which mutates
    /// its own internal state) and the handler (which mutates the grid) can
    /// both be live for each call to `Parser::advance`.
    pub fn feed(&mut self, bytes: &[u8]) {
        let parser = &mut self.parser;
        let grid = &mut self.grid;
        let attrs = &mut self.attrs;
        let mut handler = Handler { grid, attrs };
        for &b in bytes {
            parser.advance(&mut handler, b);
        }
    }
}

struct Handler<'a> {
    grid: &'a mut Grid,
    attrs: &'a mut CellAttrs,
}

impl<'a> ParserCallbacks for Handler<'a> {
    fn print(&mut self, ch: char) {
        let w = char_width(ch);
        if w == 0 {
            // Zero-width / control marker — terminal already executes
            // C0 controls separately.  Nothing to draw or advance.
            return;
        }
        let cols = self.grid.cols();
        let rows = self.grid.rows();
        let (mut col, mut row) = self.grid.cursor();

        // A wide char at the last column can't fit. Wrap first, then print
        // at the start of the new row.
        if w == 2 && col + 1 >= cols {
            if row + 1 < rows {
                self.grid.set_cursor(0, row + 1);
            } else {
                self.grid.scroll_up(1, blank_with(*self.attrs));
                self.grid.set_cursor(0, rows - 1);
            }
            let next = self.grid.cursor();
            col = next.0;
            row = next.1;
        }

        // Lead cell carries the printable char.  For wide chars, the trail
        // cell stores NUL with the same attrs — the renderer skips drawing
        // its glyph (NUL is treated as blank), and the lead glyph extends
        // visually across both cells via its natural advance width.
        self.grid.set_cell(col, row, Cell { ch, attrs: *self.attrs });
        if w == 2 {
            self.grid.set_cell(col + 1, row, Cell { ch: '\0', attrs: *self.attrs });
        }

        let next_col = col + w as u16;
        if next_col < cols {
            self.grid.set_cursor(next_col, row);
        } else if row + 1 < rows {
            self.grid.set_cursor(0, row + 1);
        } else {
            self.grid.scroll_up(1, blank_with(*self.attrs));
            self.grid.set_cursor(0, rows - 1);
        }
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x08 => {
                // BS: cursor left one column, clamped at column 0.  Does
                // not erase the cell.
                let (col, row) = self.grid.cursor();
                if col > 0 {
                    self.grid.set_cursor(col - 1, row);
                }
            }
            0x0A | 0x0B | 0x0C => {
                // LF / VT / FF: cursor down one row, scrolling at the
                // bottom.  Does not change column (LNM mode unset).
                let (col, row) = self.grid.cursor();
                let rows = self.grid.rows();
                if row + 1 < rows {
                    self.grid.set_cursor(col, row + 1);
                } else {
                    self.grid.scroll_up(1, blank_with(*self.attrs));
                    // Cursor stays at the now-blank last row.
                }
            }
            0x0D => {
                // CR: cursor to column 0 of current row.
                let (_col, row) = self.grid.cursor();
                self.grid.set_cursor(0, row);
            }
            0x09 => {} // TAB — tab stops land in a later phase
            0x07 => {} // BEL — visible bell deferred
            _ => {}    // unknown C0 control: ignore for now
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _byte: u8) {
        // ESC dispatch handled in later phases (RIS, charset switching, etc.)
    }

    fn csi_dispatch(&mut self, params: &[u16], intermediates: &[u8], byte: u8) {
        if !intermediates.is_empty() {
            // Private-marker sequences (e.g. CSI ? 25 h DECSET) are not yet
            // implemented; ignore so we don't fire wrong actions.
            return;
        }
        let (col, row) = self.grid.cursor();
        let cols = self.grid.cols();
        let rows = self.grid.rows();
        match byte {
            b'A' => {
                // CUU: cursor up by N (default 1).
                let n = param(params, 0, 1);
                self.grid.set_cursor(col, row.saturating_sub(n));
            }
            b'B' => {
                // CUD: cursor down by N.  set_cursor clamps at rows-1.
                let n = param(params, 0, 1);
                self.grid.set_cursor(col, row.saturating_add(n).min(rows - 1));
            }
            b'C' => {
                // CUF: cursor forward (right).
                let n = param(params, 0, 1);
                self.grid.set_cursor(col.saturating_add(n).min(cols - 1), row);
            }
            b'D' => {
                // CUB: cursor back (left).
                let n = param(params, 0, 1);
                self.grid.set_cursor(col.saturating_sub(n), row);
            }
            b'H' | b'f' => {
                // CUP / HVP: cursor position (1-indexed row;col → 0-indexed).
                let r = param(params, 0, 1).saturating_sub(1);
                let c = param(params, 1, 1).saturating_sub(1);
                self.grid.set_cursor(c, r);
            }
            b'G' => {
                // HPA: horizontal position absolute (1-indexed).
                let c = param(params, 0, 1).saturating_sub(1);
                self.grid.set_cursor(c, row);
            }
            b'd' => {
                // VPA: vertical position absolute (1-indexed).
                let r = param(params, 0, 1).saturating_sub(1);
                self.grid.set_cursor(col, r);
            }
            b'J' => {
                // ED: erase in display.  Cursor is not moved.
                // 0 (default) — from cursor (inclusive) to end of screen
                // 1           — from start of screen to cursor (inclusive)
                // 2           — entire screen
                // 3           — entire scrollback (xterm extension)
                let mode = param_raw(params, 0, 0);
                if mode == 3 {
                    self.grid.clear_scrollback();
                } else {
                    erase_in_display(self.grid, col, row, cols, rows, mode, *self.attrs);
                }
            }
            b'K' => {
                // EL: erase in line.  Cursor not moved.
                // 0 (default) — from cursor (inclusive) to end of line
                // 1           — from start of line to cursor (inclusive)
                // 2           — entire line
                let mode = param_raw(params, 0, 0);
                erase_in_line(self.grid, col, row, cols, mode, *self.attrs);
            }
            b'm' => {
                // SGR: set graphic rendition.  Mutates self.attrs in place.
                apply_sgr(self.attrs, params);
            }
            _ => {} // scrolling, mode-set, etc. arrive in later phases
        }
    }

    fn osc_dispatch(&mut self, _data: &[u8]) {
        // OSC handlers (window title, hyperlinks, palette) — later phase.
    }
}

/// Look up a CSI parameter, treating `0` and "missing" both as the supplied
/// default — this matches the standard convention where omitted params and
/// explicit `0` are equivalent for cursor movement and most other CSIs.
fn param(params: &[u16], idx: usize, default: u16) -> u16 {
    match params.get(idx).copied() {
        Some(0) | None => default,
        Some(n) => n,
    }
}

/// Look up a CSI parameter without folding `0` into the default.  ED and EL
/// use this convention: the explicit `0` is a real selector (= "from cursor
/// to end"), distinct from "omitted" which is also `0` here.
fn param_raw(params: &[u16], idx: usize, default: u16) -> u16 {
    params.get(idx).copied().unwrap_or(default)
}

/// A blank ' ' stamped with the supplied attrs.  Used wherever new cells
/// appear (BCE for erase, scroll-fill for the new bottom row).  Apps like
/// vim and tmux rely on this — clearing or scrolling under a non-default
/// background must produce colored cells, not transparent ones.
fn blank_with(attrs: CellAttrs) -> Cell {
    Cell { ch: ' ', attrs }
}

fn fill_range(grid: &mut Grid, start: u32, end_exclusive: u32, attrs: CellAttrs) {
    let cols = grid.cols() as u32;
    let blank = blank_with(attrs);
    for idx in start..end_exclusive {
        let col = (idx % cols) as u16;
        let row = (idx / cols) as u16;
        grid.set_cell(col, row, blank);
    }
}

fn erase_in_display(
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
        // 3 = erase scrollback — deferred until scrollback exists (1.1.5).
        _ => {}
    }
}

fn erase_in_line(grid: &mut Grid, col: u16, row: u16, cols: u16, mode: u16, attrs: CellAttrs) {
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

/// Apply a CSI SGR (Select Graphic Rendition) sequence.  Empty params is
/// equivalent to `[0]` (reset), per the standard.
///
/// We walk the param list with an explicit index because 38/48 (extended
/// color) consume additional params depending on the second value.
fn apply_sgr(attrs: &mut CellAttrs, params: &[u16]) {
    if params.is_empty() {
        *attrs = CellAttrs::default();
        return;
    }
    let mut i = 0;
    while i < params.len() {
        match params[i] {
            0 => *attrs = CellAttrs::default(),
            1 => attrs.bold = true,
            3 => attrs.italic = true,
            4 => attrs.underline = true,
            7 => attrs.reverse = true,
            22 => attrs.bold = false,
            23 => attrs.italic = false,
            24 => attrs.underline = false,
            27 => attrs.reverse = false,
            // Standard 8-color foreground.
            n @ 30..=37 => attrs.fg = Color::Indexed((n - 30) as u8),
            // Extended foreground: 38;5;n (256-color) or 38;2;r;g;b (RGB).
            38 => {
                if let Some((color, consumed)) = parse_extended_color(&params[i + 1..]) {
                    attrs.fg = color;
                    i += consumed;
                }
            }
            39 => attrs.fg = Color::Default,
            n @ 40..=47 => attrs.bg = Color::Indexed((n - 40) as u8),
            48 => {
                if let Some((color, consumed)) = parse_extended_color(&params[i + 1..]) {
                    attrs.bg = color;
                    i += consumed;
                }
            }
            49 => attrs.bg = Color::Default,
            // Bright foreground (8–15).
            n @ 90..=97 => attrs.fg = Color::Indexed(8 + (n - 90) as u8),
            n @ 100..=107 => attrs.bg = Color::Indexed(8 + (n - 100) as u8),
            _ => {} // unknown / unimplemented SGR code: silently skip
        }
        i += 1;
    }
}

/// Parse the tail of a 38/48 sequence.  Returns the resulting `Color` and
/// the number of *additional* params consumed beyond the 38/48 itself, so
/// the caller can advance its index.
///
/// 5;n           → Indexed(n) — 256-color palette
/// 2;r;g;b       → Rgb(r,g,b) — direct color
/// anything else → None (caller leaves attrs unchanged and advances 1)
fn parse_extended_color(rest: &[u16]) -> Option<(Color, usize)> {
    match rest.first().copied()? {
        5 => {
            let n = rest.get(1).copied()?;
            Some((Color::Indexed(n.min(255) as u8), 2))
        }
        2 => {
            let r = rest.get(1).copied()?;
            let g = rest.get(2).copied()?;
            let b = rest.get(3).copied()?;
            Some((Color::Rgb(r.min(255) as u8, g.min(255) as u8, b.min(255) as u8), 4))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term_with(cols: u16, rows: u16, bytes: &[u8]) -> Terminal {
        let mut t = Terminal::new(cols, rows);
        t.feed(bytes);
        t
    }

    #[test]
    fn plain_ascii_writes_cells_and_advances_cursor() {
        let t = term_with(80, 24, b"hi");
        assert_eq!(t.grid().cell(0, 0).ch, 'h');
        assert_eq!(t.grid().cell(1, 0).ch, 'i');
        assert_eq!(t.grid().cursor(), (2, 0));
    }

    #[test]
    fn lf_only_advances_row_does_not_reset_column() {
        // xterm default (LNM unset): LF moves down only.  CR is needed to
        // return to column 0.  Many real shells emit CRLF together.
        let t = term_with(80, 24, b"abc\n");
        assert_eq!(t.grid().cursor(), (3, 1));
    }

    #[test]
    fn crlf_returns_to_next_line_start() {
        let t = term_with(80, 24, b"abc\r\n");
        assert_eq!(t.grid().cursor(), (0, 1));
    }

    #[test]
    fn csi_a_moves_cursor_up() {
        let t = term_with(80, 24, b"\n\n\n\x1B[2A");
        // After 3 LFs cursor is at row 3, col 0.  CUU 2 → row 1.
        assert_eq!(t.grid().cursor(), (0, 1));
    }

    #[test]
    fn csi_b_moves_cursor_down() {
        let t = term_with(80, 24, b"\x1B[3B");
        assert_eq!(t.grid().cursor(), (0, 3));
    }

    #[test]
    fn csi_c_moves_cursor_right() {
        let t = term_with(80, 24, b"\x1B[4C");
        assert_eq!(t.grid().cursor(), (4, 0));
    }

    #[test]
    fn csi_d_moves_cursor_left() {
        let t = term_with(80, 24, b"abcdef\x1B[3D");
        assert_eq!(t.grid().cursor(), (3, 0));
    }

    #[test]
    fn csi_no_param_moves_by_one() {
        let t = term_with(80, 24, b"\x1B[B");
        assert_eq!(t.grid().cursor(), (0, 1));
    }

    #[test]
    fn csi_zero_param_treated_as_default() {
        // Param `0` and omitted are both "default" → 1 for cursor moves.
        let t = term_with(80, 24, b"\x1B[0B");
        assert_eq!(t.grid().cursor(), (0, 1));
    }

    #[test]
    fn csi_h_no_params_homes_cursor() {
        let t = term_with(80, 24, b"abc\n\n\x1B[H");
        assert_eq!(t.grid().cursor(), (0, 0));
    }

    #[test]
    fn csi_h_with_row_and_col_positions_cursor() {
        // CSI 3 ; 5 H → row 3, col 5 (1-indexed) → (col=4, row=2) 0-indexed.
        let t = term_with(80, 24, b"\x1B[3;5H");
        assert_eq!(t.grid().cursor(), (4, 2));
    }

    #[test]
    fn csi_f_alias_for_cup() {
        let t = term_with(80, 24, b"\x1B[3;5f");
        assert_eq!(t.grid().cursor(), (4, 2));
    }

    #[test]
    fn csi_g_sets_horizontal_position_absolute() {
        let t = term_with(80, 24, b"\n\x1B[10G");
        // VPA preserved at row 1; col = 10 (1-indexed) = 9 (0-indexed).
        assert_eq!(t.grid().cursor(), (9, 1));
    }

    #[test]
    fn csi_d_lowercase_sets_vertical_position_absolute() {
        let t = term_with(80, 24, b"abc\x1B[5d");
        // HPA preserved at col 3; row = 5 (1-indexed) = 4 (0-indexed).
        assert_eq!(t.grid().cursor(), (3, 4));
    }

    #[test]
    fn cursor_clamps_at_top_left() {
        let t = term_with(80, 24, b"\x1B[A\x1B[D");
        // CUU and CUB at origin must not underflow; cursor stays at (0,0).
        assert_eq!(t.grid().cursor(), (0, 0));
    }

    #[test]
    fn cursor_clamps_at_bottom_right() {
        // CUP off-grid: 99;99 gets clamped to (rows-1, cols-1).  Then CUF /
        // CUD must not move further.
        let t = term_with(80, 24, b"\x1B[99;99H\x1B[10C\x1B[10B");
        assert_eq!(t.grid().cursor(), (79, 23));
    }

    #[test]
    fn print_after_cursor_move_writes_at_new_position() {
        // Move cursor then print: cell at the moved-to position must hold
        // the printed glyph, not the original origin.
        let t = term_with(80, 24, b"\x1B[5;3HX");
        assert_eq!(t.grid().cell(2, 4).ch, 'X');
        assert_eq!(t.grid().cursor(), (3, 4));
    }

    /// Fill the grid with a marker char at every cell so erase regions are
    /// visible by absence.  Returns a Terminal with cursor parked at the
    /// requested (col, row) and all cells populated.
    fn filled_terminal(cols: u16, rows: u16, marker: char, cur_col: u16, cur_row: u16) -> Terminal {
        let mut t = Terminal::new(cols, rows);
        // Manually fill: write `marker` cols times per row, no parsing.
        // We use direct grid access only in tests — production code goes
        // through Terminal::feed.
        for r in 0..rows {
            for c in 0..cols {
                t.grid.set_cell(c, r, Cell { ch: marker, ..Default::default() });
            }
        }
        t.grid.set_cursor(cur_col, cur_row);
        t
    }

    fn count_marker(t: &Terminal, marker: char) -> usize {
        let g = t.grid();
        let mut n = 0;
        for r in 0..g.rows() {
            for c in 0..g.cols() {
                if g.cell(c, r).ch == marker {
                    n += 1;
                }
            }
        }
        n
    }

    #[test]
    fn ed_zero_erases_from_cursor_to_end_of_screen() {
        let mut t = filled_terminal(10, 5, '#', 3, 2);
        let before = count_marker(&t, '#'); // 50
        t.feed(b"\x1B[J"); // ED default = 0
        let cleared = before - count_marker(&t, '#');
        // From (3, 2) inclusive to end of screen: row 2 has cols 3..10 = 7,
        // rows 3 and 4 each have 10, total = 7 + 20 = 27.
        assert_eq!(cleared, 27);
        // Cursor unchanged.
        assert_eq!(t.grid().cursor(), (3, 2));
        // Cells before cursor untouched.
        assert_eq!(t.grid().cell(2, 2).ch, '#');
        assert_eq!(t.grid().cell(3, 2).ch, ' ');
        assert_eq!(t.grid().cell(0, 4).ch, ' ');
    }

    #[test]
    fn ed_one_erases_from_start_to_cursor_inclusive() {
        let mut t = filled_terminal(10, 5, '#', 3, 2);
        t.feed(b"\x1B[1J");
        let remaining = count_marker(&t, '#');
        // Row 2 cols 0..=3 cleared (4 cells), rows 0 and 1 cleared (20).
        // Total cleared: 24.  Remaining: 50 - 24 = 26.
        assert_eq!(remaining, 26);
        assert_eq!(t.grid().cursor(), (3, 2));
        assert_eq!(t.grid().cell(3, 2).ch, ' ', "cursor cell must be cleared");
        assert_eq!(t.grid().cell(4, 2).ch, '#', "cell after cursor must remain");
    }

    #[test]
    fn ed_two_erases_entire_screen() {
        let mut t = filled_terminal(10, 5, '#', 3, 2);
        t.feed(b"\x1B[2J");
        assert_eq!(count_marker(&t, '#'), 0);
        assert_eq!(t.grid().cursor(), (3, 2), "cursor must not move");
    }

    #[test]
    fn el_zero_erases_from_cursor_to_end_of_line() {
        let mut t = filled_terminal(10, 5, '#', 3, 2);
        t.feed(b"\x1B[K");
        // Row 2 cols 3..10 = 7 cleared.
        assert_eq!(count_marker(&t, '#'), 50 - 7);
        assert_eq!(t.grid().cursor(), (3, 2));
        assert_eq!(t.grid().cell(2, 2).ch, '#');
        assert_eq!(t.grid().cell(9, 2).ch, ' ');
        // Other rows untouched.
        assert_eq!(t.grid().cell(5, 1).ch, '#');
        assert_eq!(t.grid().cell(5, 3).ch, '#');
    }

    #[test]
    fn el_one_erases_from_start_of_line_to_cursor_inclusive() {
        let mut t = filled_terminal(10, 5, '#', 3, 2);
        t.feed(b"\x1B[1K");
        // Row 2 cols 0..=3 = 4 cleared.
        assert_eq!(count_marker(&t, '#'), 50 - 4);
        assert_eq!(t.grid().cell(3, 2).ch, ' ');
        assert_eq!(t.grid().cell(4, 2).ch, '#');
        // Other rows untouched.
        assert_eq!(t.grid().cell(0, 1).ch, '#');
    }

    #[test]
    fn el_two_erases_entire_line() {
        let mut t = filled_terminal(10, 5, '#', 3, 2);
        t.feed(b"\x1B[2K");
        // Row 2 fully cleared (10 cells).
        assert_eq!(count_marker(&t, '#'), 50 - 10);
        assert_eq!(t.grid().cursor(), (3, 2));
        // Other rows untouched.
        assert_eq!(t.grid().cell(0, 1).ch, '#');
        assert_eq!(t.grid().cell(0, 3).ch, '#');
    }

    #[test]
    fn ed_three_does_not_panic_or_modify_screen() {
        // ED 3 erases scrollback in xterm; we have no scrollback yet, so it
        // must be a no-op (not panic, not clear visible screen).
        let mut t = filled_terminal(10, 5, '#', 3, 2);
        t.feed(b"\x1B[3J");
        assert_eq!(count_marker(&t, '#'), 50);
    }

    // ----- SGR (Select Graphic Rendition) -----

    #[test]
    fn sgr_empty_is_full_reset() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[1;31;42m");
        assert!(t.current_attrs().bold);
        assert_eq!(t.current_attrs().fg, Color::Indexed(1));
        t.feed(b"\x1B[m");
        assert_eq!(t.current_attrs(), CellAttrs::default());
    }

    #[test]
    fn sgr_zero_resets_all() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[1;31;42m");
        t.feed(b"\x1B[0m");
        assert_eq!(t.current_attrs(), CellAttrs::default());
    }

    #[test]
    fn sgr_bold_on_off_independent_of_color() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[31m\x1B[1m");
        assert!(t.current_attrs().bold);
        assert_eq!(t.current_attrs().fg, Color::Indexed(1));
        t.feed(b"\x1B[22m"); // un-bold; fg stays
        assert!(!t.current_attrs().bold);
        assert_eq!(t.current_attrs().fg, Color::Indexed(1));
    }

    #[test]
    fn sgr_individual_attribute_toggles() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[3m\x1B[4m\x1B[7m");
        let a = t.current_attrs();
        assert!(a.italic && a.underline && a.reverse);
        t.feed(b"\x1B[23m\x1B[24m\x1B[27m");
        let a = t.current_attrs();
        assert!(!a.italic && !a.underline && !a.reverse);
    }

    #[test]
    fn sgr_standard_8_color_fg() {
        for (code, idx) in (30u16..=37).zip(0u8..=7) {
            let mut t = Terminal::new(10, 5);
            t.feed(format!("\x1B[{}m", code).as_bytes());
            assert_eq!(t.current_attrs().fg, Color::Indexed(idx), "code {} -> idx {}", code, idx);
        }
    }

    #[test]
    fn sgr_standard_8_color_bg() {
        for (code, idx) in (40u16..=47).zip(0u8..=7) {
            let mut t = Terminal::new(10, 5);
            t.feed(format!("\x1B[{}m", code).as_bytes());
            assert_eq!(t.current_attrs().bg, Color::Indexed(idx), "code {} -> idx {}", code, idx);
        }
    }

    #[test]
    fn sgr_bright_8_color() {
        // 90–97 → indices 8–15 (bright fg); 100–107 → bright bg.
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[91m"); // bright red fg
        assert_eq!(t.current_attrs().fg, Color::Indexed(9));
        t.feed(b"\x1B[105m"); // bright magenta bg
        assert_eq!(t.current_attrs().bg, Color::Indexed(13));
    }

    #[test]
    fn sgr_default_fg_and_bg() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[31;41m\x1B[39;49m");
        assert_eq!(t.current_attrs().fg, Color::Default);
        assert_eq!(t.current_attrs().bg, Color::Default);
    }

    #[test]
    fn sgr_256_color_fg() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[38;5;208m");
        assert_eq!(t.current_attrs().fg, Color::Indexed(208));
    }

    #[test]
    fn sgr_256_color_bg() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[48;5;42m");
        assert_eq!(t.current_attrs().bg, Color::Indexed(42));
    }

    #[test]
    fn sgr_truecolor_fg() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[38;2;100;200;50m");
        assert_eq!(t.current_attrs().fg, Color::Rgb(100, 200, 50));
    }

    #[test]
    fn sgr_truecolor_bg() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[48;2;10;20;30m");
        assert_eq!(t.current_attrs().bg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn sgr_combined_in_single_sequence() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[1;31;42m");
        let a = t.current_attrs();
        assert!(a.bold);
        assert_eq!(a.fg, Color::Indexed(1));
        assert_eq!(a.bg, Color::Indexed(2));
    }

    #[test]
    fn sgr_unknown_code_is_skipped_not_panicked() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[1;999;31m"); // 999 unknown
        let a = t.current_attrs();
        assert!(a.bold);
        assert_eq!(a.fg, Color::Indexed(1)); // 31 still applied after 999 skipped
    }

    #[test]
    fn printed_cell_carries_current_attrs() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[1;31mA");
        let cell = t.grid().cell(0, 0);
        assert_eq!(cell.ch, 'A');
        assert!(cell.attrs.bold);
        assert_eq!(cell.attrs.fg, Color::Indexed(1));
    }

    #[test]
    fn already_printed_cells_unaffected_by_later_sgr() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"A\x1B[31mB");
        // 'A' was printed before SGR — must keep default attrs.
        assert_eq!(t.grid().cell(0, 0).attrs.fg, Color::Default);
        // 'B' was printed after — must have red fg.
        assert_eq!(t.grid().cell(1, 0).attrs.fg, Color::Indexed(1));
    }

    #[test]
    fn bce_erase_fills_with_current_attrs() {
        // Background Color Erase: ED/EL fills cleared cells with the
        // current SGR attrs, not default.  Critical for vim/tmux which
        // paint full-screen backgrounds via CSI 41 m + CSI 2 J.
        let mut t = Terminal::new(10, 3);
        t.feed(b"\x1B[41m\x1B[2J"); // bg red + clear screen
        for r in 0..3 {
            for c in 0..10 {
                let cell = t.grid().cell(c, r);
                assert_eq!(cell.ch, ' ');
                assert_eq!(cell.attrs.bg, Color::Indexed(1), "cell ({},{}) bg", c, r);
            }
        }
    }

    #[test]
    fn bce_erase_in_line_uses_current_bg() {
        let mut t = Terminal::new(10, 3);
        t.feed(b"\x1B[42m\x1B[K"); // bg green + EL 0
        for c in 0..10 {
            assert_eq!(t.grid().cell(c, 0).attrs.bg, Color::Indexed(2));
        }
        // Other rows untouched.
        assert_eq!(t.grid().cell(0, 1).attrs.bg, Color::Default);
    }

    // ----- print path: wrap and BS -----

    #[test]
    fn print_at_eol_wraps_to_next_row() {
        let mut t = Terminal::new(5, 3);
        t.feed(b"abcdef"); // 5 chars fill row 0; 'f' wraps to row 1 col 0
        assert_eq!(t.grid().cell(0, 0).ch, 'a');
        assert_eq!(t.grid().cell(4, 0).ch, 'e');
        assert_eq!(t.grid().cell(0, 1).ch, 'f');
        assert_eq!(t.grid().cursor(), (1, 1));
    }

    #[test]
    fn bs_decrements_column_and_does_not_erase() {
        let mut t = Terminal::new(10, 3);
        t.feed(b"ab\x08"); // print ab, then BS
        assert_eq!(t.grid().cursor(), (1, 0));
        // BS is non-destructive — 'b' must remain.
        assert_eq!(t.grid().cell(1, 0).ch, 'b');
    }

    #[test]
    fn bs_at_column_zero_is_clamped() {
        let mut t = Terminal::new(10, 3);
        t.feed(b"\x08");
        assert_eq!(t.grid().cursor(), (0, 0));
        t.feed(b"\n\x08"); // LF then BS — col stays 0, row stays 1
        assert_eq!(t.grid().cursor(), (0, 1));
    }

    // ----- scrolling -----

    #[test]
    fn lf_at_bottom_row_scrolls_up_and_pushes_to_scrollback() {
        let mut t = Terminal::new(3, 2);
        t.feed(b"abc"); // row 0 full, cursor at (3 wraps to row 1)
        assert_eq!(t.grid().cursor(), (0, 1));
        // Manually park cursor at last row, last col, then LF.
        t.feed(b"\x1B[2;3H"); // CUP row 2 col 3 (1-indexed) → (col 2, row 1)
        t.feed(b"\n");
        // Row 0 ("abc") goes to scrollback, row 1 becomes blank.
        assert_eq!(t.grid().scrollback_len(), 1);
        let sb = t.grid().scrollback_line(0).unwrap();
        assert_eq!(sb.iter().map(|c| c.ch).collect::<String>(), "abc");
        // Cursor stays on the (now blank) last row.
        assert_eq!(t.grid().cursor(), (2, 1));
        for c in 0..3 {
            assert_eq!(t.grid().cell(c, 1), Cell::default());
        }
    }

    #[test]
    fn print_overflow_at_bottom_row_scrolls() {
        // Print enough to fill the entire 2-row grid; the next print must
        // trigger a scroll, not clamp.
        let mut t = Terminal::new(3, 2);
        t.feed(b"abcdefg"); // 6 chars fill the grid; 'g' triggers scroll
        // After scroll: scrollback contains "abc", visible row 0 = "def",
        // 'g' lands at (0, 1).
        assert_eq!(t.grid().scrollback_len(), 1);
        let sb = t.grid().scrollback_line(0).unwrap();
        assert_eq!(sb.iter().map(|c| c.ch).collect::<String>(), "abc");
        assert_eq!(t.grid().cell(0, 0).ch, 'd');
        assert_eq!(t.grid().cell(2, 0).ch, 'f');
        assert_eq!(t.grid().cell(0, 1).ch, 'g');
        assert_eq!(t.grid().cursor(), (1, 1));
    }

    #[test]
    fn scroll_inherits_current_bg_for_blank_row() {
        // BCE for scroll-fill: when SGR has bg=red and we scroll, the new
        // blank row at the bottom must carry that bg.
        let mut t = Terminal::new(3, 2);
        t.feed(b"\x1B[41m"); // bg red
        t.feed(b"abc\x1B[2;3H\n"); // park cursor at last row, LF → scroll
        for c in 0..3 {
            let cell = t.grid().cell(c, 1);
            assert_eq!(cell.ch, ' ');
            assert_eq!(cell.attrs.bg, Color::Indexed(1));
        }
    }

    #[test]
    fn csi_3_J_clears_scrollback_only() {
        let mut t = Terminal::new(3, 2);
        // Build some scrollback by feeding many lines.
        for _ in 0..5 {
            t.feed(b"xxx\x1B[2;3H\n");
        }
        assert!(t.grid().scrollback_len() > 0);
        // ESC[3J clears scrollback; visible region untouched.
        t.feed(b"\x1B[3J");
        assert_eq!(t.grid().scrollback_len(), 0);
        // Visible row 0 should still hold what was there.
        assert_eq!(t.grid().cell(0, 0).ch, 'x');
    }

    // ----- soak: long-running scroll must not grow memory -----

    /// Read this process's resident set size in bytes.  Used by the soak
    /// test to assert that millions of scrolled lines don't grow the
    /// process — the scrollback ring is pre-allocated and bounded.
    fn current_rss_bytes() -> u64 {
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let r = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDTASKINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                std::mem::size_of::<libc::proc_taskinfo>() as i32,
            )
        };
        assert!(r > 0, "proc_pidinfo failed");
        info.pti_resident_size
    }

    #[test]
    #[ignore = "soak; run via bin/soak.sh"]
    fn soak_scrollback_bounded_under_million_lines() {
        let mut t = Terminal::new(80, 24);
        // Warm up: fill the scrollback ring once so its memory stabilizes.
        let warmup = b"\x1B[24;80H\n".repeat(15_000);
        t.feed(&warmup);

        let baseline = current_rss_bytes();

        // Now feed a million more newlines worth of scroll churn.
        let line = b"this is a fairly typical 50-character log line!\n";
        let park = b"\x1B[24;80H";
        for _ in 0..1_000_000 {
            t.feed(line);
            // After each line, park cursor at last row so the next \n scrolls.
            t.feed(park);
        }

        let after = current_rss_bytes();
        let growth = after.saturating_sub(baseline);

        // Tolerance: 5 MB.  The ring is pre-allocated to capacity at
        // construction; once warmed up, additional lines reuse slots.
        const TOLERANCE: u64 = 5 * 1024 * 1024;
        assert!(
            growth < TOLERANCE,
            "scrollback ring grew {} bytes ({}→{}); expected bounded",
            growth,
            baseline,
            after
        );

        // Sanity: scrollback is exactly capped at the configured capacity.
        assert_eq!(
            t.grid().scrollback_len(),
            t.grid().scrollback_capacity(),
            "scrollback should be at capacity after the soak"
        );
    }
}

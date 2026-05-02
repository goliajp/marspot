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

use crate::grid::Grid;
use crate::parser::{Parser, ParserCallbacks};

pub struct Terminal {
    grid: Grid,
    parser: Parser,
}

impl Terminal {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            grid: Grid::new(cols, rows),
            parser: Parser::new(),
        }
    }

    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    /// Feed bytes from the PTY through the parser, applying their effects to
    /// the grid.  Splits the &mut self borrow so the parser (which mutates
    /// its own internal state) and the handler (which mutates the grid) can
    /// both be live for each call to `Parser::advance`.
    pub fn feed(&mut self, bytes: &[u8]) {
        let parser = &mut self.parser;
        let grid = &mut self.grid;
        let mut handler = Handler { grid };
        for &b in bytes {
            parser.advance(&mut handler, b);
        }
    }
}

struct Handler<'a> {
    grid: &'a mut Grid,
}

impl<'a> ParserCallbacks for Handler<'a> {
    fn print(&mut self, ch: char) {
        self.grid.print(ch);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x08 => self.grid.backspace(),
            0x0A | 0x0B | 0x0C => self.grid.linefeed(), // LF, VT, FF all advance row
            0x0D => self.grid.carriage_return(),
            0x09 => {} // TAB — handled when we add tab stops in a later phase
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
            _ => {} // erase, SGR, scrolling, etc. arrive in later phases
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
}

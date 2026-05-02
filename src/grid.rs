//! Terminal cell grid and basic print path.
//!
//! The grid is the rectangular buffer of `Cell`s that backs the visible
//! screen.  Bytes interpreted by the (future) escape parser turn into
//! mutations on this grid: print a character, move the cursor, erase
//! regions, scroll lines, etc.  This module only handles the foundational
//! ops (print, CR, LF, BS) — escape-sequence-driven mutations land in
//! later phases on top of this.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { ch: ' ' }
    }
}

pub struct Grid {
    cols: u16,
    rows: u16,
    /// Row-major: index = row * cols + col.
    cells: Vec<Cell>,
    /// Cursor as (col, row).  Always bounded to [0, cols-1] x [0, rows-1].
    cursor_col: u16,
    cursor_row: u16,
}

impl Grid {
    pub fn new(cols: u16, rows: u16) -> Self {
        assert!(cols > 0 && rows > 0, "grid dimensions must be positive");
        let cells = vec![Cell::default(); cols as usize * rows as usize];
        Self { cols, rows, cells, cursor_col: 0, cursor_row: 0 }
    }

    pub fn cols(&self) -> u16 { self.cols }
    pub fn rows(&self) -> u16 { self.rows }
    pub fn cursor(&self) -> (u16, u16) { (self.cursor_col, self.cursor_row) }

    pub fn cell(&self, col: u16, row: u16) -> Cell {
        debug_assert!(col < self.cols && row < self.rows);
        self.cells[row as usize * self.cols as usize + col as usize]
    }

    /// Place a printable character at the cursor and advance.  At end-of-line
    /// we wrap to the next row (simple immediate wrap; delayed-wrap behavior
    /// matching xterm exactly is a later refinement).  At end-of-screen we
    /// clamp for now; scrolling lands in phase 1.1.5.
    pub fn print(&mut self, ch: char) {
        let idx = self.cursor_row as usize * self.cols as usize + self.cursor_col as usize;
        self.cells[idx] = Cell { ch };
        self.cursor_col += 1;
        if self.cursor_col >= self.cols {
            self.cursor_col = 0;
            if self.cursor_row + 1 < self.rows {
                self.cursor_row += 1;
            } else {
                // Bottom-row wrap: clamp until we have scrolling.  This means
                // text overflows onto the same last row — acceptable until
                // 1.1.5 replaces this with scroll-up + scrollback push.
            }
        }
    }

    /// CR (\r): cursor to column 0 of current row.
    pub fn carriage_return(&mut self) {
        self.cursor_col = 0;
    }

    /// LF (\n): cursor down one row.  Clamps at the last row until phase
    /// 1.1.5 introduces scrolling + scrollback.
    pub fn linefeed(&mut self) {
        if self.cursor_row + 1 < self.rows {
            self.cursor_row += 1;
        }
    }

    /// BS (\x08): cursor left one column, clamped at column 0.  Does not
    /// erase the cell — that's the caller's responsibility (BS in xterm
    /// is non-destructive).
    pub fn backspace(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        }
    }

    /// Clamp-and-set the cursor to (col, row).  Used by the emulator layer
    /// for CSI cursor positioning.  Out-of-bounds values are clamped to the
    /// last valid position; the cursor is always within `[0, cols) x [0, rows)`.
    pub fn set_cursor(&mut self, col: u16, row: u16) {
        self.cursor_col = col.min(self.cols - 1);
        self.cursor_row = row.min(self.rows - 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_grid_is_all_spaces_with_origin_cursor() {
        let g = Grid::new(80, 24);
        assert_eq!(g.cols(), 80);
        assert_eq!(g.rows(), 24);
        assert_eq!(g.cursor(), (0, 0));
        for r in 0..24 {
            for c in 0..80 {
                assert_eq!(g.cell(c, r).ch, ' ', "cell ({}, {}) not space", c, r);
            }
        }
    }

    #[test]
    fn print_writes_cell_and_advances_cursor() {
        let mut g = Grid::new(80, 24);
        g.print('A');
        assert_eq!(g.cell(0, 0).ch, 'A');
        assert_eq!(g.cursor(), (1, 0));
        g.print('B');
        assert_eq!(g.cell(1, 0).ch, 'B');
        assert_eq!(g.cursor(), (2, 0));
    }

    #[test]
    fn print_at_end_of_line_wraps_to_next_row() {
        let mut g = Grid::new(5, 3);
        for ch in "abcdef".chars() {
            g.print(ch);
        }
        assert_eq!(g.cell(0, 0).ch, 'a');
        assert_eq!(g.cell(4, 0).ch, 'e');
        assert_eq!(g.cell(0, 1).ch, 'f');
        assert_eq!(g.cursor(), (1, 1));
    }

    #[test]
    fn print_overflowing_bottom_clamps_for_now() {
        // v0: no scrolling yet.  Filling past the screen should not panic
        // and the final cursor sits within bounds.  Phase 1.1.5 replaces
        // this clamp with scroll-up + scrollback.
        let mut g = Grid::new(3, 2);
        for ch in "abcdefghij".chars() {
            g.print(ch);
        }
        let (col, row) = g.cursor();
        assert!(col < g.cols(), "cursor col {} out of bounds", col);
        assert!(row < g.rows(), "cursor row {} out of bounds", row);
    }

    #[test]
    fn carriage_return_zeros_column_only() {
        let mut g = Grid::new(80, 24);
        g.print('a'); g.print('b'); g.print('c');
        g.linefeed();
        g.print('d');
        assert_eq!(g.cursor(), (4, 1));
        g.carriage_return();
        assert_eq!(g.cursor(), (0, 1));
    }

    #[test]
    fn linefeed_advances_row() {
        let mut g = Grid::new(80, 24);
        g.linefeed();
        assert_eq!(g.cursor(), (0, 1));
        g.linefeed();
        assert_eq!(g.cursor(), (0, 2));
    }

    #[test]
    fn linefeed_at_bottom_clamps_for_now() {
        // v0 placeholder: at the last row LF clamps.  Phase 1.1.5 will scroll.
        let mut g = Grid::new(80, 3);
        g.linefeed(); g.linefeed(); g.linefeed(); g.linefeed();
        assert_eq!(g.cursor(), (0, 2));
    }

    #[test]
    fn backspace_decrements_column() {
        let mut g = Grid::new(80, 24);
        g.print('a'); g.print('b');
        assert_eq!(g.cursor(), (2, 0));
        g.backspace();
        assert_eq!(g.cursor(), (1, 0));
    }

    #[test]
    fn backspace_at_column_zero_stays() {
        let mut g = Grid::new(80, 24);
        g.backspace();
        assert_eq!(g.cursor(), (0, 0));
        g.linefeed();
        g.backspace();
        assert_eq!(g.cursor(), (0, 1)); // stays at col 0; row unchanged
    }

    #[test]
    fn backspace_does_not_erase_the_cell_under_cursor() {
        // xterm BS is non-destructive: it just moves the cursor.
        let mut g = Grid::new(80, 24);
        g.print('x');
        g.backspace();
        assert_eq!(g.cell(0, 0).ch, 'x', "BS must not clear the cell");
    }
}

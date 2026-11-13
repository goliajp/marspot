//! Writing cells, and the per-row bound that says where a row's
//! default cells start.
//!
//! A row that scrolls off is filed without its blank tail.  Finding
//! that tail by comparing cells from the right edge inwards cost a
//! compare per blank cell of every line; the bound hands the answer
//! over.  Every write raises it, a row filled with default cells
//! resets it, and anything that moves rows about either carries it
//! along or gives it up for the full width.

use super::{Cell, Grid};

impl Grid {
    /// Overwrite a single cell.  Caller is responsible for valid coords.
    pub fn set_cell(&mut self, col: u16, row: u16, cell: Cell) {
        debug_assert!(col < self.cols && row < self.rows);
        let pr = self.phys_row(row);
        self.cells[pr * self.cols as usize + col as usize] = cell;
        if col >= self.row_end[pr] {
            self.row_end[pr] = col + 1;
        }
    }

    /// The whole of `row`, and its bound for the caller to keep: after
    /// writing cells up to some column it must leave the bound at
    /// least there.  One ring-index resolve for a run of any length.
    pub fn row_and_end_mut(&mut self, row: u16) -> (&mut [Cell], &mut u16) {
        debug_assert!(row < self.rows);
        let pr = self.phys_row(row);
        let cols = self.cols as usize;
        (&mut self.cells[pr * cols..(pr + 1) * cols], &mut self.row_end[pr])
    }

    /// Rows were rebuilt wholesale: nothing is known about their tails.
    pub(super) fn forget_row_ends(&mut self) {
        self.row_end.clear();
        self.row_end.resize(self.rows as usize, self.cols);
    }

    /// Whether every row's cells from its bound on are default cells.
    /// What the bound promises; for tests and debug builds.
    pub fn row_ends_hold(&self) -> bool {
        let cols = self.cols as usize;
        self.row_end.len() == self.rows as usize
            && self.row_end.iter().enumerate().all(|(pr, &end)| {
                end as usize <= cols
                    && self.cells[pr * cols + end as usize..(pr + 1) * cols]
                        .iter()
                        .all(|c| *c == Cell::default())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Cell, CellAttrs, Color, Grid};

    /// xorshift: the test needs a spread of operations, not randomness
    fn next(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    #[test]
    fn the_bound_holds_through_writes_scrolls_and_resizes() {
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let red = Cell {
            ch: ' ',
            attrs: CellAttrs { bg: Color::indexed(1), ..CellAttrs::default() },
        };
        for (cols, rows) in [(7u16, 3u16), (40, 6), (2, 2)] {
            let mut g = Grid::new(cols, rows);
            for step in 0..20_000 {
                let (cols, rows) = (g.cols(), g.rows());
                let col = (next(&mut s) % cols as u64) as u16;
                let row = (next(&mut s) % rows as u64) as u16;
                let fill = if next(&mut s) % 3 == 0 { red } else { Cell::default() };
                match next(&mut s) % 16 {
                    0..=7 => g.set_cell(col, row, Cell::from('x')),
                    8 => g.set_cell(col, row, Cell::default()),
                    9 => {
                        let (cells, end) = g.row_and_end_mut(row);
                        for c in &mut cells[..=col as usize] {
                            *c = Cell::from('y');
                        }
                        *end = (*end).max(col + 1);
                    }
                    10 | 11 => g.scroll_up(1, fill),
                    12 => g.scroll_up_region(row.min(1), rows - 1, 1, fill),
                    13 => g.scroll_down_region(row, rows - 1, 1, fill),
                    14 => g.resize(cols, 2 + (next(&mut s) % 6) as u16),
                    _ => g.resize(2 + (next(&mut s) % 40) as u16, rows),
                }
                assert!(g.row_ends_hold(), "after step {step} at {cols}x{rows}");
            }
        }
    }
}

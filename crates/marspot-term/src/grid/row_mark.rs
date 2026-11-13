//! Writing cells, and what the grid keeps knowing about each row
//! while it is written.
//!
//! A row that scrolls off is filed without its blank tail, as runs of
//! cells that share their attributes.  Finding the tail by comparing
//! cells from the right edge inwards, and the runs by comparing every
//! cell's attributes with its neighbour's, cost two looks at every
//! cell of every line.  The marks hand over what the writes already
//! knew.  Every write raises them, a row filled with default cells
//! resets them, and anything that moves rows about either carries
//! them along or gives them up for the full width.

use super::{Cell, CellAttrs, Grid};

/// Upper bounds on where a row stops being interesting; never exact.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RowMark {
    /// Every cell at or past this column is `Cell::default()`.
    pub end: u16,
    /// Every cell at or past this column carries the default attributes.
    pub plain_from: u16,
    /// A character past 0x7F may be in the row.  A guess for whoever
    /// encodes the row, which is right either way.
    pub wide: bool,
}

impl RowMark {
    /// A row about which nothing is known.
    pub(super) fn unknown(cols: u16) -> Self {
        RowMark { end: cols, plain_from: cols, wide: false }
    }

    /// A row every cell of which is `fill`.
    pub(super) fn filled(fill: Cell, cols: u16) -> Self {
        if fill.is_default() { RowMark::default() } else { RowMark::unknown(cols) }
    }
}

impl CellAttrs {
    /// Whether these are the default attributes: one compare of the
    /// sixteen bytes, where `==` goes a field at a time.
    #[inline(always)]
    pub fn is_default(&self) -> bool {
        let words = self as *const CellAttrs as *const u64;
        // SAFETY: `CellAttrs` is `repr(C)`, sixteen bytes, with its
        // padding spelt out as a field and every byte initialised
        // (asserted beside `Cell`); the two reads cover exactly those
        // bytes and make no claim about alignment.
        unsafe { words.read_unaligned() | words.add(1).read_unaligned() == 0 }
    }
}

impl Cell {
    /// Whether this is `Cell::default()`.
    #[inline(always)]
    pub fn is_default(&self) -> bool {
        self.ch == ' ' && self.attrs.is_default()
    }
}

impl Grid {
    /// Overwrite a single cell.  Caller is responsible for valid coords.
    pub fn set_cell(&mut self, col: u16, row: u16, cell: Cell) {
        debug_assert!(col < self.cols && row < self.rows);
        let pr = self.phys_row(row);
        self.cells[pr * self.cols as usize + col as usize] = cell;
        let mark = &mut self.marks[pr];
        if col >= mark.end {
            mark.end = col + 1;
        }
        if col >= mark.plain_from && !cell.attrs.is_default() {
            mark.plain_from = col + 1;
        }
        mark.wide |= cell.ch as u32 >= 0x80;
    }

    /// The whole of `row`, and its marks for the caller to keep: after
    /// writing cells it must leave each bound at least where it wrote.
    /// One ring-index resolve for a run of any length.
    // as a call it cost seventeen instructions a run of text
    #[inline(always)]
    pub fn row_and_mark_mut(&mut self, row: u16) -> (&mut [Cell], &mut RowMark) {
        debug_assert!(row < self.rows);
        let pr = self.phys_row(row);
        let cols = self.cols as usize;
        (&mut self.cells[pr * cols..(pr + 1) * cols], &mut self.marks[pr])
    }

    /// Rows were rebuilt wholesale: nothing is known about them.
    pub(super) fn forget_row_marks(&mut self) {
        self.marks.clear();
        self.marks.resize(self.rows as usize, RowMark::unknown(self.cols));
    }

    /// Whether every row is what its marks promise: default cells from
    /// `end` on, default attributes from `plain_from` on.  For tests
    /// and debug builds.
    pub fn row_marks_hold(&self) -> bool {
        let cols = self.cols as usize;
        self.marks.len() == self.rows as usize
            && self.marks.iter().enumerate().all(|(pr, mark)| {
                let row = &self.cells[pr * cols..(pr + 1) * cols];
                let (end, plain) = (mark.end as usize, mark.plain_from as usize);
                end <= cols
                    && plain <= cols
                    && row[end..].iter().all(|c| *c == Cell::default())
                    && row[plain..].iter().all(|c| c.attrs == CellAttrs::default())
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

    /// The byte compare answers what `==` answers.
    #[test]
    fn the_default_attributes_are_told_from_any_others() {
        let plain = CellAttrs::default();
        assert!(plain.is_default() && Cell::default().is_default());
        let others = [
            CellAttrs { fg: Color::indexed(0), ..plain },
            CellAttrs { bg: Color::rgb(0, 0, 0), ..plain },
            CellAttrs { bold: true, ..plain },
            CellAttrs { italic: true, ..plain },
            CellAttrs { underline: true, ..plain },
            CellAttrs { reverse: true, ..plain },
            CellAttrs { dim: true, ..plain },
            CellAttrs { _pad: [super::super::FLAG_CLUSTER, 0, 0], ..plain },
        ];
        for attrs in others {
            assert_ne!(attrs, plain);
            assert!(!attrs.is_default(), "{attrs:?}");
            assert!(!Cell { ch: ' ', attrs }.is_default());
        }
        assert!(!Cell::from('x').is_default());
    }

    #[test]
    fn the_marks_hold_through_writes_scrolls_and_resizes() {
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let red = Cell {
            ch: ' ',
            attrs: CellAttrs { bg: Color::indexed(1), ..CellAttrs::default() },
        };
        for (cols, rows) in [(7u16, 3u16), (40, 6), (2, 2)] {
            let mut g = Grid::new(cols, rows);
            // an interpreter runs this a thousand times slower
            let steps = if cfg!(miri) { 400 } else { 20_000 };
            for step in 0..steps {
                let (cols, rows) = (g.cols(), g.rows());
                let col = (next(&mut s) % cols as u64) as u16;
                let row = (next(&mut s) % rows as u64) as u16;
                let fill = if next(&mut s).is_multiple_of(3) { red } else { Cell::default() };
                match next(&mut s) % 16 {
                    0..=6 => g.set_cell(col, row, Cell::from('x')),
                    7 => g.set_cell(col, row, red),
                    8 => g.set_cell(col, row, Cell::default()),
                    9 => {
                        let (cells, mark) = g.row_and_mark_mut(row);
                        for c in &mut cells[..=col as usize] {
                            *c = red;
                        }
                        mark.end = mark.end.max(col + 1);
                        mark.plain_from = mark.plain_from.max(col + 1);
                    }
                    10 | 11 => g.scroll_up(1, fill),
                    12 => g.scroll_up_region(row.min(1), rows - 1, 1, fill),
                    13 => g.scroll_down_region(row, rows - 1, 1, fill),
                    14 => g.resize(cols, 2 + (next(&mut s) % 6) as u16),
                    _ => g.resize(2 + (next(&mut s) % 40) as u16, rows),
                }
                assert!(g.row_marks_hold(), "after step {step} at {cols}x{rows}");
            }
        }
    }
}

//! One pane's strip: the rows it asks for, what they hold, and where
//! they are drawn.

use marspot::render::StripView;
use marspot_term::grid::{char_width, Cell, CellAttrs, Grid};

/// The most rows a strip may take.
pub(in super::super) const MAX_ROWS: u16 = 8;

/// Text at a column of a strip's row.
pub(in super::super) struct Run<'a> {
    pub col: u16,
    pub text: &'a str,
    pub attrs: CellAttrs,
}

pub(in super::super) struct Strip {
    grid: Grid,
    /// Changes whenever a cell of `grid` does.
    seq: u64,
    rows: u16,
    /// Where it is drawn, in rows from the terminal's first; `None`
    /// while it is not.
    top_row: Option<u16>,
    /// The last used row of a screen, by what it was worked out from:
    /// the session shown, its frame and its height.
    last_used: Option<((u64, u64, u16), u16)>,
}

/// Where a new strip's `seq` starts, so that one that takes another's
/// place is never taken for it.
fn first_seq() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1 << 40, std::sync::atomic::Ordering::Relaxed)
}

impl Strip {
    pub(super) fn new(rows: u16) -> Self {
        Self { grid: Grid::with_scrollback(1, rows, 0), seq: first_seq(), rows, top_row: None, last_used: None }
    }

    /// The rows it asks the layout for.
    pub(in super::super) fn rows(&self) -> u16 {
        self.rows
    }

    pub(in super::super) fn top_row(&self) -> Option<u16> {
        self.top_row
    }

    pub(super) fn set_rows(&mut self, rows: u16) {
        self.rows = rows;
    }

    /// Take the size the layout gave, and say whether that changed it:
    /// what it held is gone then.
    pub(super) fn fit(&mut self, cols: u16, rows: u16) -> bool {
        if (self.grid.cols(), self.grid.rows()) == (cols, rows) {
            return false;
        }
        self.grid = Grid::with_scrollback(cols, rows, 0);
        self.seq += 1;
        true
    }

    /// Make row `row` hold exactly `runs`, which are in column order.
    /// Only cells that differ are written, and nothing is allocated.
    pub(in super::super) fn line(&mut self, row: u16, runs: &[Run]) {
        if row >= self.grid.rows() {
            return;
        }
        let cols = self.grid.cols();
        let mut col = 0;
        for run in runs {
            while col < run.col.min(cols) {
                self.put(col, row, Cell::default());
                col += 1;
            }
            for ch in run.text.chars() {
                let width = char_width(ch) as u16;
                if width == 0 {
                    continue;
                }
                if col + width > cols {
                    break;
                }
                self.put(col, row, Cell { ch, attrs: run.attrs });
                if width == 2 {
                    self.put(col + 1, row, Cell { ch: '\0', attrs: run.attrs });
                }
                col += width;
            }
        }
        while col < cols {
            self.put(col, row, Cell::default());
            col += 1;
        }
    }

    fn put(&mut self, col: u16, row: u16, cell: Cell) {
        if self.grid.cell(col, row) != cell {
            self.grid.set_cell(col, row, cell);
            self.seq += 1;
        }
    }

    /// Work out where it goes over `screen`, the grid of session
    /// `shown` at frame `seq`: right under what the screen holds, or
    /// under its last row while it shows its history.  `rows` is the
    /// terminal's height in the layout.  A screen still taller than
    /// that has rows where the strip goes, and the strip waits.
    pub(in super::super) fn place(&mut self, screen: &Grid, shown: u64, seq: u64, scrolled: bool, rows: u16) {
        self.top_row = if screen.rows() > rows {
            None
        } else if scrolled {
            Some(rows)
        } else {
            Some((self.last_used_row(screen, shown, seq) + 1).min(rows))
        };
    }

    fn last_used_row(&mut self, screen: &Grid, shown: u64, seq: u64) -> u16 {
        let of = (shown, seq, screen.rows());
        match self.last_used {
            Some((was, row)) if was == of => row,
            _ => {
                let row = screen.last_used_row();
                self.last_used = Some((of, row));
                row
            }
        }
    }

    pub(super) fn hide(&mut self) {
        self.top_row = None;
    }

    pub(super) fn view(&self) -> Option<StripView<'_>> {
        Some(StripView { grid: &self.grid, seq: self.seq, top_row: self.top_row? })
    }

    #[cfg(test)]
    pub(super) fn grid(&self) -> &Grid {
        &self.grid
    }

    #[cfg(test)]
    pub(super) fn seq(&self) -> u64 {
        self.seq
    }
}

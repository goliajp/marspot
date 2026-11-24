//! Giving up the bottom rows of a pane's cells, and where a pane's
//! cells are.

use super::{next_generation, CellRect, Layout};

/// A pane with fewer rows than this gives none up.
pub const STRIP_MIN_TOTAL_ROWS: u16 = 12;

/// Where a pane's cells are, in whole pixels: the top left corner of
/// the first one, and the size of each.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellGrid {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl CellGrid {
    /// The column and row a point is in, which may be off any grid.
    pub fn at(&self, px: f64, py: f64) -> (i64, i64) {
        (((px - self.x) / self.w).floor() as i64, ((py - self.y) / self.h).floor() as i64)
    }

    pub fn row_top(&self, row: u16) -> f64 {
        self.y + row as f64 * self.h
    }
}

/// The cells of the pane at `rect`.  The one place this is worked out:
/// a font's cell is rarely a whole number of pixels, and drawing that
/// rounds it and a hit test that does not drift apart down the pane.
pub fn cell_grid(rect: &CellRect, title_h: f64, padding: f64, cell_w: f64, cell_h: f64) -> CellGrid {
    CellGrid {
        x: (rect.x as f32 + padding as f32).round() as f64,
        y: (rect.y_top as f32 + title_h as f32 + padding as f32).round() as f64,
        w: (cell_w as f32).round() as f64,
        h: (cell_h as f32).round() as f64,
    }
}

impl Layout {
    /// Give up the bottom `strips[i]` rows of pane `i`'s cells.  A pane
    /// too short for it gives up none, nor does one that would keep no
    /// row of its own.
    pub fn with_strips(mut self, strips: &[u16], cell_w: f64, cell_h: f64) -> Self {
        for (c, &want) in self.cells.iter_mut().zip(strips) {
            let total = c.rows + c.strip_rows;
            let given = if total >= STRIP_MIN_TOTAL_ROWS && want < total { want } else { 0 };
            c.strip_rows = given;
            c.rows = total - given;
            c.term_bottom = if given == 0 {
                c.y_top + c.h
            } else {
                cell_grid(c, self.cell_title_h, self.padding, cell_w, cell_h).row_top(c.rows)
            };
        }
        self.generation = next_generation();
        self
    }
}

#[cfg(test)]
mod helper_strip_tests;

//! Window layout: where each session and the sidebar live, in
//! physical pixels.
//!
//! mars carves the window into:
//!
//! ```text
//!   ┌─────────┬───────────────────────────┐
//!   │ sidebar │  N×M grid of session cells │
//!   │         │  ┌──────┬──────┬──────┐   │
//!   │         │  │ cell │ cell │ cell │   │
//!   │         │  ├──────┼──────┼──────┤   │
//!   │         │  │ cell │ cell │ cell │   │
//!   │         │  └──────┴──────┴──────┘   │
//!   └─────────┴───────────────────────────┘
//! ```
//!
//! Sizes are everywhere in **physical pixels** so the renderer can
//! consume them directly.  The caller passes `cell_w` / `cell_h`
//! (font cell metrics) so each session knows how many terminal
//! columns / rows actually fit in its physical sub-rect.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellRect {
    /// Physical-pixel x of the rect's left edge.
    pub x: f64,
    /// Physical-pixel y of the rect's TOP edge (y-down).  The renderer
    /// flips to y-up when computing baselines.
    pub y_top: f64,
    pub w: f64,
    pub h: f64,
    /// Terminal column count that fits in this rect at the configured
    /// font cell width.
    pub cols: u16,
    /// Terminal row count that fits.
    pub rows: u16,
}

#[derive(Clone, Debug)]
pub struct Layout {
    /// Total window dimensions in physical pixels.
    pub window_w: f64,
    pub window_h: f64,
    /// Sidebar width in physical pixels.  The sidebar occupies
    /// `[0, sidebar_w] × [0, window_h]`.  Set to 0 for no sidebar.
    pub sidebar_w: f64,
    /// `(cols, rows)` of the session grid layout itself (not the
    /// terminal cell grid inside one session — that's per-cell).
    pub grid_cols: usize,
    pub grid_rows: usize,
    /// The N session sub-rects, in row-major order.  Length is
    /// `grid_cols * grid_rows`.
    pub cells: Vec<CellRect>,
}

impl Layout {
    /// Build a layout for `grid_cols × grid_rows` sessions inside a
    /// window of `(window_w, window_h)` physical pixels, leaving a
    /// `sidebar_w`-wide strip on the left for chrome.
    ///
    /// `cell_w` / `cell_h` are the font's per-character cell metrics
    /// — used to compute how many terminal cols / rows fit in each
    /// sub-rect.
    pub fn build(
        window_w: f64,
        window_h: f64,
        sidebar_w: f64,
        grid_cols: usize,
        grid_rows: usize,
        cell_w: f64,
        cell_h: f64,
    ) -> Self {
        assert!(grid_cols > 0 && grid_rows > 0);
        let avail_w = (window_w - sidebar_w).max(0.0);
        let cell_phys_w = avail_w / grid_cols as f64;
        let cell_phys_h = window_h / grid_rows as f64;

        let mut cells = Vec::with_capacity(grid_cols * grid_rows);
        for r in 0..grid_rows {
            for c in 0..grid_cols {
                let x = sidebar_w + c as f64 * cell_phys_w;
                let y_top = r as f64 * cell_phys_h;
                let cols = ((cell_phys_w / cell_w).floor() as u16).max(1);
                let rows = ((cell_phys_h / cell_h).floor() as u16).max(1);
                cells.push(CellRect {
                    x,
                    y_top,
                    w: cell_phys_w,
                    h: cell_phys_h,
                    cols,
                    rows,
                });
            }
        }
        Self {
            window_w,
            window_h,
            sidebar_w,
            grid_cols,
            grid_rows,
            cells,
        }
    }

    /// Hit-test a click at physical coords `(px, py)`.  Returns the
    /// session index when the click landed inside one of the grid
    /// cells, else `None` (sidebar hit, or outside the window).  For
    /// sidebar clicks, see [`hit_test_sidebar_row`](Self::hit_test_sidebar_row).
    pub fn hit_test(&self, px: f64, py: f64) -> Option<usize> {
        if px < self.sidebar_w {
            return None;
        }
        for (i, c) in self.cells.iter().enumerate() {
            if px >= c.x && px < c.x + c.w && py >= c.y_top && py < c.y_top + c.h {
                return Some(i);
            }
        }
        None
    }

    /// Map a click in the sidebar to the session-list row index.
    /// Returns `None` if the click was outside the sidebar or above /
    /// below the entry list.  `row_height_phys` is the per-entry
    /// height the renderer used (in physical pixels), and `top_pad`
    /// is the gap between the window's top edge and the first entry.
    pub fn hit_test_sidebar_row(
        &self,
        px: f64,
        py: f64,
        top_pad: f64,
        row_height_phys: f64,
        rows: usize,
    ) -> Option<usize> {
        if px < 0.0 || px >= self.sidebar_w || py < top_pad {
            return None;
        }
        let idx = ((py - top_pad) / row_height_phys).floor() as usize;
        if idx >= rows {
            None
        } else {
            Some(idx)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_by_three_with_sidebar_uses_full_width() {
        let l = Layout::build(1440.0, 900.0, 200.0, 3, 3, 8.0, 16.0);
        assert_eq!(l.cells.len(), 9);

        // First column at x = sidebar_w; last at sidebar_w + 2 * cell_w.
        assert_eq!(l.cells[0].x, 200.0);
        let cell_w = (1440.0 - 200.0) / 3.0;
        assert!((l.cells[2].x - (200.0 + 2.0 * cell_w)).abs() < 1e-6);

        // Per-cell terminal grid: floor(cell_w / 8) cols, floor(cell_h / 16) rows.
        assert_eq!(l.cells[0].cols, (cell_w / 8.0).floor() as u16);
        assert_eq!(l.cells[0].rows, ((900.0_f64 / 3.0) / 16.0).floor() as u16);
    }

    #[test]
    fn hit_test_picks_correct_cell_or_sidebar() {
        let l = Layout::build(1000.0, 600.0, 200.0, 2, 2, 8.0, 16.0);
        // Click in sidebar
        assert_eq!(l.hit_test(50.0, 300.0), None);
        // Click in top-left cell
        assert_eq!(l.hit_test(250.0, 50.0), Some(0));
        // Click in top-right cell
        assert_eq!(l.hit_test(800.0, 50.0), Some(1));
        // Click in bottom-left cell
        assert_eq!(l.hit_test(250.0, 400.0), Some(2));
        // Click in bottom-right cell
        assert_eq!(l.hit_test(800.0, 400.0), Some(3));
    }

    #[test]
    fn zero_sidebar_means_grid_uses_whole_window() {
        let l = Layout::build(900.0, 600.0, 0.0, 3, 1, 8.0, 16.0);
        assert_eq!(l.cells[0].x, 0.0);
        assert!((l.cells[2].x - 600.0).abs() < 1e-6);
    }
}

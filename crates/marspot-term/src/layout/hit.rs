//! From a point of the window to what is there, and back.

use super::{cell_grid, Layout};

impl Layout {
    /// Hit-test a click at physical coords `(px, py)`.  Returns the
    /// session index when the click landed inside one of the grid
    /// cells, else `None` (sidebar hit, or outside the window).
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

    /// Map a click in the terminal-content area of a cell to its
    /// `(session_idx, col, row)` cell coordinates.  Returns `None`
    /// when the click is in the sidebar, the title strip, the
    /// padding band, below the terminal's rows in a pane that gives
    /// some up, or outside the window.  Used by the selection
    /// machinery to anchor / extend a selection on mouse drag.
    /// `cell_w` / `cell_h` are the per-glyph monospace metrics in
    /// physical pixels; the renderer is the source of truth for
    /// these so the caller passes them through.
    pub fn hit_test_cell_pos(
        &self,
        px: f64,
        py: f64,
        cell_w: f64,
        cell_h: f64,
    ) -> Option<(usize, u16, u16)> {
        if px < self.sidebar_w {
            return None;
        }
        for (i, c) in self.cells.iter().enumerate() {
            if px < c.x || px >= c.x + c.w || py < c.y_top || py >= c.y_top + c.h {
                continue;
            }
            // Inside the rect — discard hits in the title strip and
            // the surrounding padding so the selection only anchors
            // on actual terminal content.
            let g = cell_grid(c, self.cell_title_h, self.padding, cell_w, cell_h);
            if py < g.y || px < g.x {
                return None;
            }
            let (col, row) = g.at(px, py);
            // Clamp to the cell's reported terminal dims so a
            // drag past the right / bottom edge doesn't escape.
            // Rows given up at the bottom are nobody's: a point on
            // them is not the last row's.
            if c.strip_rows > 0 && row >= c.rows as i64 {
                return None;
            }
            let col = (col as u16).min(c.cols.saturating_sub(1));
            let row = (row as u16).min(c.rows.saturating_sub(1));
            return Some((i, col, row));
        }
        None
    }

    /// Inverse of `hit_test_cell_pos`: given a cell index + `(col,
    /// row)` inside that cell's terminal grid, return the physical-pixel
    /// rect that one character cell occupies in view-local top-left
    /// coordinates (y-down — same space the renderer paints in).
    /// Returns `None` when `cell_idx` is out of range.  Used to
    /// publish the focused caret to the IME
    /// (`MarspotAppCtx::set_caret_rect_phys`).
    pub fn caret_view_phys_rect(
        &self,
        cell_idx: usize,
        col: u16,
        row: u16,
        cell_w: f64,
        cell_h: f64,
    ) -> Option<(f64, f64, f64, f64)> {
        let c = self.cells.get(cell_idx)?;
        let g = cell_grid(c, self.cell_title_h, self.padding, cell_w, cell_h);
        Some((g.x + col as f64 * g.w, g.y + row as f64 * g.h, g.w, g.h))
    }

    /// Which row of which pane's strip is at `(px, py)`.  `top_row` says
    /// where a pane's strip is drawn, as a row of the pane counted from
    /// its terminal's first, or `None` when it is not drawn.
    pub fn hit_test_strip(
        &self,
        px: f64,
        py: f64,
        cell_w: f64,
        cell_h: f64,
        top_row: impl Fn(usize) -> Option<u16>,
    ) -> Option<(usize, u16)> {
        let i = self.hit_test(px, py)?;
        let c = &self.cells[i];
        if c.strip_rows == 0 {
            return None;
        }
        let g = cell_grid(c, self.cell_title_h, self.padding, cell_w, cell_h);
        let row = g.at(px, py).1 - top_row(i)? as i64;
        (0..c.strip_rows as i64).contains(&row).then_some((i, row as u16))
    }
}

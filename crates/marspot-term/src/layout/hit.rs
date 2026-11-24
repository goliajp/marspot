//! From a point of the window to what is there, and back.

use super::Layout;

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
    /// padding band, or outside the window.  Used by the selection
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
            let inner_x = c.x + self.padding;
            let inner_y = c.y_top + self.cell_title_h + self.padding;
            if py < inner_y {
                return None;
            }
            let dx = px - inner_x;
            let dy = py - inner_y;
            if dx < 0.0 {
                return None;
            }
            let col = (dx / cell_w).floor().max(0.0) as u16;
            let row = (dy / cell_h).floor().max(0.0) as u16;
            // Clamp to the cell's reported terminal dims so a
            // drag past the right / bottom edge doesn't escape.
            let col = col.min(c.cols.saturating_sub(1));
            let row = row.min(c.rows.saturating_sub(1));
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
        let inner_x = c.x + self.padding;
        let inner_y = c.y_top + self.cell_title_h + self.padding;
        let x = inner_x + col as f64 * cell_w;
        let y = inner_y + row as f64 * cell_h;
        Some((x, y, cell_w, cell_h))
    }
}

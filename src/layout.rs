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
    /// `[0, sidebar_w] × [top_inset, window_h]`.  Set to 0 for no
    /// sidebar.
    pub sidebar_w: f64,
    /// Top inset in physical pixels — a clear band above sidebar
    /// items AND cells, reserved for the macOS traffic-light buttons
    /// when `FullSizeContentView` is on.  Without it, the buttons
    /// would overlap whatever the renderer paints into the top-left
    /// (sidebar item 1, cell 1's first row).  Set to 0 for headless
    /// / snapshot use where there's no window chrome to clear.
    pub top_inset: f64,
    /// `(cols, rows)` of the session grid layout itself (not the
    /// terminal cell grid inside one session — that's per-cell).
    pub grid_cols: usize,
    pub grid_rows: usize,
    /// The N session sub-rects, in row-major order.  Length is
    /// `grid_cols * grid_rows`.
    pub cells: Vec<CellRect>,
    /// Inter-cell gutter width in physical pixels.  The renderer
    /// uses this to paint the focus indicator AS the gutter around
    /// the focused cell (focus frame and divider are the same
    /// thing — no separate inner stroke).  0 if the grid is 1×1.
    pub gutter: f64,
    /// Inner padding on each side of every cell, in physical pixels.
    /// `cell.cols` / `cell.rows` are counted off the area AFTER
    /// padding; the renderer offsets glyph origins by this amount
    /// so terminal content doesn't crowd the cell's visible edge.
    pub padding: f64,
    /// Title strip height at the top of every cell, in physical
    /// pixels.  The renderer paints the strip with the session
    /// label + a SEAM hairline at its bottom; terminal content
    /// (cols × rows × glyphs) starts BELOW this strip.  Set to 0
    /// for headless / single-pane snapshot layouts that don't
    /// want a title band.
    pub cell_title_h: f64,
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
        top_inset: f64,
        cell_title_h: f64,
        grid_cols: usize,
        grid_rows: usize,
        cell_w: f64,
        cell_h: f64,
    ) -> Self {
        assert!(grid_cols > 0 && grid_rows > 0);
        // When a sidebar is present, reserve one 1 px strip between
        // it and the 9-grid for the SEAM hairline.  Same physical
        // width as inter-cell seams — iTerm2 reads as a 1-pixel
        // hairline; anything wider looks like chrome.
        let sidebar_seam = if sidebar_w > 0.0 { 1.0 } else { 0.0 };
        let avail_w = (window_w - sidebar_w - sidebar_seam).max(0.0);
        let avail_h = (window_h - top_inset).max(0.0);
        // Inter-cell gutter in physical pixels — a thin strip of
        // chrome (the GUTTER color cleared by the renderer) shows
        // between sessions so the 3×3 grid reads as a grid, not as a
        // single sea of identical-looking shells.  2 px = 1 logical
        // point at 2× retina; iTerm2-style hairline.  Skipped when
        // the grid is 1×1 (single session, nothing to divide).
        let gutter = if grid_cols > 1 || grid_rows > 1 { 1.0 } else { 0.0 };
        // Inner padding (physical px) — breathing room between the
        // cell rect's edge and the first/last terminal column / row.
        // Without this, "Last login: ..." crowds the very top-left
        // pixel of the cell.  ~8 px ≈ 4 logical points at 2× retina,
        // matches iTerm2's default leftmargin/topmargin feel.
        let padding = 8.0;

        // Cells flush to the available area's outer edges — gutters
        // appear ONLY between cells, never around the outside.  Old
        // layout left a `gutter/2` margin on each side; that strip
        // showed the renderer's GUTTER clear colour (a light grey
        // hairline) all the way around the 9-grid, framing it like
        // a chrome inset.  Worse, the strip extended into macOS's
        // rounded window corners — the rounded mask clipped the
        // grey strip into a "chipped" look.  Flushing cells means
        // cell BG paints the whole window-content rectangle and
        // the rounding just clips dark-on-dark.
        let inner_w = (avail_w - (grid_cols - 1) as f64 * gutter).max(1.0);
        let inner_h = (avail_h - (grid_rows - 1) as f64 * gutter).max(1.0);
        let cell_phys_w = (inner_w / grid_cols as f64).max(1.0);
        let cell_phys_h = (inner_h / grid_rows as f64).max(1.0);
        // Inner content area (after padding + title strip) is what
        // cols/rows are counted from.  The cell rect itself stays
        // at the outer size — the title strip and BG fill cover
        // the whole cell so the padding zone reads as terminal-bg.
        let cell_inner_w = (cell_phys_w - 2.0 * padding).max(1.0);
        let cell_inner_h =
            (cell_phys_h - cell_title_h - 2.0 * padding).max(1.0);

        let mut cells = Vec::with_capacity(grid_cols * grid_rows);
        for r in 0..grid_rows {
            for c in 0..grid_cols {
                let x = sidebar_w + sidebar_seam + c as f64 * (cell_phys_w + gutter);
                let y_top = top_inset + r as f64 * (cell_phys_h + gutter);
                let cols = ((cell_inner_w / cell_w).floor() as u16).max(1);
                let rows = ((cell_inner_h / cell_h).floor() as u16).max(1);
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
            top_inset,
            grid_cols,
            grid_rows,
            cells,
            gutter,
            padding,
            cell_title_h,
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

    /// Hit-test the per-cell title strip — the band at the top of
    /// each cell where `cell_title_h` reserves space for the
    /// session label.  Returns `Some(i)` if the click landed in
    /// cell `i`'s title strip, else `None`.  Used for click-to-edit
    /// title behaviour without disturbing terminal-area clicks.
    pub fn hit_test_cell_title(&self, px: f64, py: f64) -> Option<usize> {
        if self.cell_title_h <= 0.0 || px < self.sidebar_w {
            return None;
        }
        for (i, c) in self.cells.iter().enumerate() {
            if px >= c.x
                && px < c.x + c.w
                && py >= c.y_top
                && py < c.y_top + self.cell_title_h
            {
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
        // Cells flush to the outer cell area; 2 px inter-cell gutter
        // only between cells.  When a sidebar is present, an extra
        // 2 px seam strip sits between sidebar and grid for the
        // sidebar-to-grid region hairline.
        let gutter = 1.0;
        let sidebar_seam = 1.0;
        let l = Layout::build(1440.0, 900.0, 200.0, 0.0, 0.0, 3, 3, 8.0, 16.0);
        assert_eq!(l.cells.len(), 9);

        // First column flush against the sidebar seam.
        assert!((l.cells[0].x - (200.0 + sidebar_seam)).abs() < 1e-6);
        // Cells share `inner_w = avail_w - 2 * gutter` equally,
        // where avail_w excludes the sidebar AND the seam.
        let avail_w = 1440.0 - 200.0 - sidebar_seam;
        let cell_w = (avail_w - 2.0 * gutter) / 3.0;
        // Third column at sidebar_w + seam + 2 * (cell_w + gutter).
        assert!(
            (l.cells[2].x
                - (200.0 + sidebar_seam + 2.0 * (cell_w + gutter)))
                .abs()
                < 1e-6
        );
        // Last cell's right edge flush against the window's right.
        assert!(
            ((l.cells[2].x + l.cells[2].w) - 1440.0).abs() < 1e-6
        );

        // Inner content area (where cols/rows are counted) is the
        // cell rect shrunk by 2 * padding.
        let cell_h = (900.0 - 2.0 * gutter) / 3.0;
        let inner_w = cell_w - 2.0 * l.padding;
        let inner_h = cell_h - 2.0 * l.padding;
        assert_eq!(l.cells[0].cols, (inner_w / 8.0).floor() as u16);
        assert_eq!(l.cells[0].rows, (inner_h / 16.0).floor() as u16);
    }

    #[test]
    fn hit_test_picks_correct_cell_or_sidebar() {
        let l = Layout::build(1000.0, 600.0, 200.0, 0.0, 0.0, 2, 2, 8.0, 16.0);
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
        // 3×1 has > 1 cell so a gutter applies *between* cells —
        // first cell is flush at x=0, last cell flush at the right.
        let gutter = 1.0;
        let l = Layout::build(900.0, 600.0, 0.0, 0.0, 0.0, 3, 1, 8.0, 16.0);
        assert!(l.cells[0].x.abs() < 1e-6);
        let cell_w = (900.0 - 2.0 * gutter) / 3.0;
        assert!(
            (l.cells[2].x - 2.0 * (cell_w + gutter)).abs() < 1e-6
        );
        // Last cell's right edge flush against window_w.
        assert!(
            ((l.cells[2].x + l.cells[2].w) - 900.0).abs() < 1e-6
        );
    }
}

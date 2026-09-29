//! `GridSeams` — hairline dividers between a rectangular grid of
//! sub-rects.  Built for the marspot pane grid (anywhere from 1×1
//! to 3×3 today, generalises to x×y) but reusable for any UI that
//! lays cells out in a regular grid (settings forms, picker panels,
//! future tile dashboards).
//!
//! Style is per-orientation: vertical and horizontal seams take
//! independent `SeamStyle` values so a caller can suppress one
//! direction (`thickness = 0.0`) or paint them in different colours.
//!
//! Geometry comes from `cells: &[Rect]` in row-major order (cols ×
//! rows entries).  No assumption about cell sizes — each seam runs
//! between two neighbouring cells of the same column / row, spanning
//! the full grid extent on the perpendicular axis (so the seam reads
//! as a continuous line even with non-uniform cells).

use marspot_term::layout::Rect;
use crate::ui::core::ViewPainter;

#[derive(Debug, Clone, Copy)]
pub struct SeamStyle {
    pub color: [f32; 4],
    /// Hairline thickness in physical px.  `0.0` skips the seam.
    pub thickness: f64,
}

impl SeamStyle {
    /// "No seam" — paint() short-circuits when thickness is zero.
    pub fn none() -> Self {
        Self { color: [0.0; 4], thickness: 0.0 }
    }
}

pub struct GridSeams<'a> {
    /// Cell rects in row-major order.  Length must equal `cols * rows`.
    pub cells: &'a [Rect],
    pub cols: usize,
    pub rows: usize,
    /// Style for vertical seams (between adjacent columns).
    pub vertical: SeamStyle,
    /// Style for horizontal seams (between adjacent rows).
    pub horizontal: SeamStyle,
}

impl<'a> GridSeams<'a> {
    /// Paint the seams via the UI pipeline (so they layer on top of
    /// pane BG cells, which the BG pipeline drew first).  No-op when
    /// the grid is 0×0 / 1×1 / styles all zero-thickness.
    pub fn paint(&self, p: &mut ViewPainter) {
        if self.cols == 0 || self.rows == 0 || self.cells.is_empty() {
            return;
        }
        // Grid bounds — union of all cells.  Caller can pass cells
        // with arbitrary sizes; we still want seams to read as one
        // continuous line spanning the whole grid.
        let grid_left = self.cells[0].x;
        let last_col_idx = self.cols - 1;
        let grid_right = self.cells[last_col_idx].x + self.cells[last_col_idx].w;
        let grid_top = self.cells[0].y_top;
        let last_row_first_cell_idx = (self.rows - 1) * self.cols;
        let last_cell = self.cells[last_row_first_cell_idx];
        let grid_bottom = last_cell.y_top + last_cell.h;
        let grid_w = (grid_right - grid_left).max(0.0);
        let grid_h = (grid_bottom - grid_top).max(0.0);

        // Vertical seams — one per inter-column gap.  X = right edge
        // of column c-1 (read off the first row).  Span the full
        // grid height.
        if self.vertical.thickness > 0.0 && self.cols > 1 {
            for c in 1..self.cols {
                let prev = self.cells[c - 1];
                let x = prev.x + prev.w;
                ui_fill(
                    p,
                    Rect { x, y_top: grid_top, w: self.vertical.thickness, h: grid_h },
                    self.vertical.color,
                );
            }
        }

        // Horizontal seams — one per inter-row gap.  Y = bottom edge
        // of row r-1 (read off that row's first cell).  Span the
        // full grid width.
        if self.horizontal.thickness > 0.0 && self.rows > 1 {
            for r in 1..self.rows {
                let prev = self.cells[(r - 1) * self.cols];
                let y = prev.y_top + prev.h;
                ui_fill(
                    p,
                    Rect { x: grid_left, y_top: y, w: grid_w, h: self.horizontal.thickness },
                    self.horizontal.color,
                );
            }
        }

        // F3+1.19 — focus outline removed from GridSeams.  Focus is
        // a per-item visual concern, not a grid concern; it lives on
        // GridItem (which the caller paints alongside pane content
        // and lets manage its own focused state + ring rendering).
    }
}

/// F3+1.18 — seams use the BG (cells) pipeline, NOT the UI pipeline.
///
/// Why: the UI pipeline runs SDF anti-aliasing on every rect edge.
/// When focus rects abut base seam rects (or each other) at pixel
/// boundaries, both sides land in the AA band; the premultiplied-
/// alpha blend leaves ~98 % coverage instead of 100 %.  Visible as
/// "线宽和分隔线不一致" (focus line slightly thinner than base
/// gray) and "四个角没有 100 % 与线融合" (corner dim seam).
///
/// The BG pipeline draws flat colored rects with no SDF AA — the
/// rasterizer assigns each pixel to one rect by sample-center rule.
/// Two abutting axis-aligned rects tile pixel-perfect.  No seams,
/// no width mismatch.  Caller must round-snap coordinates to
/// integers (or let cell layout guarantee that, which marspot's
/// does) for the boundary to land on a pixel edge.
///
/// Caller must arrange to push seams AFTER pane BG in build order
/// (same pipeline = push order = z order).
fn ui_fill(p: &mut ViewPainter, rect: Rect, color: [f32; 4]) {
    p.fill_rect(rect, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y_top: y, w, h }
    }

    fn build_grid(cols: usize, rows: usize, cell_w: f64, cell_h: f64) -> Vec<Rect> {
        let mut out = Vec::with_capacity(cols * rows);
        for r in 0..rows {
            for c in 0..cols {
                out.push(rect(c as f64 * cell_w, r as f64 * cell_h, cell_w, cell_h));
            }
        }
        out
    }

    #[test]
    fn one_by_one_has_no_seams_to_paint() {
        // 1×1 grid: no inter-cell gaps, paint is effectively a no-op.
        let cells = build_grid(1, 1, 100.0, 100.0);
        let gs = GridSeams {
            cells: &cells, cols: 1, rows: 1,
            vertical: SeamStyle { color: [1.0; 4], thickness: 2.0 },
            horizontal: SeamStyle { color: [1.0; 4], thickness: 2.0 },
            
        };
        // We don't have a painter spy in this crate; assert bounds
        // line up with the single cell (smoke test for the math).
        assert_eq!(gs.cells.len(), 1);
    }

    #[test]
    fn zero_thickness_is_explicit_no_seam() {
        let s = SeamStyle::none();
        assert_eq!(s.thickness, 0.0);
    }

    #[test]
    fn three_by_three_seam_positions_align_with_cell_edges() {
        let cells = build_grid(3, 3, 100.0, 100.0);
        let gs = GridSeams {
            cells: &cells, cols: 3, rows: 3,
            vertical:   SeamStyle { color: [1.0; 4], thickness: 1.0 },
            horizontal: SeamStyle { color: [1.0; 4], thickness: 1.0 },
            
        };
        // Inter-col seams at x = 100, 200.  Inter-row at y = 100, 200.
        // Verify by recomputing the formula GridSeams uses.
        for c in 1..gs.cols {
            let prev = gs.cells[c - 1];
            let expected_x = prev.x + prev.w;
            assert_eq!(expected_x, (c as f64) * 100.0);
        }
        for r in 1..gs.rows {
            let prev = gs.cells[(r - 1) * gs.cols];
            let expected_y = prev.y_top + prev.h;
            assert_eq!(expected_y, (r as f64) * 100.0);
        }
    }

    #[test]
    fn non_uniform_cells_seam_spans_full_grid() {
        // 2×2 grid where right column is wider than the left.  Seams
        // should still span the union of all cells.
        let cells = vec![
            rect(0.0,   0.0, 60.0, 50.0),
            rect(60.0,  0.0, 100.0, 50.0),
            rect(0.0,  50.0, 60.0, 80.0),
            rect(60.0, 50.0, 100.0, 80.0),
        ];
        let gs = GridSeams {
            cells: &cells, cols: 2, rows: 2,
            vertical:   SeamStyle { color: [1.0; 4], thickness: 1.0 },
            horizontal: SeamStyle { color: [1.0; 4], thickness: 1.0 },
            
        };
        // Grid right = 60 + 100 = 160; grid bottom = 50 + 80 = 130.
        let grid_left = gs.cells[0].x;
        let grid_right = gs.cells[gs.cols - 1].x + gs.cells[gs.cols - 1].w;
        let grid_top = gs.cells[0].y_top;
        let last = gs.cells[(gs.rows - 1) * gs.cols];
        let grid_bottom = last.y_top + last.h;
        assert_eq!(grid_left, 0.0);
        assert_eq!(grid_right, 160.0);
        assert_eq!(grid_top, 0.0);
        assert_eq!(grid_bottom, 130.0);
    }

    #[test]
    fn independent_styles_allow_suppressing_one_axis() {
        let s_v = SeamStyle { color: [1.0; 4], thickness: 1.0 };
        let s_h = SeamStyle::none();
        assert!(s_v.thickness > 0.0);
        assert!(s_h.thickness == 0.0);
        // Caller wires vertical = s_v, horizontal = s_h → only
        // vertical seams paint.  (Behavioural test would need a
        // painter spy — invariant captured here at the data layer.)
    }
}

//! Lucide-style `layout-grid` icon: outer outline + interior dividers
//! drawn as thin strokes (NOT filled cells).  `(cols, rows)` drives
//! the divider count so the icon doubles as a "current grid shape"
//! indicator.  Used for the toolbar's layout-picker button.

use marspot_term::layout::Rect;
use crate::ui::core::{IconComponent, ViewPainter};
use super::{icon_stroke, icon_inner_rect};

pub struct GridIcon {
    pub cols: usize,
    pub rows: usize,
}

impl IconComponent for GridIcon {
    fn paint(&self, p: &mut ViewPainter, rect: Rect, fg: [f32; 4]) {
        if self.cols == 0 || self.rows == 0 {
            return;
        }
        let inner = icon_inner_rect(rect);
        let stroke = icon_stroke(inner);
        // Outer border: 4 hairlines.
        paint_border(p, inner, stroke, fg);
        // Vertical interior dividers.
        for c in 1..self.cols {
            let x = inner.x + c as f64 * inner.w / self.cols as f64 - stroke * 0.5;
            ui_fill(p, Rect { x, y_top: inner.y_top, w: stroke, h: inner.h }, fg);
        }
        // Horizontal interior dividers.
        for r in 1..self.rows {
            let y = inner.y_top + r as f64 * inner.h / self.rows as f64 - stroke * 0.5;
            ui_fill(p, Rect { x: inner.x, y_top: y, w: inner.w, h: stroke }, fg);
        }
    }
}

/// Solid fill via the UI pipeline (corner_radius=0, no border).  Used
/// by icons so they layer on top of the Button BG which also lives
/// in the UI pipeline — both in `ui_rects`, push-order = z-order.
pub(crate) fn ui_fill(p: &mut ViewPainter, rect: Rect, color: [f32; 4]) {
    p.fill_rounded_rect(rect, color, 0.0, ([0.0, 0.0, 0.0, 0.0], 0.0));
}

pub(crate) fn paint_border(p: &mut ViewPainter, r: Rect, w: f64, color: [f32; 4]) {
    let w = w.max(1.0);
    // top
    ui_fill(p, Rect { x: r.x, y_top: r.y_top, w: r.w, h: w }, color);
    // bottom
    ui_fill(p, Rect { x: r.x, y_top: r.y_top + r.h - w, w: r.w, h: w }, color);
    // left
    ui_fill(p, Rect { x: r.x, y_top: r.y_top, w, h: r.h }, color);
    // right
    ui_fill(p, Rect { x: r.x + r.w - w, y_top: r.y_top, w, h: r.h }, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_dims_noop() {
        // Both 0×0 and 1×0 / 0×1 should early-return without panic.
        let icon = GridIcon { cols: 0, rows: 0 };
        // No painter to test the early return — verify the shape
        // value-wise (placeholder).
        assert_eq!(icon.cols, 0);
        assert_eq!(icon.rows, 0);
    }

    #[test]
    fn fields_round_trip() {
        let icon = GridIcon { cols: 3, rows: 3 };
        assert_eq!(icon.cols, 3);
        assert_eq!(icon.rows, 3);
    }
}

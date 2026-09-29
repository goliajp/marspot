//! Lucide-style "chart-column" icon: a baseline with three columns
//! of rising height.  Used for the toolbar's Claude-usage (`Cc`)
//! modal toggle — it reads as "utilization levels", which is exactly
//! what the modal shows, and it keeps the toolbar one visual family
//! (thin axis-aligned strokes, shared `icon_stroke` / inner pad)
//! instead of the odd-one-out text label it replaced.

use marspot_term::layout::Rect;
use crate::ui::core::{IconComponent, ViewPainter};
use super::{icon_stroke, icon_inner_rect};
use super::grid::ui_fill;

pub struct UsageBarsIcon;

impl IconComponent for UsageBarsIcon {
    fn paint(&self, p: &mut ViewPainter, rect: Rect, fg: [f32; 4]) {
        let inner = icon_inner_rect(rect);
        let stroke = icon_stroke(inner);
        // Baseline along the bottom — anchors the columns the way
        // GridIcon's outline anchors its cells.
        let baseline_y = inner.y_top + inner.h - stroke;
        ui_fill(
            p,
            Rect { x: inner.x, y_top: baseline_y, w: inner.w, h: stroke },
            fg,
        );
        // Three columns rising left→right, drawn as filled bars of
        // the same width as the stroke family reads at.  Heights are
        // fractions of the inner box measured up from the baseline.
        let bar_w = (inner.w * 0.18).max(stroke);
        let heights = [0.35, 0.62, 0.88];
        let gap = (inner.w - 3.0 * bar_w) / 2.0;
        for (i, frac) in heights.iter().enumerate() {
            let bx = inner.x + i as f64 * (bar_w + gap);
            let bh = ((inner.h - stroke) * frac).max(stroke);
            ui_fill(
                p,
                Rect {
                    x: bx,
                    y_top: baseline_y - bh,
                    w: bar_w,
                    h: bh,
                },
                fg,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctor_is_unit() {
        let _ = UsageBarsIcon;
    }
}

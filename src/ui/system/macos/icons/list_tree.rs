//! Lucide-style "list-tree" icon: three horizontal bars, the lower
//! two indented to suggest nesting.  Used for the toolbar's process-
//! tree panel toggle.

use marspot_term::layout::Rect;
use crate::ui::core::{IconComponent, ViewPainter};
use super::{icon_stroke, icon_inner_rect};
use super::grid::ui_fill;

pub struct ListTreeIcon;

impl IconComponent for ListTreeIcon {
    fn paint(&self, p: &mut ViewPainter, rect: Rect, fg: [f32; 4]) {
        let inner = icon_inner_rect(rect);
        let stroke = icon_stroke(inner);
        // Vertical centers + horizontal indent + length, normalised
        // to inner.h / inner.w.
        let bar_y_offsets = [0.20, 0.50, 0.80];
        let bar_x_indents = [0.00, 0.20, 0.20];
        let bar_lengths   = [0.75, 0.55, 0.55];
        for ((y_o, x_i), len) in bar_y_offsets.iter()
            .zip(bar_x_indents.iter())
            .zip(bar_lengths.iter())
        {
            let by = inner.y_top + inner.h * y_o - stroke * 0.5;
            let bx = inner.x + inner.w * x_i;
            let bw = (inner.w * len).max(1.0);
            ui_fill(p, Rect { x: bx, y_top: by, w: bw, h: stroke }, fg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctor_is_unit() {
        let _ = ListTreeIcon;
    }
}

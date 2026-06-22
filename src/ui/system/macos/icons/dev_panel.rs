//! Lucide-style "panels-right" icon — a square with the right
//! third subdivided by a vertical bar.  Used for the dev-panel
//! toolbar toggle.  Reads as "there's a side panel" the way
//! sidebar / process-tree icons read as "there's a side list."

use marspot_term::layout::Rect;
use crate::ui::core::{IconComponent, ViewPainter};
use super::{icon_stroke, icon_inner_rect};
use super::grid::ui_fill;

pub struct DevPanelIcon;

impl IconComponent for DevPanelIcon {
    fn paint(&self, p: &mut ViewPainter, rect: Rect, fg: [f32; 4]) {
        let inner = icon_inner_rect(rect);
        let stroke = icon_stroke(inner);
        // Outer rounded-square outline drawn as 4 strokes (matches the
        // stroke-only convention used by SidebarIcon / GridIcon).
        // Top / Bottom / Left / Right.
        let top    = Rect { x: inner.x, y_top: inner.y_top, w: inner.w, h: stroke };
        let bottom = Rect { x: inner.x, y_top: inner.y_top + inner.h - stroke, w: inner.w, h: stroke };
        let left   = Rect { x: inner.x, y_top: inner.y_top, w: stroke, h: inner.h };
        let right  = Rect { x: inner.x + inner.w - stroke, y_top: inner.y_top, w: stroke, h: inner.h };
        ui_fill(p, top, fg);
        ui_fill(p, bottom, fg);
        ui_fill(p, left, fg);
        ui_fill(p, right, fg);
        // Vertical divider at the 2/3 mark (right third = the
        // panel).
        let divider_x = inner.x + inner.w * (2.0 / 3.0) - stroke * 0.5;
        ui_fill(
            p,
            Rect { x: divider_x, y_top: inner.y_top, w: stroke, h: inner.h },
            fg,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctor_is_unit() {
        let _ = DevPanelIcon;
    }
}

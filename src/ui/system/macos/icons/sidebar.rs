//! Lucide-style `panel-left` icon: outer outline + a single vertical
//! divider at ~1/3 of the inner width.  When `collapsed`, the frame
//! dims and the divider stays bright — the icon doubles as a state
//! indicator ("sidebar showing" vs "sidebar hidden").

use marspot_term::layout::Rect;
use crate::ui::core::{IconComponent, ViewPainter};
use super::{icon_stroke, icon_inner_rect};
use super::grid::{paint_border, ui_fill};

pub struct SidebarIcon {
    pub collapsed: bool,
}

impl IconComponent for SidebarIcon {
    fn paint(&self, p: &mut ViewPainter, rect: Rect, fg: [f32; 4]) {
        let inner = icon_inner_rect(rect);
        let stroke = icon_stroke(inner);
        let frame_color = if self.collapsed {
            [fg[0] * 0.55, fg[1] * 0.55, fg[2] * 0.55, fg[3]]
        } else {
            fg
        };
        paint_border(p, inner, stroke, frame_color);
        // Vertical divider at ~1/3 — stays bright even when collapsed
        // so the "this is a sidebar toggle" affordance reads.
        let div_x = inner.x + (inner.w / 3.0).round() - stroke * 0.5;
        ui_fill(p, Rect { x: div_x, y_top: inner.y_top, w: stroke, h: inner.h }, fg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapsed_flag_round_trips() {
        let on = SidebarIcon { collapsed: true };
        let off = SidebarIcon { collapsed: false };
        assert!(on.collapsed);
        assert!(!off.collapsed);
    }
}

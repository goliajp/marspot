//! macOS-style window-chrome traffic lights: red close, yellow
//! minimize, green maximize.  Drawn as small filled discs (SDF
//! corner_radius = size/2) anchored to the left edge of a title bar.

use marspot_term::layout::Rect;
use crate::render_metal::UiRectInstance;

/// Layout: 3 dots anchored left.  Rects use physical px.
pub struct TrafficLights {
    pub close: Rect,
    pub min: Rect,
    pub max: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrafficLightHit {
    Close,
    Minimize,
    Maximize,
}

/// Standard colours (close to Apple HIG values).
pub const COLOR_CLOSE: [f32; 4] = [0.99, 0.36, 0.31, 1.0];
pub const COLOR_MIN:   [f32; 4] = [0.99, 0.74, 0.18, 1.0];
pub const COLOR_MAX:   [f32; 4] = [0.21, 0.78, 0.35, 1.0];
const BORDER:          [f32; 4] = [0.0, 0.0, 0.0, 0.25];

impl TrafficLights {
    /// Lay out 3 dots inside `title_bar`, anchored to its left.
    /// `size` is the diameter in physical px; `gap` is the
    /// horizontal spacing between dots; `left_pad` is the inset
    /// from the title bar's left edge to the close dot.
    pub fn layout(title_bar: Rect, size: f64, gap: f64, left_pad: f64) -> Self {
        let y = title_bar.y_top + (title_bar.h - size) * 0.5;
        let x0 = title_bar.x + left_pad;
        let x1 = x0 + size + gap;
        let x2 = x1 + size + gap;
        Self {
            close: Rect { x: x0, y_top: y, w: size, h: size },
            min:   Rect { x: x1, y_top: y, w: size, h: size },
            max:   Rect { x: x2, y_top: y, w: size, h: size },
        }
    }

    /// Push 3 `UiRectInstance` dots into `ui_rects`.  Caller has
    /// already drawn the title-bar BG.
    pub fn paint(&self, ui_rects: &mut Vec<UiRectInstance>) {
        for (r, color) in [
            (self.close, COLOR_CLOSE),
            (self.min,   COLOR_MIN),
            (self.max,   COLOR_MAX),
        ] {
            ui_rects.push(UiRectInstance {
                origin: [r.x as f32, r.y_top as f32],
                size: [r.w as f32, r.h as f32],
                fill_color: color,
                border_color: BORDER,
                corner_radius: (r.w * 0.5) as f32,
                border_width: 1.0,
                shadow_blur: 0.0,
                shadow_alpha: 0.0,
                shadow_color: [0.0, 0.0, 0.0, 1.0],
            });
        }
    }

    pub fn hit_test(&self, x: f64, y: f64) -> Option<TrafficLightHit> {
        if self.close.contains(x, y) { return Some(TrafficLightHit::Close); }
        if self.min.contains(x, y)   { return Some(TrafficLightHit::Minimize); }
        if self.max.contains(x, y)   { return Some(TrafficLightHit::Maximize); }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y_top: y, w, h }
    }

    #[test]
    fn layout_anchors_close_at_left() {
        let bar = rect(100.0, 50.0, 800.0, 28.0);
        let t = TrafficLights::layout(bar, 12.0, 8.0, 12.0);
        assert_eq!(t.close.x, 112.0);
        // Vertically centered in the title bar.
        assert_eq!(t.close.y_top, 50.0 + (28.0 - 12.0) * 0.5);
        // Gaps: close → min → max stride = size + gap = 20.
        assert_eq!(t.min.x - t.close.x, 20.0);
        assert_eq!(t.max.x - t.min.x, 20.0);
    }

    #[test]
    fn hit_test_disjoint_regions() {
        let t = TrafficLights::layout(rect(0.0, 0.0, 800.0, 28.0), 12.0, 8.0, 12.0);
        let mid_close = (t.close.x + t.close.w * 0.5, t.close.y_top + t.close.h * 0.5);
        let mid_min   = (t.min.x   + t.min.w   * 0.5, t.min.y_top   + t.min.h   * 0.5);
        let mid_max   = (t.max.x   + t.max.w   * 0.5, t.max.y_top   + t.max.h   * 0.5);
        assert_eq!(t.hit_test(mid_close.0, mid_close.1), Some(TrafficLightHit::Close));
        assert_eq!(t.hit_test(mid_min.0,   mid_min.1),   Some(TrafficLightHit::Minimize));
        assert_eq!(t.hit_test(mid_max.0,   mid_max.1),   Some(TrafficLightHit::Maximize));
        assert_eq!(t.hit_test(-1.0, -1.0), None);
    }
}

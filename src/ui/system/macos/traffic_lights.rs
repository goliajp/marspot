//! macOS-style window-chrome traffic lights: red close, yellow
//! minimize, green maximize.  Drawn as small filled discs (SDF
//! corner_radius = size/2) anchored to the left edge of a title bar.

use marspot_term::layout::Rect;
use crate::render_metal::UiRectInstance;

/// Layout: 3 dots anchored left.  Rects use physical px.
#[derive(Debug, Clone)]
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

/// Traffic-light geometry, in logical points (callers multiply by the
/// backing scale).  These are the single source of truth for every
/// custom-drawn title bar; the main window uses the OS's own native
/// buttons and never touches these.
///
/// 15 rather than the nominal 12 of the macOS spec.  Measured against
/// the OS's own buttons rendered beside ours (pixel-counted from a
/// screenshot, not eyeballed): native shows **14 px** of saturated
/// colour on this display.  A 15 px disc loses about a pixel to SDF
/// antialiasing, landing on the same visible 14.  Gap stays 8.
pub const LIGHT_SIZE_LOGICAL: f64 = 15.0;
pub const LIGHT_GAP_LOGICAL: f64 = 8.0;
pub const LIGHT_LEFT_PAD_LOGICAL: f64 = 12.0;

/// Glyph colour for the hover-revealed ×/−/+ .
///
/// A dark wash of the dot's own hue rather than pure black — that is
/// how the system draws it, and pure black on the yellow dot reads far
/// heavier than on the red one.
pub const GLYPH_FG: [f32; 4] = [0.12, 0.10, 0.06, 0.80];

/// Standard colours (close to Apple HIG values).
pub const COLOR_CLOSE: [f32; 4] = [0.99, 0.36, 0.31, 1.0];
pub const COLOR_MIN:   [f32; 4] = [0.99, 0.74, 0.18, 1.0];
pub const COLOR_MAX:   [f32; 4] = [0.21, 0.78, 0.35, 1.0];
/// No rim.
///
/// There used to be a 25 %-black inside-stroke here, and for most of its
/// life it was invisible: the `ui_rect` pipeline was applying alpha
/// twice, so 0.25 rendered as 0.06.  Fixing that blend made the rim
/// appear for the first time — and because the shader draws borders
/// *inside* the shape (box-sizing: border-box), it ate ~1 px off every
/// edge.  Measured against the OS's own buttons in the same screenshot:
/// native showed 14 px of colour, ours showed 9.  The dots did not
/// shrink; the rim grew.  Dropping it restores what the design always
/// looked like.
const BORDER:          [f32; 4] = [0.0, 0.0, 0.0, 0.0];

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
                border_width: 0.0,
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
        let t = TrafficLights::layout(bar, LIGHT_SIZE_LOGICAL, LIGHT_GAP_LOGICAL, LIGHT_LEFT_PAD_LOGICAL);
        // Assertions derive from the constants rather than restating
        // their current values — the previous version hardcoded both the
        // inputs and the expected outputs, so bumping the diameter broke
        // a test that was only ever checking arithmetic.
        assert_eq!(t.close.x, 100.0 + LIGHT_LEFT_PAD_LOGICAL);
        // Vertically centered in the title bar.
        assert_eq!(t.close.y_top, 50.0 + (28.0 - LIGHT_SIZE_LOGICAL) * 0.5);
        // Stride between dots is diameter + gap.
        let stride = LIGHT_SIZE_LOGICAL + LIGHT_GAP_LOGICAL;
        assert_eq!(t.min.x - t.close.x, stride);
        assert_eq!(t.max.x - t.min.x, stride);
    }

    #[test]
    fn hit_test_disjoint_regions() {
        let t = TrafficLights::layout(
            rect(0.0, 0.0, 800.0, 28.0),
            LIGHT_SIZE_LOGICAL, LIGHT_GAP_LOGICAL, LIGHT_LEFT_PAD_LOGICAL,
        );
        let mid_close = (t.close.x + t.close.w * 0.5, t.close.y_top + t.close.h * 0.5);
        let mid_min   = (t.min.x   + t.min.w   * 0.5, t.min.y_top   + t.min.h   * 0.5);
        let mid_max   = (t.max.x   + t.max.w   * 0.5, t.max.y_top   + t.max.h   * 0.5);
        assert_eq!(t.hit_test(mid_close.0, mid_close.1), Some(TrafficLightHit::Close));
        assert_eq!(t.hit_test(mid_min.0,   mid_min.1),   Some(TrafficLightHit::Minimize));
        assert_eq!(t.hit_test(mid_max.0,   mid_max.1),   Some(TrafficLightHit::Maximize));
        assert_eq!(t.hit_test(-1.0, -1.0), None);
    }
}

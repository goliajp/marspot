//! macOS-style title bar: solid fill + optional traffic lights on
//! the left + centered title text.  Built on top of `TrafficLights`
//! and `ViewPainter`; produces hit rects so the caller can route
//! "close button clicked" / "minimize clicked" / etc.
//!
//! Typical use:
//! ```ignore
//! let bar = TitleBar::layout(title_bar_rect, scale, true /* with_lights */);
//! bar.paint(painter, "Process Monitor", FG_TITLE, BG_FILL, hovered);
//! match bar.hit_test(x, y) {
//!     Some(TitleBarHit::Light(TrafficLightHit::Close)) => self.close(),
//!     Some(TitleBarHit::Light(TrafficLightHit::Minimize)) => self.minimize(),
//!     Some(TitleBarHit::Light(TrafficLightHit::Maximize)) => self.maximize(),
//!     Some(TitleBarHit::Body) => self.start_drag(...),
//!     None => {}
//! }
//! ```

use marspot_term::layout::Rect;
use crate::ui::core::ViewPainter;
use super::traffic_lights::{
    TrafficLights, TrafficLightHit,
    LIGHT_SIZE_LOGICAL, LIGHT_GAP_LOGICAL, LIGHT_LEFT_PAD_LOGICAL,
};

#[derive(Debug, Clone)]
pub struct TitleBar {
    pub rect: Rect,
    /// `None` when this title bar opts out of traffic lights (e.g.
    /// a borderless inline panel) — caller wants just the band +
    /// centered text.
    pub lights: Option<TrafficLights>,
    pub scale: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleBarHit {
    Light(TrafficLightHit),
    /// Anywhere in the title bar that's not a light.  Caller decides
    /// the semantics (typically: start a drag).
    Body,
}

impl TitleBar {
    pub fn layout(rect: Rect, scale: f64, with_lights: bool) -> Self {
        let lights = if with_lights {
            Some(TrafficLights::layout(
                rect,
                LIGHT_SIZE_LOGICAL * scale,
                LIGHT_GAP_LOGICAL * scale,
                LIGHT_LEFT_PAD_LOGICAL * scale,
            ))
        } else {
            None
        };
        Self { rect, lights, scale }
    }

    /// Paint the title bar BG + traffic lights + centered title text.
    /// Caller owns the rect placement; this just fills it.
    pub fn paint(
        &self,
        p: &mut ViewPainter,
        title: &str,
        fg_color: [f32; 4],
        bg_color: [f32; 4],
        // Cursor anywhere in this bar — reveals the traffic-light
        // marks, the way the system does (all three at once, because
        // the affordance answers "are these clickable", not "which one
        // am I on").
        lights_hovered: bool,
    ) {
        // Title bar fill (flat — the parent View already drew the
        // outer rounded chrome, this just tints the band).
        p.fill_rect(self.rect, bg_color);
        // Traffic lights (if any).
        if let Some(l) = &self.lights {
            l.paint(p.ui_rects, lights_hovered);
        }
        // Centered title text.
        let title_w_chars = title.chars().count() as f32;
        let title_x = self.rect.x as f32
            + (self.rect.w as f32 - title_w_chars * p.cell_w) * 0.5;
        let baseline = self.rect.y_top as f32
            + (self.rect.h as f32 - p.cell_h) * 0.5
            + p.ascent;
        p.text(title_x, baseline, title, fg_color);
    }

    pub fn hit_test(&self, x: f64, y: f64) -> Option<TitleBarHit> {
        if let Some(l) = &self.lights {
            if let Some(h) = l.hit_test(x, y) {
                return Some(TitleBarHit::Light(h));
            }
        }
        if self.rect.contains(x, y) {
            return Some(TitleBarHit::Body);
        }
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
    fn lights_skipped_when_with_lights_false() {
        let bar = TitleBar::layout(rect(0.0, 0.0, 800.0, 28.0), 1.0, false);
        assert!(bar.lights.is_none());
    }

    #[test]
    fn hit_test_routes_light_clicks() {
        let bar = TitleBar::layout(rect(0.0, 0.0, 800.0, 28.0), 1.0, true);
        let l = bar.lights.as_ref().unwrap();
        let cx = l.close.x + l.close.w * 0.5;
        let cy = l.close.y_top + l.close.h * 0.5;
        assert_eq!(bar.hit_test(cx, cy), Some(TitleBarHit::Light(TrafficLightHit::Close)));
    }

    #[test]
    fn hit_test_body_returns_body_variant() {
        let bar = TitleBar::layout(rect(0.0, 0.0, 800.0, 28.0), 1.0, true);
        // Middle of bar, not a light.
        assert_eq!(bar.hit_test(400.0, 14.0), Some(TitleBarHit::Body));
    }

    #[test]
    fn hit_test_outside_returns_none() {
        let bar = TitleBar::layout(rect(0.0, 0.0, 800.0, 28.0), 1.0, true);
        assert_eq!(bar.hit_test(-5.0, -5.0), None);
        assert_eq!(bar.hit_test(900.0, 0.0), None);
    }
}

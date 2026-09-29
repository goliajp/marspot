//! `Panel` — opinionated `View` + structured padding.  Use when a
//! feature wants a uniform "card" surface: rounded BG + inner content
//! inset by the same padding all the way around.  Wraps `core::View`
//! so the always-on-top + opaque-by-default invariants apply for free.
//!
//! Typical use:
//! ```ignore
//! let panel = Panel::new(rect, ViewStyle { ..Default::default() }, 12.0);
//! panel.paint(p, |p| {
//!     // content_rect already inset by padding
//!     let cr = panel.content_rect();
//!     p.text(cr.x as f32, cr.y_top as f32 + p.ascent, "Hello", FG);
//! });
//! ```

use marspot_term::layout::Rect;
use crate::ui::core::{View, ViewPainter, ViewStyle};

#[derive(Debug, Clone, Copy)]
pub struct Panel {
    pub view: View,
    pub padding: f64,
}

impl Panel {
    pub fn new(rect: Rect, style: ViewStyle, padding: f64) -> Self {
        Self {
            view: View { rect, style },
            padding,
        }
    }

    /// Inner rect available to the panel's content.  Always inset by
    /// `padding` on all four sides.  Callers position their widgets
    /// relative to this.
    pub fn content_rect(&self) -> Rect {
        let pad = self.padding.max(0.0);
        let inner_w = (self.view.rect.w - 2.0 * pad).max(0.0);
        let inner_h = (self.view.rect.h - 2.0 * pad).max(0.0);
        Rect {
            x: self.view.rect.x + pad,
            y_top: self.view.rect.y_top + pad,
            w: inner_w,
            h: inner_h,
        }
    }

    /// Paint the panel's chrome (View frame + backdrop) then hand the
    /// painter to `body` so the caller fills the content area.
    pub fn paint<F>(&self, painter: &mut ViewPainter, body: F)
    where
        F: FnOnce(&mut ViewPainter),
    {
        self.view.paint(painter, body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y_top: y, w, h }
    }

    #[test]
    fn content_rect_inset_by_padding() {
        let p = Panel::new(rect(0.0, 0.0, 200.0, 100.0), ViewStyle::default(), 10.0);
        let cr = p.content_rect();
        assert_eq!(cr.x, 10.0);
        assert_eq!(cr.y_top, 10.0);
        assert_eq!(cr.w, 180.0);
        assert_eq!(cr.h, 80.0);
    }

    #[test]
    fn content_rect_clamps_when_padding_exceeds_size() {
        let p = Panel::new(rect(0.0, 0.0, 10.0, 10.0), ViewStyle::default(), 20.0);
        let cr = p.content_rect();
        assert_eq!(cr.w, 0.0);
        assert_eq!(cr.h, 0.0);
    }

    #[test]
    fn negative_padding_is_clamped_to_zero() {
        let p = Panel::new(rect(0.0, 0.0, 200.0, 100.0), ViewStyle::default(), -5.0);
        let cr = p.content_rect();
        assert_eq!(cr.w, 200.0);
        assert_eq!(cr.h, 100.0);
    }
}

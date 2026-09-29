//! Vertical scroll view: a viewport rect + scroll offset + content
//! height, with helpers for wheel events and visible-row math.
//!
//! Component is stateful (`scroll_y` lives here) — caller stores a
//! `ScrollView` per scrollable surface and asks it which rows to paint.

use marspot_term::layout::Rect;

#[derive(Debug, Clone)]
pub struct ScrollView {
    pub viewport: Rect,
    pub scroll_y: f64,
    pub content_h: f64,
}

impl ScrollView {
    pub fn new(viewport: Rect) -> Self {
        Self { viewport, scroll_y: 0.0, content_h: 0.0 }
    }

    /// Max allowed scroll = content - viewport (clamped to ≥ 0).
    pub fn max_scroll(&self) -> f64 {
        (self.content_h - self.viewport.h).max(0.0)
    }

    pub fn clamp(&mut self) {
        let max = self.max_scroll();
        if self.scroll_y < 0.0 { self.scroll_y = 0.0; }
        if self.scroll_y > max { self.scroll_y = max; }
    }

    /// Apply a wheel delta in physical pixels (positive = scroll down).
    pub fn wheel(&mut self, dy_phys: f64) {
        self.scroll_y += dy_phys;
        self.clamp();
    }

    /// Compute the inclusive row range visible given a fixed row
    /// height.  Returns `(first, last_exclusive)`.
    pub fn visible_row_range(&self, row_h: f64) -> (usize, usize) {
        if row_h <= 0.0 {
            return (0, 0);
        }
        let first = (self.scroll_y / row_h).floor().max(0.0) as usize;
        let last  = ((self.scroll_y + self.viewport.h) / row_h).ceil() as usize;
        (first, last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y_top: y, w, h }
    }

    #[test]
    fn clamp_does_not_exceed_max() {
        let mut sv = ScrollView::new(rect(0.0, 0.0, 100.0, 200.0));
        sv.content_h = 500.0;
        sv.scroll_y = 1000.0;
        sv.clamp();
        assert_eq!(sv.scroll_y, 300.0);
    }

    #[test]
    fn clamp_does_not_go_negative() {
        let mut sv = ScrollView::new(rect(0.0, 0.0, 100.0, 200.0));
        sv.content_h = 500.0;
        sv.scroll_y = -50.0;
        sv.clamp();
        assert_eq!(sv.scroll_y, 0.0);
    }

    #[test]
    fn wheel_updates_and_clamps() {
        let mut sv = ScrollView::new(rect(0.0, 0.0, 100.0, 200.0));
        sv.content_h = 500.0;
        sv.wheel(50.0);
        assert_eq!(sv.scroll_y, 50.0);
        sv.wheel(1000.0);
        assert_eq!(sv.scroll_y, 300.0);
    }

    #[test]
    fn visible_row_range_at_top() {
        let mut sv = ScrollView::new(rect(0.0, 0.0, 100.0, 200.0));
        sv.content_h = 1000.0;
        let (first, last) = sv.visible_row_range(20.0);
        assert_eq!(first, 0);
        assert_eq!(last, 10);
    }

    #[test]
    fn visible_row_range_mid_scroll() {
        let mut sv = ScrollView::new(rect(0.0, 0.0, 100.0, 200.0));
        sv.content_h = 1000.0;
        sv.scroll_y = 100.0;
        let (first, last) = sv.visible_row_range(20.0);
        assert_eq!(first, 5);
        assert_eq!(last, 15);
    }

    #[test]
    fn max_scroll_zero_when_content_fits() {
        let mut sv = ScrollView::new(rect(0.0, 0.0, 100.0, 200.0));
        sv.content_h = 100.0;
        assert_eq!(sv.max_scroll(), 0.0);
    }
}

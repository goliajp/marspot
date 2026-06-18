//! Modal frame: a centered floating window with chrome
//! (title bar with traffic lights + optional tab strip + body).
//! Handles geometry (default vs. maximized vs. minimized + drag offset)
//! and produces the canonical sub-rects (title bar, tab strip area, body).

use marspot_term::layout::Rect;

#[derive(Debug, Clone, Copy)]
pub struct ModalFrame {
    pub frame: Rect,
    pub title_bar: Rect,
    /// Empty rect (h = 0) when no tab strip needed.
    pub tab_strip: Rect,
    /// Empty rect (h = 0) when `minimized`.
    pub body: Rect,
}

#[derive(Debug, Clone, Copy)]
pub struct ModalLayoutSpec {
    pub default_w: f64,
    pub default_h: f64,
    pub title_bar_h: f64,
    pub tab_strip_h: f64,
    /// True = expand to `(window_w * max_w_ratio, window_h * max_h_ratio)`.
    pub maximized: bool,
    pub max_w_ratio: f64,
    pub max_h_ratio: f64,
    /// True = body collapsed; frame.h becomes title_bar_h only.
    pub minimized: bool,
    /// Whether to render the tab strip at all.
    pub with_tab_strip: bool,
    /// Offset from centered default position (pixels).  Drag-controlled.
    pub pos_offset: (f64, f64),
}

impl ModalFrame {
    pub fn layout(window_w: f64, window_h: f64, spec: ModalLayoutSpec) -> Self {
        let (w_unclamped, h_unclamped) = if spec.maximized {
            (window_w * spec.max_w_ratio, window_h * spec.max_h_ratio)
        } else {
            (spec.default_w, spec.default_h)
        };
        // Clamp to fit inside the window with a small visible margin.
        let w = w_unclamped.min(window_w * 0.98);
        let h_full = h_unclamped.min(window_h * 0.95);
        // Center the default frame, then offset by drag.
        let cx = (window_w - w) * 0.5 + spec.pos_offset.0;
        let cy = (window_h - h_full) * 0.5 + spec.pos_offset.1;
        // Clamp the offset position so the modal stays at least
        // partially on-screen (title bar must remain accessible).
        let cx = cx.clamp(-w * 0.5, window_w - w * 0.5);
        let cy = cy.clamp(0.0, window_h - spec.title_bar_h);
        let frame_h = if spec.minimized { spec.title_bar_h } else { h_full };
        let frame = Rect { x: cx, y_top: cy, w, h: frame_h };
        let title_bar = Rect {
            x: cx, y_top: cy, w, h: spec.title_bar_h,
        };
        let tab_strip = if spec.with_tab_strip && !spec.minimized {
            Rect { x: cx, y_top: cy + spec.title_bar_h, w, h: spec.tab_strip_h }
        } else {
            Rect { x: cx, y_top: cy + spec.title_bar_h, w, h: 0.0 }
        };
        let body = if spec.minimized {
            Rect { x: cx, y_top: cy + spec.title_bar_h, w, h: 0.0 }
        } else {
            let body_top = cy + spec.title_bar_h + tab_strip.h;
            Rect { x: cx, y_top: body_top, w, h: (frame_h - (body_top - cy)).max(0.0) }
        };
        Self { frame, title_bar, tab_strip, body }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(maximized: bool, minimized: bool) -> ModalLayoutSpec {
        ModalLayoutSpec {
            default_w: 800.0,
            default_h: 600.0,
            title_bar_h: 28.0,
            tab_strip_h: 30.0,
            maximized,
            max_w_ratio: 0.95,
            max_h_ratio: 0.90,
            minimized,
            with_tab_strip: true,
            pos_offset: (0.0, 0.0),
        }
    }

    #[test]
    fn default_layout_is_centered() {
        let m = ModalFrame::layout(1920.0, 1080.0, spec(false, false));
        assert_eq!(m.frame.w, 800.0);
        assert_eq!(m.frame.h, 600.0);
        assert_eq!(m.frame.x, (1920.0 - 800.0) * 0.5);
        assert_eq!(m.frame.y_top, (1080.0 - 600.0) * 0.5);
        assert_eq!(m.title_bar.h, 28.0);
        assert_eq!(m.tab_strip.h, 30.0);
        // body fills the remainder.
        assert!((m.body.h - (600.0 - 28.0 - 30.0)).abs() < 0.001);
    }

    #[test]
    fn maximized_expands_to_ratio() {
        let m = ModalFrame::layout(1920.0, 1080.0, spec(true, false));
        assert!((m.frame.w - 1920.0 * 0.95).abs() < 0.001);
        assert!((m.frame.h - 1080.0 * 0.90).abs() < 0.001);
    }

    #[test]
    fn minimized_collapses_to_title_bar_only() {
        let m = ModalFrame::layout(1920.0, 1080.0, spec(false, true));
        assert_eq!(m.frame.h, 28.0);
        assert_eq!(m.tab_strip.h, 0.0);
        assert_eq!(m.body.h, 0.0);
    }

    #[test]
    fn no_tab_strip_when_disabled() {
        let mut s = spec(false, false);
        s.with_tab_strip = false;
        let m = ModalFrame::layout(1920.0, 1080.0, s);
        assert_eq!(m.tab_strip.h, 0.0);
        // body absorbs the strip height.
        assert!((m.body.h - (600.0 - 28.0)).abs() < 0.001);
    }

    #[test]
    fn pos_offset_translates_the_frame() {
        let mut s = spec(false, false);
        s.pos_offset = (50.0, 30.0);
        let m = ModalFrame::layout(1920.0, 1080.0, s);
        assert_eq!(m.frame.x, (1920.0 - 800.0) * 0.5 + 50.0);
        assert_eq!(m.frame.y_top, (1080.0 - 600.0) * 0.5 + 30.0);
    }

    #[test]
    fn pos_offset_clamped_to_keep_title_bar_accessible() {
        let mut s = spec(false, false);
        // Drag way off-screen down.
        s.pos_offset = (0.0, 100_000.0);
        let m = ModalFrame::layout(1920.0, 1080.0, s);
        assert!(m.frame.y_top <= 1080.0 - 28.0);
    }
}

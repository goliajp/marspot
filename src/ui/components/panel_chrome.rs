//! The frame, header and content area every panel shares.
//!
//! A panel is, top to bottom: a frame, a header, a content area.  This
//! module is the only place that knows where those are, so the title
//! of one panel sits where the title of the next one does, and the
//! close button is under the pointer in the same place whichever panel
//! is open.
//!
//! [`PanelChrome`] is geometry first — built from the frame rect with
//! no painter, so a click handler asks it the same questions the
//! painter does — and [`PanelChrome::paint`] draws what the geometry
//! describes.  Below it are the pieces content is made of: a group
//! heading, a card, the line between two rows of a card.
//!
//! All rects are physical pixels.

use marspot_term::layout::Rect;

use crate::ui::components::controls;
use crate::ui::components::panel_palette as palette;
use crate::ui::core::view::{Backdrop, View, ViewStyle};
use crate::ui::core::ViewPainter;
use crate::ui::theme::spec::{self, card as card_spec, content, header, panel, space};

/// Where a panel's shared parts are.
#[derive(Debug, Clone, Copy)]
pub struct PanelChrome {
    /// The whole panel.
    pub frame: Rect,
    /// The band across the top: title, meta line, close button.  Also
    /// the handle a draggable panel is dragged by.
    pub header: Rect,
    /// The close button's hit box, inside `header`.
    pub close: Rect,
    /// Where the panel's own content goes: under the header, inset
    /// from the frame's left, right and bottom edges.
    pub content: Rect,
    /// The window is dimmed from this y down; the strip above it (the
    /// window's own toolbar) stays as it is.
    pub backdrop_top: f64,
}

impl PanelChrome {
    /// The chrome of the panel at `frame`.  `top_inset` is the height
    /// of the window's own chrome above the panes.
    pub fn new(frame: Rect, top_inset: f64) -> Self {
        let header_h = spec::px(header::H).min(frame.h);
        let close_d = spec::px(header::CLOSE_SIZE);
        let pad_x = spec::px(content::PAD_X);
        let header = Rect { h: header_h, ..frame };
        Self {
            frame,
            header,
            close: Rect {
                x: frame.x + frame.w - spec::px(header::CLOSE_PAD_RIGHT) - close_d,
                y_top: frame.y_top + (header_h - close_d) * 0.5,
                w: close_d,
                h: close_d,
            },
            content: Rect {
                x: frame.x + pad_x,
                y_top: frame.y_top + header_h,
                w: (frame.w - 2.0 * pad_x).max(0.0),
                h: (frame.h - header_h - spec::px(content::PAD_BOTTOM)).max(0.0),
            },
            backdrop_top: top_inset,
        }
    }

    /// Left edge of the title — the content area's left edge.
    pub fn title_x(&self) -> f64 {
        self.content.x
    }

    /// Baseline that centres the title's capitals in the header.
    pub fn title_baseline(&self) -> f64 {
        self.header.y_top + (self.header.h + spec::px(header::TITLE.cap())) * 0.5
    }

    /// Right edge of the optional meta line, left of the close button.
    pub fn meta_right(&self) -> f64 {
        self.close.x - spec::px(header::META_GAP)
    }

    /// Is `(x, y)` on the close button?
    pub fn hit_close(&self, x: f64, y: f64) -> bool {
        self.close.contains(x, y)
    }

    /// Is `(x, y)` on the header where it is a drag handle — anywhere
    /// in it but the close button?
    pub fn hit_header(&self, x: f64, y: f64) -> bool {
        self.header.contains(x, y) && !self.hit_close(x, y)
    }

    /// Dims the window, draws the frame and the header, then hands
    /// `body` the painter and the content rect.
    ///
    /// Nothing is clipped: content that does not fit the content rect
    /// has to be left out by the panel itself.
    pub fn paint(
        &self,
        p: &mut ViewPainter,
        title: &str,
        meta: Option<&str>,
        close_hovered: bool,
        body: impl FnOnce(&mut ViewPainter, Rect),
    ) {
        let view = View {
            rect: self.frame,
            style: ViewStyle {
                bg: palette::frame_bg(),
                border_color: palette::frame_border(),
                border_width: panel::BORDER_PX,
                corner_radius: spec::px(panel::RADIUS) as f32,
                shadow_blur: spec::px(panel::SHADOW_BLUR) as f32,
                shadow_alpha: panel::SHADOW_ALPHA,
                padding: 0.0,
                backdrop: Backdrop::Dim {
                    color: palette::backdrop(),
                    exclude_above_y: self.backdrop_top,
                },
            },
        };
        view.paint(p, |p| {
            p.panel_text(
                header::TITLE,
                self.title_x() as f32,
                self.title_baseline() as f32,
                title,
                palette::fg(),
            );
            if let Some(meta) = meta {
                let w = p.panel_text_width(header::META, meta) as f64;
                let baseline = p.ui_baseline_centred(
                    self.header.y_top as f32,
                    self.header.h as f32,
                    header::META.pt(),
                );
                p.panel_text(
                    header::META,
                    (self.meta_right() - w) as f32,
                    baseline,
                    meta,
                    palette::fg_muted(),
                );
            }
            let mark = if close_hovered {
                p.fill_rounded_rect(
                    self.close,
                    palette::close_hover_bg(),
                    spec::px(header::CLOSE_RADIUS) as f32,
                    (palette::none(), 0.0),
                );
                palette::fg()
            } else {
                palette::fg_muted()
            };
            controls::paint_cross(p, self.close, header::CLOSE_MARK, mark);
            body(p, self.content);
        });
    }
}

/// The frame for a panel that wants to be `w × h`: centred in the
/// window, no larger than the window allows, clear of the window's own
/// chrome at the top.
pub fn frame_rect(window_w: f64, window_h: f64, w: f64, h: f64, top_inset: f64) -> Rect {
    let w = w.min(window_w * panel::MAX_W_FRAC);
    let h = h.min(window_h * panel::MAX_H_FRAC);
    Rect {
        x: (window_w - w) * 0.5,
        y_top: ((window_h - h) * 0.5).max(top_inset + spec::px(panel::TOP_CLEARANCE)),
        w,
        h,
    }
}

/// Height of the frame whose content is `content_h` tall: the header
/// above it and the bottom padding below.
pub fn frame_height(content_h: f64) -> f64 {
    spec::px(header::H) + content_h + spec::px(content::PAD_BOTTOM)
}

/// From the top of a group heading's line to the top of the card
/// under it.
pub fn heading_height() -> f64 {
    spec::px(content::HEADING.cap() + space::HEADING_TO_CARD)
}

/// A group heading whose line starts at `top`: `title` at the left
/// edge `x`, `trailing` — a caption, if any — ending at `x + w`.
/// Returns the y where the group's card starts.
pub fn section_heading(
    p: &mut ViewPainter,
    x: f64,
    top: f64,
    w: f64,
    title: &str,
    trailing: Option<&str>,
) -> f64 {
    let baseline = (top + spec::px(content::HEADING.cap())) as f32;
    p.panel_text(content::HEADING, x as f32, baseline, title, palette::fg());
    if let Some(trailing) = trailing {
        let tw = p.panel_text_width(content::HEADING_TRAILING, trailing) as f64;
        p.panel_text(
            content::HEADING_TRAILING,
            (x + w - tw) as f32,
            baseline,
            trailing,
            palette::fg_muted(),
        );
    }
    top + heading_height()
}

/// A card: the surface a group's rows or table sit on.
pub fn card(p: &mut ViewPainter, rect: Rect) {
    p.fill_rounded_rect(
        rect,
        palette::card_bg(),
        spec::px(card_spec::RADIUS) as f32,
        (palette::card_border(), card_spec::BORDER_PX),
    );
}

/// The line between two rows of `card`, its top edge at `y`.
pub fn row_separator_rect(card: Rect, y: f64) -> Rect {
    let inset = spec::px(card_spec::SEPARATOR_INSET);
    Rect {
        x: card.x + inset,
        y_top: y,
        w: (card.w - 2.0 * inset).max(0.0),
        h: card_spec::SEPARATOR_PX,
    }
}

/// Draws the line [`row_separator_rect`] describes.
pub fn row_separator(p: &mut ViewPainter, card: Rect, y: f64) {
    // In the rounded-rect pass, like the card: the plain-rect pass is
    // drawn before it, so a line submitted there ends up under the
    // card it is meant to divide.
    p.fill_rounded_rect(row_separator_rect(card, y), palette::separator(), 0.0, (palette::none(), 0.0));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y_top: y, w, h }
    }

    #[test]
    fn panel_chrome_header_and_content_tile_the_frame() {
        let f = frame(100.0, 60.0, 880.0, 700.0);
        let c = PanelChrome::new(f, 0.0);
        assert_eq!((c.header.x, c.header.y_top, c.header.w), (f.x, f.y_top, f.w));
        assert_eq!(c.header.h, spec::px(header::H));
        assert_eq!(c.content.y_top, c.header.y_top + c.header.h, "content starts where the header ends");
        let pad = spec::px(content::PAD_X);
        assert_eq!(c.content.x - f.x, pad);
        assert_eq!(f.x + f.w - (c.content.x + c.content.w), pad, "same inset left and right");
        assert_eq!(
            f.y_top + f.h - (c.content.y_top + c.content.h),
            spec::px(content::PAD_BOTTOM),
        );
        assert_eq!(c.title_x(), c.content.x, "the title starts on the content's left edge");
    }

    #[test]
    fn panel_chrome_close_button_sits_in_the_header() {
        let f = frame(100.0, 60.0, 880.0, 700.0);
        let c = PanelChrome::new(f, 0.0);
        let d = spec::px(header::CLOSE_SIZE);
        assert_eq!((c.close.w, c.close.h), (d, d));
        assert_eq!(f.x + f.w - (c.close.x + c.close.w), spec::px(header::CLOSE_PAD_RIGHT));
        let above = c.close.y_top - c.header.y_top;
        let below = c.header.y_top + c.header.h - (c.close.y_top + c.close.h);
        assert_eq!(above, below, "centred in the header");
        assert!(above > 0.0);
        assert_eq!(c.close.x - c.meta_right(), spec::px(header::META_GAP));
        // The title's capitals are centred on the same line.
        let cap = spec::px(header::TITLE.cap());
        let title_mid = c.title_baseline() - cap * 0.5;
        assert!((title_mid - (c.header.y_top + c.header.h * 0.5)).abs() < 1e-9);
    }

    #[test]
    fn panel_chrome_hits_close_and_header_apart() {
        let c = PanelChrome::new(frame(100.0, 60.0, 880.0, 700.0), 0.0);
        let (cx, cy) = (c.close.x + c.close.w * 0.5, c.close.y_top + c.close.h * 0.5);
        assert!(c.hit_close(cx, cy));
        assert!(!c.hit_header(cx, cy), "the close button is not a drag handle");
        assert!(c.hit_header(c.title_x(), cy));
        assert!(!c.hit_close(c.title_x(), cy));
        let in_content = c.content.y_top + 10.0;
        assert!(!c.hit_header(c.title_x(), in_content));
        assert!(!c.hit_close(cx, in_content));
        assert!(!c.hit_header(c.frame.x - 1.0, cy), "outside the frame is outside the header");
    }

    /// The point of sharing the chrome: whatever a panel's size, its
    /// header is laid out the same from the frame's corners.
    #[test]
    fn panel_chrome_header_is_the_same_in_every_frame() {
        let a = PanelChrome::new(frame(0.0, 0.0, 600.0, 400.0), 0.0);
        let b = PanelChrome::new(frame(321.0, 77.0, 1900.0, 1000.0), 40.0);
        assert_eq!(a.header.h, b.header.h);
        assert_eq!(a.title_x() - a.frame.x, b.title_x() - b.frame.x);
        assert_eq!(a.title_baseline() - a.frame.y_top, b.title_baseline() - b.frame.y_top);
        let right = |c: &PanelChrome| c.frame.x + c.frame.w - c.close.x;
        assert_eq!(right(&a), right(&b));
        assert_eq!(a.close.y_top - a.frame.y_top, b.close.y_top - b.frame.y_top);
        assert_eq!(b.backdrop_top, 40.0);
    }

    #[test]
    fn panel_chrome_frame_is_centred_and_fits_the_window() {
        let r = frame_rect(2000.0, 1200.0, 880.0, 600.0, 0.0);
        assert_eq!((r.w, r.h), (880.0, 600.0));
        assert_eq!((r.x, r.y_top), (560.0, 300.0));
        // Wanting more than the window has is cut to its share of it.
        let r = frame_rect(1000.0, 800.0, 5000.0, 5000.0, 0.0);
        assert_eq!(r.w, 1000.0 * panel::MAX_W_FRAC);
        assert_eq!(r.h, 800.0 * panel::MAX_H_FRAC);
        assert!(r.x >= 0.0 && r.x + r.w <= 1000.0);
        assert!(r.y_top >= 0.0 && r.y_top + r.h <= 800.0);
        // The window's own chrome is never covered.
        let r = frame_rect(1000.0, 800.0, 600.0, 700.0, 120.0);
        assert_eq!(r.y_top, 120.0 + spec::px(panel::TOP_CLEARANCE));
    }

    #[test]
    fn panel_chrome_frame_height_wraps_its_content() {
        let content_h = 333.0;
        let f = frame(0.0, 0.0, 800.0, frame_height(content_h));
        assert!((PanelChrome::new(f, 0.0).content.h - content_h).abs() < 1e-9);
    }

    #[test]
    fn panel_chrome_separator_is_inset_from_both_card_edges() {
        let card = frame(40.0, 100.0, 600.0, 200.0);
        let s = row_separator_rect(card, 150.0);
        let inset = spec::px(card_spec::SEPARATOR_INSET);
        assert_eq!(s.x - card.x, inset);
        assert_eq!(card.x + card.w - (s.x + s.w), inset);
        assert_eq!((s.y_top, s.h), (150.0, card_spec::SEPARATOR_PX));
    }

    /// An opening bracket straight into `0.` or `1.` is the start of
    /// an RGBA array written out by hand.
    fn colour_literals(src: &str) -> usize {
        ['0', '1'].iter().map(|d| src.matches(&format!("[{d}.")).count()).sum()
    }

    #[test]
    fn panel_chrome_source_has_no_colour_literals() {
        // Assembled, so that this file does not contain one itself.
        let literal = format!("bg: [{}.13, 0.14, 0.17, 1.0],", 0);
        assert_eq!(colour_literals(&literal), 1, "the search finds a literal when there is one");
        assert_eq!(colour_literals(include_str!("panel_chrome.rs")), 0);
        assert_eq!(colour_literals(include_str!("process_panel.rs")), 0);
        assert_eq!(colour_literals(include_str!("process_panel_paint.rs")), 0);
        assert_eq!(colour_literals(include_str!("table.rs")), 0);
    }
}

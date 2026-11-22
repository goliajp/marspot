//! A pane's scrollbar: a thumb at the pane's right edge that says how
//! far back into its history the pane is showing.
//!
//! It is there only while the pane is scrolled back.  At the live tail
//! nothing is drawn, so a pane at rest costs nothing and needs no timer
//! to fade a bar out: the bar goes when the view returns to live, which
//! is a change that already redraws the pane.
//!
//! Track and thumb are worked out in physical pixels from the pane's
//! rect, every length from the spec in points, and each edge is put on
//! a whole window unit so the bar is the same shape at every display
//! scale.

use crate::layout::CellRect;
use crate::render::HistoryView;
use crate::ui::theme::spec::{chrome as chrome_spec, snap};
use golia_ui_core::scene::UiRectInstance;
use marspot_term::layout::Rect;

/// Where a pane's scrollbar is, for one frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scrollbar {
    /// The pane's rect, which a press is tested against.
    pane: Rect,
    /// The column the thumb runs in, end to end.
    pub track: Rect,
    pub thumb: Rect,
    history: HistoryView,
}

/// A press held on a scrollbar: where on the thumb the pointer took
/// hold, as a distance from the thumb's top.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollbarGrab {
    pub at: f64,
}

impl Scrollbar {
    /// The scrollbar of a pane laid out in `cell`, its grid starting
    /// `title_h` below the cell's top and `rows` rows tall.  `None`
    /// while the pane shows its live tail or has no history.
    pub fn of(cell: &CellRect, title_h: f64, rows: u16, history: HistoryView) -> Option<Self> {
        if history.offset == 0 {
            return None;
        }
        Self::held(cell, title_h, rows, history)
    }

    /// [`Self::of`] for a press already holding the thumb, which keeps
    /// the bar at the live tail too: a drag down to live and back up
    /// is one gesture.  `None` only without history.
    pub fn held(cell: &CellRect, title_h: f64, rows: u16, history: HistoryView) -> Option<Self> {
        if history.len == 0 {
            return None;
        }
        let px = chrome_spec::px;
        let inset = px(chrome_spec::SCROLLBAR_INSET);
        let right = snap(cell.x + cell.w - inset);
        let left = right - snap(px(chrome_spec::SCROLLBAR_W));
        let top = snap(cell.y_top + title_h + inset);
        let bottom = snap(cell.y_top + cell.h - inset);
        if bottom <= top {
            return None;
        }
        let track = Rect { x: left, y_top: top, w: right - left, h: bottom - top };
        let seen = rows.max(1) as f64 / (rows.max(1) as f64 + history.len as f64);
        let thumb_h = snap((track.h * seen).max(px(chrome_spec::SCROLLBAR_MIN_THUMB)).min(track.h));
        let travel = track.h - thumb_h;
        let back = history.offset.min(history.len) as f64 / history.len as f64;
        let thumb_top = snap(top + travel * (1.0 - back));
        let thumb = Rect { x: left, y_top: thumb_top, w: track.w, h: thumb_h };
        let pane = Rect { x: cell.x, y_top: cell.y_top, w: cell.w, h: cell.h };
        Some(Self { pane, track, thumb, history })
    }

    /// The hold a press at `(x, y)` takes, or `None` when it is not on
    /// the scrollbar.  A press on the thumb holds it where it landed; a
    /// press beside it moves the thumb's middle under the pointer.
    pub fn press(&self, x: f64, y: f64) -> Option<ScrollbarGrab> {
        let hit_left = self.pane.x + self.pane.w - chrome_spec::px(chrome_spec::SCROLLBAR_HIT_W);
        let on = x >= hit_left
            && x < self.pane.x + self.pane.w
            && y >= self.track.y_top
            && y < self.track.y_top + self.track.h;
        if !on {
            return None;
        }
        let into = y - self.thumb.y_top;
        let at = if (0.0..self.thumb.h).contains(&into) { into } else { self.thumb.h * 0.5 };
        Some(ScrollbarGrab { at })
    }

    /// The offset into history that puts the thumb, held by `grab`,
    /// under a pointer at height `y`.
    pub fn offset_at(&self, grab: ScrollbarGrab, y: f64) -> u16 {
        let travel = self.track.h - self.thumb.h;
        if travel <= 0.0 {
            return self.history.offset;
        }
        let down = ((y - grab.at - self.track.y_top) / travel).clamp(0.0, 1.0);
        ((1.0 - down) * self.history.len as f64).round() as u16
    }

    /// The thumb, as the rounded rectangle it is drawn as.
    pub fn instance(&self) -> UiRectInstance {
        let fill = crate::frame_build::color::rgba8_of_f32(super::panel_palette::scroll_thumb());
        UiRectInstance {
            origin: [self.thumb.x as f32, self.thumb.y_top as f32],
            size: [self.thumb.w as f32, self.thumb.h as f32],
            fill,
            border: golia_ui_core::Rgba8::TRANSPARENT,
            radius: (self.thumb.w * 0.5) as f32,
            border_width: 0.0,
            shadow_offset: [0.0, 0.0],
            shadow_color: golia_ui_core::Rgba8::TRANSPARENT,
            shadow_blur: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::components::density_kit::at_scale;

    fn cell() -> CellRect {
        CellRect { x: 100.0, y_top: 50.0, w: 800.0, h: 600.0, cols: 80, rows: 30 }
    }

    fn back(offset: u16, len: u16) -> HistoryView {
        HistoryView { offset, len }
    }

    #[test]
    fn nothing_is_drawn_at_the_live_tail_or_without_history() {
        at_scale(1.0, || {
            assert_eq!(Scrollbar::of(&cell(), 44.0, 30, back(0, 500)), None);
            assert_eq!(Scrollbar::of(&cell(), 44.0, 30, back(3, 0)), None);
            let held = Scrollbar::held(&cell(), 44.0, 30, back(0, 500)).unwrap();
            assert_eq!(held.thumb.y_top + held.thumb.h, held.track.y_top + held.track.h);
        });
    }

    #[test]
    fn the_thumb_runs_from_the_bottom_at_live_to_the_top_at_the_oldest_row() {
        at_scale(1.0, || {
            let oldest = Scrollbar::of(&cell(), 44.0, 30, back(500, 500)).unwrap();
            assert_eq!(oldest.thumb.y_top, oldest.track.y_top);
            let near_live = Scrollbar::of(&cell(), 44.0, 30, back(1, 500)).unwrap();
            let bottom = near_live.track.y_top + near_live.track.h;
            assert!(bottom - (near_live.thumb.y_top + near_live.thumb.h) <= 1.0);
            // an offset past the history it was taken against stays at the top
            let past = Scrollbar::of(&cell(), 44.0, 30, back(900, 500)).unwrap();
            assert_eq!(past.thumb, oldest.thumb);
        });
    }

    #[test]
    fn the_thumb_is_the_share_of_history_on_screen_but_never_too_short_to_see() {
        at_scale(1.0, || {
            let shallow = Scrollbar::of(&cell(), 44.0, 30, back(10, 30)).unwrap();
            assert_eq!(shallow.thumb.h, snap(shallow.track.h * 0.5));
            let deep = Scrollbar::of(&cell(), 44.0, 30, back(10, 60000)).unwrap();
            assert_eq!(deep.thumb.h, chrome_spec::px(chrome_spec::SCROLLBAR_MIN_THUMB));
        });
    }

    #[test]
    fn the_bar_is_twice_the_size_at_twice_the_density() {
        let at = |k: f64| {
            at_scale(k, || {
                let c = CellRect { x: 100.0 * k, y_top: 50.0 * k, w: 800.0 * k, h: 600.0 * k, cols: 80, rows: 30 };
                Scrollbar::of(&c, 44.0 * k, 30, back(250, 60000)).unwrap()
            })
        };
        let (one, two) = (at(1.0), at(2.0));
        for (a, b) in [(one.track, two.track), (one.thumb, two.thumb)] {
            assert_eq!([b.x, b.y_top, b.w, b.h], [a.x * 2.0, a.y_top * 2.0, a.w * 2.0, a.h * 2.0]);
        }
    }

    #[test]
    fn dragging_the_thumb_maps_back_to_the_offset_it_shows() {
        at_scale(1.0, || {
            let bar = Scrollbar::of(&cell(), 44.0, 30, back(200, 1000)).unwrap();
            let grab = bar.press(bar.thumb.x + 1.0, bar.thumb.y_top + 3.0).unwrap();
            assert_eq!(grab.at, 3.0);
            // holding still gives back the offset it was drawn at, to the pixel
            let still = bar.offset_at(grab, bar.thumb.y_top + 3.0) as i32;
            let per_px = 1000.0 / (bar.track.h - bar.thumb.h);
            assert!((still - 200).abs() as f64 <= per_px, "{still}");
            // to the ends of the track and past them
            assert_eq!(bar.offset_at(grab, -1e6), 1000);
            assert_eq!(bar.offset_at(grab, 1e6), 0);
        });
    }

    #[test]
    fn a_press_beside_the_thumb_centres_it_and_one_off_the_bar_is_not_taken() {
        at_scale(1.0, || {
            let bar = Scrollbar::of(&cell(), 44.0, 30, back(1000, 1000)).unwrap();
            let edge = cell().x + cell().w;
            let low = bar.track.y_top + bar.track.h - 2.0;
            assert_eq!(bar.press(edge - 1.0, low), Some(ScrollbarGrab { at: bar.thumb.h * 0.5 }));
            let hit_w = chrome_spec::px(chrome_spec::SCROLLBAR_HIT_W);
            assert_eq!(bar.press(edge - hit_w - 1.0, low), None);
            assert_eq!(bar.press(edge, low), None);
            assert_eq!(bar.press(edge - 1.0, bar.track.y_top - 1.0), None);
        });
    }
}

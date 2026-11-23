//! The sidebar's list: one row per pane — a status dot, the pane's
//! name, and a close button that shows only on the row the pointer is
//! in and on the row of the pane in use.
//!
//! Paint only.  Where the rows and their close buttons are is the
//! layout's business (`marspot_term::layout`), and so is hit-testing;
//! this draws at the rects it is given.  Sizes are `spec::chrome`'s,
//! colours `panel_palette`'s.

use marspot_term::layout::Rect;
use marspot_term::session::SessionState;

use crate::ui::components::controls;
use crate::ui::components::panel_palette as palette;
use crate::ui::core::ViewPainter;
use crate::ui::system::macos::icons::{self, Symbol};
use crate::ui::theme::spec::chrome;

pub struct Sidebar {
    /// The sidebar column, from under the window's header to the
    /// window's bottom.
    pub rect: Rect,
    /// From the column's top to the first row.
    pub top_pad: f64,
}

/// The longest prefix of `s` that is at most `max_chars` characters.
pub(crate) fn clip_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((end, _)) => &s[..end],
        None => s,
    }
}

impl Sidebar {
    pub fn row_rect(&self, i: usize) -> Rect {
        Rect {
            x: self.rect.x,
            y_top: self.rect.y_top + self.top_pad + i as f64 * chrome::px(chrome::SIDEBAR_ROW_H),
            w: self.rect.w,
            h: chrome::px(chrome::SIDEBAR_ROW_H),
        }
    }

    /// The row at the column's bottom, whatever the rows above reach.
    pub fn bottom_row(&self) -> Rect {
        let h = chrome::px(chrome::SIDEBAR_ROW_H);
        Rect { x: self.rect.x, y_top: (self.rect.y_top + self.rect.h - h).max(self.rect.y_top), w: self.rect.w, h }
    }

    /// The hover and selected fill of row `i`: the row, short of the
    /// sidebar's edges.
    pub fn fill_rect(&self, i: usize) -> Rect {
        row_fill(self.row_rect(i))
    }
}

/// How a row's close button looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum CloseLook {
    /// The last pane: a mark that does nothing.
    Disabled = 0,
    /// Under the pointer.
    Hot = 1,
    /// Showing, not under the pointer.
    Rest = 2,
}

pub(crate) fn close_look(can_close: bool, hot: bool) -> CloseLook {
    if !can_close {
        CloseLook::Disabled
    } else if hot {
        CloseLook::Hot
    } else {
        CloseLook::Rest
    }
}

/// The hover and selected fill of the row in `row`: short of the
/// sidebar's edges.
fn row_fill(row: Rect) -> Rect {
    Rect {
        x: row.x + chrome::px(chrome::SIDEBAR_ROW_INSET),
        w: (row.w - 2.0 * chrome::px(chrome::SIDEBAR_ROW_INSET)).max(0.0),
        ..row
    }
}

fn status_dot(row: Rect) -> Rect {
    Rect {
        x: row.x + chrome::px(chrome::SIDEBAR_PAD_X),
        y_top: row.y_top + (row.h - chrome::px(chrome::SIDEBAR_DOT)) * 0.5,
        w: chrome::px(chrome::SIDEBAR_DOT),
        h: chrome::px(chrome::SIDEBAR_DOT),
    }
}

/// How many mono cells of a name fit in the row `row`.  The name stops
/// short of where the close button goes, shown or not.
pub(crate) fn name_room(row: Rect, close: Option<&Rect>, cell_w: f32) -> usize {
    let dot = status_dot(row);
    let label_x = dot.x + dot.w + chrome::px(chrome::SIDEBAR_DOT_GAP);
    let label_right = close.map_or(row.x + row.w, |c| c.x) - chrome::px(chrome::SIDEBAR_LABEL_GAP);
    ((label_right - label_x) / cell_w as f64).floor().max(0.0) as usize
}

/// One row in `rect`: its fill, its status dot and `name`, already cut
/// to the room it has.
pub(crate) fn paint_row(p: &mut ViewPainter, rect: Rect, name: &str, state: SessionState, selected: bool, hovered: bool) {
    let radius = chrome::px(chrome::SIDEBAR_ROW_RADIUS) as f32;
    if selected {
        p.fill_rounded_rect(row_fill(rect), palette::chrome_on(), radius, (palette::none(), 0.0));
    } else if hovered {
        p.fill_rounded_rect(row_fill(rect), palette::chrome_hover(), radius, (palette::none(), 0.0));
    }
    let dot = status_dot(rect);
    let dot_color = match state {
        SessionState::Active => palette::ok(),
        SessionState::Idle => palette::chrome_icon(),
        SessionState::Exited => palette::danger(),
    };
    p.fill_rounded_rect(dot, dot_color, (chrome::px(chrome::SIDEBAR_DOT) * 0.5) as f32, (palette::none(), 0.0));
    let label_x = dot.x + dot.w + chrome::px(chrome::SIDEBAR_DOT_GAP);
    let baseline = (rect.y_top + (rect.h - p.cell_h as f64) * 0.5) as f32 + p.ascent;
    let fg = if selected { palette::fg() } else { palette::chrome_fg() };
    p.text(label_x as f32, baseline, name, fg);
}

/// The row at the column's bottom: opaque, so a row the column is too
/// short for does not show through it.
pub(crate) fn paint_bottom_row(p: &mut ViewPainter, rect: Rect, label: &str, hovered: bool) {
    p.fill_rounded_rect(rect, palette::pane_bg(false), 0.0, (palette::none(), 0.0));
    if hovered {
        let radius = chrome::px(chrome::SIDEBAR_ROW_RADIUS) as f32;
        p.fill_rounded_rect(row_fill(rect), palette::chrome_hover(), radius, (palette::none(), 0.0));
    }
    let x = rect.x + chrome::px(chrome::SIDEBAR_PAD_X);
    let baseline = (rect.y_top + (rect.h - p.cell_h as f64) * 0.5) as f32 + p.ascent;
    p.text(x as f32, baseline, label, palette::chrome_fg());
}

/// A row's close button in `close`.
pub(crate) fn paint_close(p: &mut ViewPainter, close: Rect, look: CloseLook) {
    match look {
        CloseLook::Disabled => controls::paint_danger_mark(p, close, palette::disabled_fg()),
        CloseLook::Hot => controls::paint_danger_button(p, close, true),
        CloseLook::Rest => controls::paint_danger_mark(p, close, palette::chrome_icon()),
    }
}

/// The add button in `add`: a ghost, its `+` and a fill only under the
/// pointer, as wide as a row's fill in a sidebar `sidebar_w` wide so
/// the two line up.  At the pane limit it is drawn disabled.
pub(crate) fn paint_add(p: &mut ViewPainter, add: Rect, sidebar_w: f64, full: bool, hovered: bool) {
    let hovered = hovered && !full;
    if hovered {
        let fill = Rect {
            x: chrome::px(chrome::SIDEBAR_ROW_INSET),
            w: (sidebar_w - 2.0 * chrome::px(chrome::SIDEBAR_ROW_INSET)).max(0.0),
            ..add
        };
        p.fill_rounded_rect(fill, palette::chrome_hover(), chrome::px(chrome::SIDEBAR_ROW_RADIUS) as f32, (palette::none(), 0.0));
    }
    let fg = if full {
        palette::disabled_fg()
    } else if hovered {
        palette::fg()
    } else {
        palette::chrome_icon()
    };
    icons::paint(p, Symbol::Plus, add, chrome::px(chrome::SIDEBAR_ADD_ICON), fg);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sidebar() -> Sidebar {
        Sidebar { rect: Rect { x: 0.0, y_top: 64.0, w: 200.0, h: 800.0 }, top_pad: 38.0 }
    }

    #[test]
    fn sidebar_rows_step_by_the_row_height_under_the_top_pad() {
        let sb = sidebar();
        assert_eq!(sb.row_rect(0).y_top, 64.0 + 38.0);
        assert_eq!(sb.row_rect(3).y_top - sb.row_rect(0).y_top, 3.0 * chrome::px(chrome::SIDEBAR_ROW_H));
        assert_eq!(sb.row_rect(2).w, 200.0);
    }

    #[test]
    fn sidebar_row_fill_is_inset_from_both_edges() {
        let sb = sidebar();
        let (row, fill) = (sb.row_rect(1), sb.fill_rect(1));
        assert_eq!(fill.x - row.x, chrome::px(chrome::SIDEBAR_ROW_INSET));
        assert_eq!(row.x + row.w - (fill.x + fill.w), chrome::px(chrome::SIDEBAR_ROW_INSET));
        assert_eq!((fill.y_top, fill.h), (row.y_top, row.h));
    }

    #[test]
    fn sidebar_names_are_cut_on_a_character_boundary() {
        assert_eq!(clip_chars("insight-mono", 7), "insight");
        assert_eq!(clip_chars("alt", 7), "alt");
        assert_eq!(clip_chars("日本語の名前", 3), "日本語");
        assert_eq!(clip_chars("abc", 0), "");
    }
}

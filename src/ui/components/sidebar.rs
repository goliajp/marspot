//! The sidebar's list: one row per pane — a status dot, the pane's
//! name, and a close button that shows only on the row the pointer is
//! in and on the row of the pane in use.
//!
//! Paint only.  Where the rows and their close buttons are is the
//! layout's business (`marspot_term::layout`), and so is hit-testing;
//! this draws at the rects it is given.  Sizes are `spec::chrome`'s,
//! colours `panel_palette`'s.

use marspot_term::layout::Rect;
use marspot_term::render::SidebarEntry;
use marspot_term::session::SessionState;

use crate::ui::components::chrome_hover::ChromeHover;
use crate::ui::components::controls;
use crate::ui::components::panel_palette as palette;
use crate::ui::core::ViewPainter;
use crate::ui::theme::spec::chrome;

pub struct Sidebar<'a> {
    /// The sidebar column, from under the window's header to the
    /// window's bottom.
    pub rect: Rect,
    /// From the column's top to the first row.
    pub top_pad: f64,
    pub rows: &'a [SidebarEntry<'a>],
    /// Each row's close button, as the layout placed it.
    pub close_rects: &'a [Rect],
    /// The row of the pane in use.
    pub focused_idx: usize,
    pub hover: Option<ChromeHover>,
}

/// The longest prefix of `s` that is at most `max_chars` characters.
pub(crate) fn clip_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((end, _)) => &s[..end],
        None => s,
    }
}

impl Sidebar<'_> {
    pub fn row_rect(&self, i: usize) -> Rect {
        Rect {
            x: self.rect.x,
            y_top: self.rect.y_top + self.top_pad + i as f64 * chrome::px(chrome::SIDEBAR_ROW_H),
            w: self.rect.w,
            h: chrome::px(chrome::SIDEBAR_ROW_H),
        }
    }

    /// The hover and selected fill of row `i`: the row, short of the
    /// sidebar's edges.
    pub fn fill_rect(&self, i: usize) -> Rect {
        let row = self.row_rect(i);
        Rect {
            x: row.x + chrome::px(chrome::SIDEBAR_ROW_INSET),
            w: (row.w - 2.0 * chrome::px(chrome::SIDEBAR_ROW_INSET)).max(0.0),
            ..row
        }
    }

    pub fn paint(&self, p: &mut ViewPainter) {
        let radius = chrome::px(chrome::SIDEBAR_ROW_RADIUS) as f32;
        // The last pane cannot be closed.
        let can_close = self.rows.len() > 1;
        for (i, row) in self.rows.iter().enumerate() {
            let rect = self.row_rect(i);
            let selected = i == self.focused_idx;
            let hovered = self.hover.and_then(ChromeHover::row) == Some(i);
            if selected {
                p.fill_rounded_rect(self.fill_rect(i), palette::chrome_on(), radius, (palette::none(), 0.0));
            } else if hovered {
                p.fill_rounded_rect(self.fill_rect(i), palette::chrome_hover(), radius, (palette::none(), 0.0));
            }

            let dot = Rect {
                x: rect.x + chrome::px(chrome::SIDEBAR_PAD_X),
                y_top: rect.y_top + (rect.h - chrome::px(chrome::SIDEBAR_DOT)) * 0.5,
                w: chrome::px(chrome::SIDEBAR_DOT),
                h: chrome::px(chrome::SIDEBAR_DOT),
            };
            let dot_color = match row.state {
                SessionState::Active => palette::ok(),
                SessionState::Idle => palette::chrome_icon(),
                SessionState::Exited => palette::danger(),
            };
            p.fill_rounded_rect(dot, dot_color, (chrome::px(chrome::SIDEBAR_DOT) * 0.5) as f32, (palette::none(), 0.0));

            // The name stops short of where the close button goes,
            // shown or not.
            let label_x = dot.x + dot.w + chrome::px(chrome::SIDEBAR_DOT_GAP);
            let close = self.close_rects.get(i);
            let label_right = close.map_or(rect.x + rect.w, |c| c.x) - chrome::px(chrome::SIDEBAR_LABEL_GAP);
            let room = ((label_right - label_x) / p.cell_w as f64).floor().max(0.0) as usize;
            let baseline = (rect.y_top + (rect.h - p.cell_h as f64) * 0.5) as f32 + p.ascent;
            let fg = if selected { palette::fg() } else { palette::chrome_fg() };
            p.text(label_x as f32, baseline, clip_chars(row.label, room), fg);

            if let Some(&close) = close.filter(|_| selected || hovered) {
                if !can_close {
                    controls::paint_danger_mark(p, close, palette::disabled_fg());
                } else if self.hover == Some(ChromeHover::Close(i)) {
                    controls::paint_danger_button(p, close, true);
                } else {
                    controls::paint_danger_mark(p, close, palette::chrome_icon());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sidebar<'a>(rows: &'a [SidebarEntry<'a>]) -> Sidebar<'a> {
        Sidebar {
            rect: Rect { x: 0.0, y_top: 64.0, w: 200.0, h: 800.0 },
            top_pad: 38.0,
            rows,
            close_rects: &[],
            focused_idx: 0,
            hover: None,
        }
    }

    #[test]
    fn sidebar_rows_step_by_the_row_height_under_the_top_pad() {
        let sb = sidebar(&[]);
        assert_eq!(sb.row_rect(0).y_top, 64.0 + 38.0);
        assert_eq!(sb.row_rect(3).y_top - sb.row_rect(0).y_top, 3.0 * chrome::SIDEBAR_ROW_H);
        assert_eq!(sb.row_rect(2).w, 200.0);
    }

    #[test]
    fn sidebar_row_fill_is_inset_from_both_edges() {
        let sb = sidebar(&[]);
        let (row, fill) = (sb.row_rect(1), sb.fill_rect(1));
        assert_eq!(fill.x - row.x, chrome::SIDEBAR_ROW_INSET);
        assert_eq!(row.x + row.w - (fill.x + fill.w), chrome::SIDEBAR_ROW_INSET);
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

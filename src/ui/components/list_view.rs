//! `ListView` — vertical list of selectable rows.  One row may carry
//! a "focused" mark, drawn with a rounded BG highlight and a brighter
//! text color.  Truncates rows past the viewport (no scroll yet; pair
//! with `ScrollView` later when needed).
//!
//! No input handling here — the caller hit-tests `row_rect(i)` and
//! reacts.  Paint-only.

use marspot_term::layout::Rect;
use crate::ui::core::ViewPainter;

#[derive(Debug, Clone, Copy)]
pub struct ListViewStyle {
    pub fg: [f32; 4],
    pub fg_focused: [f32; 4],
    pub focused_bg: [f32; 4],
    /// Corner radius of the focused-row highlight rect.
    pub focused_radius: f32,
    /// Horizontal inset for the focused-row highlight (so it doesn't
    /// touch the panel border — typically 4 px).
    pub focused_inset_x: f32,
}

impl Default for ListViewStyle {
    fn default() -> Self {
        Self {
            fg:           [0.60, 0.63, 0.70, 1.0],
            fg_focused:   [0.95, 0.96, 0.97, 1.0],
            focused_bg:   [0.18, 0.28, 0.48, 1.0],
            focused_radius: 5.0,
            focused_inset_x: 4.0,
        }
    }
}

pub struct ListRow<'a> {
    pub label: &'a str,
    pub is_focused: bool,
}

pub struct ListView<'a> {
    /// Viewport rect.  Rows beyond it are clipped (skipped).
    pub rect: Rect,
    pub rows: &'a [ListRow<'a>],
    /// Row height in physical px.  Caller usually sets to `cell_h`.
    pub row_h: f32,
    /// Padding inside the row on the left (text x offset).
    pub text_pad_left: f32,
    pub style: ListViewStyle,
}

impl<'a> ListView<'a> {
    /// Rect for the i-th row (no scroll math; just `rect.x` +
    /// `i * row_h`).  Useful for hit-testing on the caller side.
    pub fn row_rect(&self, i: usize) -> Rect {
        Rect {
            x: self.rect.x,
            y_top: self.rect.y_top + (i as f64) * (self.row_h as f64),
            w: self.rect.w,
            h: self.row_h as f64,
        }
    }

    pub fn paint(&self, p: &mut ViewPainter) {
        let cell_w = p.cell_w;
        let cell_h = p.cell_h;
        let ascent = p.ascent;
        let visible_h = self.rect.h as f32;
        let max_visible = (visible_h / self.row_h).floor() as usize;

        for (i, row) in self.rows.iter().enumerate().take(max_visible) {
            let row_y = self.rect.y_top as f32 + (i as f32) * self.row_h;
            if row.is_focused {
                p.fill_rounded_rect(
                    Rect {
                        x: self.rect.x + self.style.focused_inset_x as f64,
                        y_top: row_y as f64,
                        w: self.rect.w - 2.0 * self.style.focused_inset_x as f64,
                        h: self.row_h as f64,
                    },
                    self.style.focused_bg,
                    self.style.focused_radius,
                    ([0.0, 0.0, 0.0, 0.0], 0.0),
                );
            }
            // Ellipsised, not chopped: a snippet that ran out of room
            // used to end mid-word with no mark, which reads as a
            // snippet that ended there (2026-08-11, search results).
            let display = crate::ui::core::fit_mono(
                row.label,
                self.rect.w,
                cell_w as f64,
                self.text_pad_left as f64,
            );
            let baseline = row_y + (self.row_h - cell_h) * 0.5 + ascent;
            let fg = if row.is_focused {
                self.style.fg_focused
            } else {
                self.style.fg
            };
            p.text(
                self.rect.x as f32 + self.text_pad_left,
                baseline,
                &display,
                fg,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y_top: y, w, h }
    }

    #[test]
    fn row_rect_at_index() {
        let rows = [];
        let lv = ListView {
            rect: rect(0.0, 50.0, 200.0, 200.0),
            rows: &rows,
            row_h: 20.0,
            text_pad_left: 4.0,
            style: ListViewStyle::default(),
        };
        let r0 = lv.row_rect(0);
        assert_eq!(r0.y_top, 50.0);
        assert_eq!(r0.h, 20.0);
        let r3 = lv.row_rect(3);
        assert_eq!(r3.y_top, 50.0 + 3.0 * 20.0);
    }

    #[test]
    fn default_style_focused_bg_opaque() {
        let s = ListViewStyle::default();
        assert_eq!(s.focused_bg[3], 1.0, "focused row BG must be opaque");
        assert_eq!(s.fg[3], 1.0);
        assert_eq!(s.fg_focused[3], 1.0);
    }
}

//! Sidebar list — one entry per session.  Each row: optional focus
//! BG highlight + a coloured status dot + the session label.  The dot
//! is rendered as an `fill_rounded_rect` with `corner_radius = r` so
//! it reads as a circle without depending on a dedicated DOT
//! pipeline.
//!
//! Hit-testing (which row the user clicked, where to inject the
//! [×] / [+] glyphs) stays in the scene — this component is paint-
//! only.  `row_rect(i)` returns the per-row bounding box so the
//! scene can compute close [×] / add [+] positions from the same
//! geometry the renderer used.

use marspot_term::layout::Rect;
use crate::ui::core::ViewPainter;

#[derive(Debug, Clone)]
pub struct SidebarRow<'a> {
    pub label: &'a str,
    pub dot_color: [f32; 4],
}

#[derive(Debug, Clone, Copy)]
pub struct SidebarStyle {
    pub focused_bg: [f32; 4],
    pub label_fg: [f32; 4],
    pub row_h: f32,
    pub top_pad: f32,
    pub left_pad: f32,
    pub dot_radius: f32,
    pub dot_label_gap: f32,
}

impl Default for SidebarStyle {
    fn default() -> Self {
        Self {
            focused_bg: [0.0, 0.0, 0.0, 1.0],
            label_fg:   [0.78, 0.82, 0.88, 1.0],
            row_h: 22.0,
            top_pad: 0.0,
            left_pad: 14.0,
            dot_radius: 4.5,
            dot_label_gap: 10.0,
        }
    }
}

pub struct Sidebar<'a> {
    /// Bounding rect of the sidebar column.  `x` is the window left
    /// edge (always 0 today); `w` is the sidebar width; `y_top` is
    /// the top inset (header chrome reserved above).  `h` extends
    /// to the window bottom.
    pub rect: Rect,
    pub rows: &'a [SidebarRow<'a>],
    pub focused_idx: usize,
    pub style: SidebarStyle,
}

impl<'a> Sidebar<'a> {
    /// Per-row bounding box.  Equivalent to the scene's hit-test
    /// formula — return this so click handlers and renderers agree.
    pub fn row_rect(&self, i: usize) -> Rect {
        Rect {
            x: self.rect.x,
            y_top: self.rect.y_top + (self.style.top_pad as f64)
                + (i as f64) * (self.style.row_h as f64),
            w: self.rect.w,
            h: self.style.row_h as f64,
        }
    }

    pub fn paint(&self, p: &mut ViewPainter) {
        for (i, row) in self.rows.iter().enumerate() {
            let row_rect = self.row_rect(i);
            // Focus BG (full row width).
            if i == self.focused_idx {
                p.fill_rounded_rect(row_rect, self.style.focused_bg, 0.0,
                    ([0.0, 0.0, 0.0, 0.0], 0.0));
            }
            // Status dot — rounded rect with radius = w/2 reads as a
            // circle.  Replaces the legacy DOT pipeline use.
            let dot_d = (self.style.dot_radius * 2.0) as f64;
            let dot_x = row_rect.x + (self.style.left_pad as f64);
            let dot_y = row_rect.y_top
                + (row_rect.h - dot_d) * 0.5;
            p.fill_rounded_rect(
                Rect { x: dot_x, y_top: dot_y, w: dot_d, h: dot_d },
                row.dot_color,
                (self.style.dot_radius) as f32,
                ([0.0, 0.0, 0.0, 0.0], 0.0),
            );
            // Label.  Align cap-height roughly with dot centre:
            // baseline at dot_cy + ascent*0.30 in y-down (matches
            // the legacy push_sidebar formula).
            let label_x = dot_x + dot_d
                + (self.style.dot_label_gap as f64);
            let dot_cy = row_rect.y_top + row_rect.h * 0.5;
            let baseline = dot_cy as f32 + p.ascent * 0.30;
            p.text(label_x as f32, baseline, row.label, self.style.label_fg);
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
    fn row_rect_stride_is_row_h() {
        let rows = [];
        let sb = Sidebar {
            rect: rect(0.0, 64.0, 160.0, 800.0),
            rows: &rows,
            focused_idx: 0,
            style: SidebarStyle::default(),
        };
        let r0 = sb.row_rect(0);
        let r3 = sb.row_rect(3);
        assert_eq!(r0.y_top, 64.0);
        assert_eq!(r3.y_top - r0.y_top, 3.0 * (sb.style.row_h as f64));
    }

    #[test]
    fn row_rect_inherits_sidebar_width() {
        let rows = [];
        let sb = Sidebar {
            rect: rect(0.0, 50.0, 200.0, 800.0),
            rows: &rows,
            focused_idx: 0,
            style: SidebarStyle::default(),
        };
        assert_eq!(sb.row_rect(2).w, 200.0);
    }

    #[test]
    fn default_style_label_opaque() {
        let s = SidebarStyle::default();
        assert_eq!(s.label_fg[3], 1.0);
        assert_eq!(s.focused_bg[3], 1.0);
    }
}

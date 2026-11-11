//! What the pointer is on in the main window's chrome.
//!
//! One answer per pointer position, asked by the mouse handler and
//! read by the painter, so a control lights up exactly where a click
//! would land on it.

use marspot_term::layout::Layout;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromeHover {
    /// A toolbar button, by its place in `Layout::toolbar_buttons`.
    Toolbar(u8),
    /// The sidebar's add button.
    Add,
    /// A sidebar row, anywhere but its close button.
    Row(usize),
    /// The close button of a sidebar row.
    Close(usize),
}

impl ChromeHover {
    /// What `(x, y)` is on in a window laid out as `layout`, whose
    /// sidebar lists `n_rows` panes; `None` off the chrome.
    pub fn at(layout: &Layout, n_rows: usize, x: f64, y: f64) -> Option<Self> {
        if let Some(i) = layout.toolbar_buttons().iter().position(|r| r.contains(x, y)) {
            return Some(Self::Toolbar(i as u8));
        }
        if layout.hit_test_add_session_button(x, y) {
            return Some(Self::Add);
        }
        if let Some(i) = layout.hit_test_close_session(x, y) {
            return Some(Self::Close(i));
        }
        let rows_top = layout.top_inset + layout.sidebar_top_pad_phys;
        layout.hit_test_sidebar_row(x, y, rows_top, marspot_term::layout::sidebar_row_h(), n_rows).map(Self::Row)
    }

    /// The sidebar row the pointer is in, on its close button or not.
    pub fn row(self) -> Option<usize> {
        match self {
            Self::Row(i) | Self::Close(i) => Some(i),
            _ => None,
        }
    }

    /// Whether the pointer is on the toolbar: all that can be reached
    /// while a panel lies over the window and the sidebar under it.
    pub fn on_toolbar(&self) -> bool {
        matches!(self, Self::Toolbar(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Layout {
        Layout::build(1400.0, 900.0, 200.0, 32.0, 22.0, 2, 2, 8.0, 16.0)
            .with_chrome(1.0, 3, Some(70.0))
    }

    fn centre(r: marspot_term::layout::Rect) -> (f64, f64) {
        (r.x + r.w * 0.5, r.y_top + r.h * 0.5)
    }

    #[test]
    fn chrome_hover_names_what_a_click_would_hit() {
        let l = layout();
        let at = |(x, y): (f64, f64)| ChromeHover::at(&l, 3, x, y);
        assert_eq!(at(centre(l.sidebar_button_rect)), Some(ChromeHover::Toolbar(0)));
        assert_eq!(at(centre(l.settings_button_rect)), Some(ChromeHover::Toolbar(4)));
        assert_eq!(at(centre(l.add_session_button_rect)), Some(ChromeHover::Add));
        assert_eq!(at(centre(l.close_session_rects[1])), Some(ChromeHover::Close(1)));
        // Left of the close button, the same row.
        let (_, y) = centre(l.close_session_rects[1]);
        assert_eq!(at((40.0, y)), Some(ChromeHover::Row(1)));
        assert_eq!(ChromeHover::Row(1).row(), Some(1));
        assert_eq!(ChromeHover::Close(1).row(), Some(1));
        assert_eq!(ChromeHover::Add.row(), None);
    }

    #[test]
    fn chrome_hover_is_none_off_the_chrome() {
        let l = layout();
        // In a pane, below the last row, and at the window's corner.
        assert_eq!(ChromeHover::at(&l, 3, 700.0, 400.0), None);
        let below = l.close_session_rects[2].y_top + 2.0 * marspot_term::layout::SIDEBAR_ROW_H;
        assert_eq!(ChromeHover::at(&l, 3, 40.0, below), None);
        assert_eq!(ChromeHover::at(&l, 3, 0.0, 0.0), None);
    }

    #[test]
    fn chrome_hover_knows_the_toolbar_from_the_sidebar() {
        assert!(ChromeHover::Toolbar(2).on_toolbar());
        for h in [ChromeHover::Add, ChromeHover::Row(0), ChromeHover::Close(0)] {
            assert!(!h.on_toolbar());
        }
    }
}

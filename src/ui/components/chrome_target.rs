//! What a point is on in the main window's chrome: a toolbar button,
//! the sidebar's add button, a row or its close button, a pane's title
//! strip or its badge's clickable prefix.
//!
//! Each is a node of the toolbar's, the sidebar's or the title strips'
//! tree, and the answer comes from hit testing the layout those trees
//! were last laid out to: what lights under the pointer and where a
//! press lands are the nodes the frame was drawn from.

use golia_ui_core::Action;

use super::{sidebar_view, title_strip_view, toolbar_view};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChromeTarget {
    /// A toolbar button, by its place in `Layout::toolbar_buttons`.
    Toolbar(u8),
    /// The sidebar's add button.
    Add,
    /// A sidebar row, anywhere but its close button.
    Row(usize),
    /// The close button of a sidebar row, shown or not.
    Close(usize),
    /// A pane's title strip, anywhere but its badge's prefix.
    Title(usize),
    /// The clickable prefix of a pane's badge.
    Badge(usize),
    /// The `+N` after a pane's title.
    Helpers(usize),
    /// The sidebar's row that counts every shadow pane.
    AllHelpers,
}

impl ChromeTarget {
    /// The target a node's action names.
    pub fn of(a: Action) -> Option<Self> {
        let i = a.arg as usize;
        Some(match a.id {
            toolbar_view::PRESS => Self::Toolbar(a.arg as u8),
            sidebar_view::ADD => Self::Add,
            sidebar_view::ROW => Self::Row(i),
            sidebar_view::CLOSE => Self::Close(i),
            title_strip_view::TITLE => Self::Title(i),
            title_strip_view::BADGE => Self::Badge(i),
            title_strip_view::HELPERS => Self::Helpers(i),
            sidebar_view::HELPERS => Self::AllHelpers,
            _ => return None,
        })
    }

    /// The sidebar row the point is in, on its close button or not.
    pub fn row(self) -> Option<usize> {
        match self {
            Self::Row(i) | Self::Close(i) => Some(i),
            _ => None,
        }
    }

    /// Whether this acts when the button is let go on it, having been
    /// pressed on it: a button.  A row and a strip act on the press,
    /// which focuses a pane and, on a strip, may start dragging it.
    pub fn acts_on_release(self) -> bool {
        matches!(self, Self::Toolbar(_) | Self::Add | Self::Close(_) | Self::Badge(_) | Self::Helpers(_) | Self::AllHelpers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_target_is_named_by_its_own_action_and_no_other() {
        let all = [
            ChromeTarget::Toolbar(3),
            ChromeTarget::Add,
            ChromeTarget::Row(7),
            ChromeTarget::Close(7),
            ChromeTarget::Title(7),
            ChromeTarget::Badge(7),
            ChromeTarget::Helpers(7),
            ChromeTarget::AllHelpers,
        ];
        let actions = [
            Action::new(toolbar_view::PRESS, 3),
            Action::new(sidebar_view::ADD, 0),
            Action::new(sidebar_view::ROW, 7),
            Action::new(sidebar_view::CLOSE, 7),
            Action::new(title_strip_view::TITLE, 7),
            Action::new(title_strip_view::BADGE, 7),
            Action::new(title_strip_view::HELPERS, 7),
            Action::new(sidebar_view::HELPERS, 0),
        ];
        for (t, a) in all.iter().zip(actions) {
            assert_eq!(ChromeTarget::of(a), Some(*t));
        }
        assert_eq!(ChromeTarget::of(Action::new(99, 0)), None);
        assert_eq!(ChromeTarget::Close(7).row(), Some(7));
        assert_eq!(ChromeTarget::Row(7).row(), Some(7));
        assert_eq!(ChromeTarget::Add.row(), None);
        let on_release: Vec<bool> = all.iter().map(|t| t.acts_on_release()).collect();
        assert_eq!(on_release, [true, true, false, true, false, true, true, true]);
    }
}

//! The rows a sidebar lists: the window's panes and their children, or
//! blanks where only where the rows are matters.

use crate::render::{SidebarEntry, SidebarTree};
use marspot_term::session::SessionState;

/// The rows a sidebar lists.
pub trait Rows {
    fn count(&self) -> usize;
    fn label(&self, i: usize) -> &str;
    fn state(&self, i: usize) -> SessionState;
    fn code(&self, i: usize) -> Option<u16>;
    fn tree(&self, i: usize) -> SidebarTree;
    fn hollow(&self, i: usize) -> bool;
}

impl Rows for &[SidebarEntry<'_>] {
    fn count(&self) -> usize {
        <[SidebarEntry<'_>]>::len(self)
    }
    fn label(&self, i: usize) -> &str {
        self[i].label
    }
    fn state(&self, i: usize) -> SessionState {
        self[i].state
    }
    fn code(&self, i: usize) -> Option<u16> {
        self[i].code
    }
    fn tree(&self, i: usize) -> SidebarTree {
        self[i].tree
    }
    fn hollow(&self, i: usize) -> bool {
        self[i].hollow
    }
}

/// The row to light: rows are not slots, so the focused slot's index
/// says nothing about them, and with no row marked none is lit.
pub fn selected_row(rows: &[SidebarEntry]) -> Option<usize> {
    rows.iter().position(|e| e.selected)
}

/// `n` rows with nothing to show, for laying the sidebar out where
/// only where its rows are matters.
pub struct Blank(pub usize);

impl Rows for Blank {
    fn count(&self) -> usize {
        self.0
    }
    fn label(&self, _: usize) -> &str {
        ""
    }
    fn state(&self, _: usize) -> SessionState {
        SessionState::Active
    }
    fn code(&self, _: usize) -> Option<u16> {
        None
    }
    fn tree(&self, _: usize) -> SidebarTree {
        SidebarTree::Top
    }
    fn hollow(&self, _: usize) -> bool {
        false
    }
}

#[cfg(test)]
#[test]
fn a_focused_slot_without_a_row_lights_no_row() {
    let mut rows = [SidebarEntry::top("a", SessionState::Idle, None), SidebarEntry::top("b", SessionState::Idle, None)];
    assert_eq!(selected_row(&rows), None);
    rows[1].selected = true;
    assert_eq!(selected_row(&rows), Some(1));
}

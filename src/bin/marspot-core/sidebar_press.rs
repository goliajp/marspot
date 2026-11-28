//! What a press on a sidebar row does.  A row is a pane of this
//! window, of another one or of none: the first is focused, the second
//! is focused in its window, which comes to the front, and the third is
//! shown in this window beside the pane in use.

use marspot::render::SidebarTree;

use crate::draw::sidebar_tree::{Row, RowPane};
use super::*;

impl CoreApp {
    /// A press on row `row` of window `wi`'s sidebar.
    pub(crate) fn sidebar_row_pressed(&mut self, wi: usize, row: usize) {
        let Some(r) = self.sidebar_row(wi, row) else { return };
        match r.pane {
            RowPane::Slot(idx) => self.focus_pane(wi, idx),
            RowPane::Other(sid) => self.reach_pane(wi, sid),
        }
        lx_event!("SIDEBAR_ROW_PRESSED", "a sidebar row was pressed", row = row as u32,
            grey = (r.tree == SidebarTree::Parent) as u32);
    }

    /// Pane `sid`, of no slot of window `wi`: focused in the window it
    /// is in, which comes to the front, or shown in `wi`.
    pub(crate) fn reach_pane(&mut self, wi: usize, sid: u64) {
        if let Some((wj, idx)) = self.find_window_pane_by_sid(sid) {
            self.raise_window(wj);
            self.focus_pane(wj, idx);
            win!(self, wj).needs_render = true;
        } else if self.shadow_index(sid).is_some() {
            self.show_here(wi, sid);
        }
    }

    /// Ask the shell to bring window `wi` to the front.  The core's key
    /// window moves only with the shell's `WindowFocus`: a shell that
    /// does not know the frame leaves the window behind, and keys must
    /// not go to a window the user cannot see.
    pub(crate) fn raise_window(&mut self, wi: usize) {
        let id = win!(self, wi).window_id;
        self.pending_to_shell.push((MsgType::WindowRaise, marspot::shell_proto::encode_window_raise(id)));
        win!(self, wi).needs_render = true;
    }

    /// Hidden pane `sid` shown in window `wi` beside the pane in use,
    /// its parent kept, and focused; the grid grows a column, else a
    /// row, when it has no cell for it.  A window holding as many panes
    /// as it can says so over the pane in use instead.
    pub(crate) fn show_here(&mut self, wi: usize, sid: u64) {
        if self.window_full(wi) {
            return;
        }
        let Some(i) = self.shadow_index(sid) else { return };
        let mut pane = self.take_shadow(i);
        unwatched::set(sid, false);
        // a shadow's frames were not read while it was one
        pane.pump();
        let w = &mut win!(self, wi);
        let at = (w.focused_idx + 1).min(w.panes.len());
        if w.panes.len() + 1 > w.grid_cols * w.grid_rows {
            if w.grid_cols < GRID_MAX {
                w.grid_cols += 1;
            } else if w.grid_rows < GRID_MAX {
                w.grid_rows += 1;
            }
        }
        w.panes.insert(at, pane);
        w.focused_idx = at;
        if w.selection.is_some_and(|s| s.session_idx >= at) {
            w.selection = None;
            w.selection_dragging = false;
        }
        self.rebuild_layout(wi);
        self.shadows_changed();
        self.save_session_state();
        lx_event!("PANE_SHOWN_HERE", "a hidden pane was shown in a window from the sidebar", session = sid,
            window_id = win!(self, wi).window_id, slot = at as u32);
    }

    /// Whether window `wi` holds as many panes as a window can, in
    /// which case it says so over the pane in use.
    pub(crate) fn window_full(&mut self, wi: usize) -> bool {
        let n = win!(self, wi).panes.len();
        if n < SESSION_COUNT_HARD_CAP {
            return false;
        }
        self.show_notice(strings::text(Arg::WindowFull(n)));
        lx_event!("WINDOW_FULL", "a pane was refused a window that holds as many as it can",
            window_id = win!(self, wi).window_id, panes = n as u32);
        true
    }

    /// The close button of row `row` of window `wi`'s sidebar.
    pub(crate) fn close_row(&mut self, wi: usize, row: usize) {
        let Some(r) = self.sidebar_row(wi, row).filter(|r| r.tree != SidebarTree::Parent) else { return };
        let sid = match r.pane {
            RowPane::Slot(idx) => win!(self, wi).panes.get(idx).and_then(Pane::shelld_session_id),
            RowPane::Other(sid) => Some(sid),
        };
        // a pane with children asks first
        if sid.is_some_and(|sid| self.close_asked(wi, sid)) {
            return;
        }
        match r.pane {
            RowPane::Slot(idx) => self.close_slot(wi, idx),
            RowPane::Other(sid) => {
                self.close_anywhere(sid, false);
            }
        }
    }

    /// Close slot `idx` of window `wi`; the last pane closes too (see
    /// `close_window_if_emptied`).
    pub(crate) fn close_slot(&mut self, wi: usize, idx: usize) {
        if idx < win!(self, wi).panes.len() {
            self.close_session(wi, idx);
            if wi < self.windows.len() {
                self.rebuild_layout(wi);
            }
        }
    }

    /// Close pane `sid` wherever it is, its session's directory kept
    /// for the recycle period when `recycle`; false when there is none.
    pub(crate) fn close_anywhere(&mut self, sid: u64, recycle: bool) -> bool {
        if let Some((wi, idx)) = self.find_window_pane_by_sid(sid) {
            if recycle {
                crate::shadows::recycle_session(sid);
            }
            self.close_slot(wi, idx);
        } else if let Some(i) = self.shadow_index(sid) {
            if recycle {
                crate::shadows::recycle_session(sid);
            }
            let pane = self.take_shadow(i);
            if !pane.is_l3() {
                self.pane_control.shadows.closed.note(sid);
            }
            self.retire_pane_session(sid, pane.is_l3());
            self.shadows_changed();
            lx_event!("SHADOW_RETIRED", "closed a shadow pane", session = sid, recycled = recycle as u32);
        } else {
            return false;
        }
        true
    }

    /// The slot of window `wi` row `row` stands for, if it is one.
    pub(crate) fn row_slot(&self, wi: usize, row: usize) -> Option<usize> {
        match self.sidebar_row(wi, row)? {
            r if r.tree == SidebarTree::Parent => None,
            Row { pane: RowPane::Slot(idx), .. } => Some(idx),
            _ => None,
        }
    }

    /// The move rows of the menu of pane `sid`, of no slot of the
    /// window: as for a slot of the window it is in; a hidden one can be
    /// shown in any window and has no window of its own to leave.
    pub(crate) fn moves_of(&self, sid: u64) -> Vec<marspot::ui::components::MenuItem> {
        use marspot::ui::components::MenuItem;
        match self.find_window_pane_by_sid(sid) {
            Some((wj, _)) => crate::context_menu::menus::move_items(self.windows.len(), wj, win!(self, wj).panes.len()),
            None => {
                let to = (0..self.windows.len())
                    .map(|ti| MenuItem::entry(&strings::text(Arg::MoveToWindow(ti + 1)), MOVE_TO_WINDOW_TAG_BASE + ti as u32));
                let new = MenuItem::entry(t(Msg::MenuMoveToNewWindow), MOVE_TO_NEW_WINDOW_TAG).disabled();
                to.chain(std::iter::once(new)).collect()
            }
        }
    }

    /// A move row picked on the menu of pane `sid`: `to` a window, or
    /// to a new one (`None`).
    pub(crate) fn move_other(&mut self, sid: u64, to: Option<usize>) {
        match (self.find_window_pane_by_sid(sid), to) {
            (Some((wj, idx)), Some(to)) => self.move_pane_to_window(wj, idx, to),
            (Some((wj, idx)), None) => self.move_pane_to_new_window(wj, idx),
            (None, Some(to)) if to < self.windows.len() => {
                self.show_here(to, sid);
                if to != self.key_window {
                    self.raise_window(to);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
#[path = "sidebar_press_tests.rs"]
mod tests;

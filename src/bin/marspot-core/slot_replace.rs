//! "Replace with…": a slot shows a hidden pane, or a new shell, in
//! place of the pane in it.
//!
//! The pane that leaves goes on running in no window, hollow in the
//! sidebar, or, when the pane shown was in a slot not drawn, takes that
//! slot: the two change places.  Whose child either pane is does not
//! change: nothing is asked of either session.  The keyboard follows the slot to the pane
//! now in it.  A pane's menu and its sidebar row's menu offer the same
//! rows and pick through the same command: a row of a slot of the
//! window replaces that slot, any other row the slot in use.

use marspot::ui::components::MenuItem;

use super::*;

/// The row that opens the panes to show; never picked.
pub(crate) const REPLACE_ROW_TAG: u32 = 0x5300_0000;
/// A new top-level shell in the directory of the pane it replaces.
pub(crate) const NEW_SHELL_TAG: u32 = 0x5300_0001;
/// A hidden pane: this plus its place in the menu's list of them.
pub(crate) const REPLACE_TAG_BASE: u32 = 0x5400_0000;

/// Hidden panes a menu lists at most.
const LISTED_CAP: usize = crate::draw::sidebar_tree::ROW_CAP;

/// What a slot shows instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Replace {
    Hidden(u64),
    NewShell,
}

impl CoreApp {
    /// The "Replace with…" row: a new shell first, then, past a
    /// divider, every pane not shown, a child with its parent's code.
    /// The panes listed are kept for the pick.
    pub(crate) fn replace_row(&mut self) -> MenuItem {
        let hidden = self.not_shown(LISTED_CAP);
        let names = self.pane_names();
        let mut rows = vec![MenuItem::entry(t(Msg::MenuNewShell), NEW_SHELL_TAG)];
        if !hidden.is_empty() {
            rows.push(MenuItem::divider());
        }
        for (i, sid) in hidden.iter().enumerate() {
            let mut label = self.coded_name(*sid, &names);
            let parent = self.pane_control.shadows.family.parents.get(sid).copied();
            if let Some(code) = parent.and_then(marspot_term::pane_code::known) {
                label.push_str(&strings::text(Arg::ChildOf(code)));
            }
            rows.push(MenuItem::entry(&label, REPLACE_TAG_BASE + i as u32));
        }
        self.pane_control.shadows.menu_hidden = hidden;
        rows.extend(self.web_rows());
        MenuItem::entry(t(Msg::MenuReplaceWith), REPLACE_ROW_TAG).with_submenu(rows)
    }

    /// A row of "Replace with…"; false when `tag` is none.
    pub(crate) fn pick_replace(&mut self, wi: usize, region: ContextRegion, tag: u32) -> bool {
        let with = match tag {
            NEW_SHELL_TAG => Some(Replace::NewShell),
            t if (REPLACE_TAG_BASE..REPLACE_TAG_BASE + LISTED_CAP as u32).contains(&t) => {
                self.pane_control.shadows.menu_hidden.get((t - REPLACE_TAG_BASE) as usize).map(|s| Replace::Hidden(*s))
            }
            REPLACE_ROW_TAG => None,
            _ => return false,
        };
        win!(self, wi).context_menu = None;
        win!(self, wi).needs_render = true;
        let idx = match region {
            ContextRegion::Pane(idx) | ContextRegion::SidebarSlot(idx) => Some(idx),
            // the row of a pane of no slot here: the slot in use
            ContextRegion::SidebarPane(_) => Some(win!(self, wi).focused_idx),
            _ => None,
        };
        if let (Some(with), Some(idx)) = (with, idx) {
            self.replace_slot(wi, idx, with);
        }
        true
    }

    /// Slot `idx` of window `wi` shows `with` from now on, and has the
    /// keyboard; the pane that was in it goes on in no window, or in
    /// the slot `with` was in.
    pub(crate) fn replace_slot(&mut self, wi: usize, idx: usize, with: Replace) {
        if idx >= win!(self, wi).panes.len() {
            return;
        }
        if let Replace::Hidden(sid) = with
            && let Some(other) = self.slot_not_shown(sid)
        {
            self.unview(wi, idx);
            self.unview(other.0, other.1);
            let old_sid = win!(self, wi).panes[idx].shelld_session_id();
            self.swap_slots((wi, idx), other);
            win!(self, wi).focused_idx = idx;
            self.save_session_state();
            lx_event!("PANE_SLOTS_SWAPPED", "a slot shows a pane that was not shown; the one it showed takes that pane's slot",
                window_id = win!(self, wi).window_id, slot = idx as u32, shown = sid, other_window = win!(self, other.0).window_id,
                other_slot = other.1 as u32, hidden = old_sid.unwrap_or(0));
            return;
        }
        // the new shell is started as a pane of the window before it
        // takes the slot, so a full window cannot start one
        if with == Replace::NewShell && self.window_full(wi) {
            return;
        }
        self.unview(wi, idx);
        let new = match with {
            Replace::Hidden(sid) => {
                let Some(i) = self.shadow_index(sid) else { return };
                let mut pane = self.take_shadow(i);
                unwatched::set(sid, false);
                // a hidden pane's frames were not read while it was one
                pane.pump();
                pane
            }
            Replace::NewShell => {
                let cwd = win!(self, wi).panes[idx].shelld_session_id().map(crate::spawn::live_cwd_of).unwrap_or_default();
                if self.spawn_session_at(wi, cwd).is_none() {
                    return;
                }
                let Some(pane) = win!(self, wi).panes.pop() else { return };
                pane
            }
        };
        let new_sid = new.shelld_session_id();
        let old = std::mem::replace(&mut win!(self, wi).panes[idx], new);
        let old_sid = old.shelld_session_id();
        // a placeholder is space, not a pane: it is not kept
        if let Some(sid) = old_sid.filter(|_| !old.is_dormant()) {
            unwatched::set(sid, true);
            self.shadows.push(old);
        }
        let w = &mut win!(self, wi);
        w.focused_idx = idx;
        if w.selection.is_some_and(|s| s.session_idx == idx) {
            w.selection = None;
            w.selection_dragging = false;
        }
        self.rebuild_layout(wi);
        self.shadows_changed();
        self.save_session_state();
        lx_event!("PANE_REPLACED", "a slot shows another pane; the one it showed runs on in no window",
            window_id = win!(self, wi).window_id, slot = idx as u32, shown = new_sid.unwrap_or(0),
            hidden = old_sid.unwrap_or(0), new_shell = (with == Replace::NewShell) as u32);
    }
}

#[cfg(test)]
#[path = "slot_replace_tests.rs"]
mod tests;

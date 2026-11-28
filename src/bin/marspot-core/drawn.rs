//! Whether a pane is shown: its slot is drawn in a window the user can
//! see.  A window may hold more panes than its grid has cells (a grid
//! made smaller, a pane placed into a full grid); the panes past the
//! cells are listed in its sidebar and drawn nowhere, as hidden as a
//! pane in no window.  The sidebar's dot, "Replace with…" and a press
//! on a row all ask this one question.

use super::*;

impl CoreApp {
    /// Whether slot `idx` of `w` is drawn: the layout gives it a cell
    /// of some size.
    pub(crate) fn slot_drawn(w: &WindowState, idx: usize) -> bool {
        w.layout.cells.get(idx).is_some_and(|c| c.w >= 1.0 && c.h >= 1.0)
    }

    /// Whether `sid` is drawn in a slot of `w`, its own or viewed there.
    fn drawn_in(w: &WindowState, sid: u64) -> bool {
        let holds = |p: &Pane| p.shelld_session_id() == Some(sid) || p.peeked() == Some(sid);
        w.panes.iter().enumerate().any(|(i, p)| holds(p) && Self::slot_drawn(w, i))
    }

    /// Whether `sid` is shown: drawn in a window that is not minimized.
    /// What the sidebar's solid dot means.
    pub(crate) fn on_visible_screen(&self, sid: u64) -> bool {
        let seen = |w: &&WindowState| !self.pane_control.shadows.minimized.contains(&w.window_id);
        self.windows.iter().filter(seen).any(|w| Self::drawn_in(w, sid))
    }

    /// The panes "Replace with…" offers, at most `cap`: those in a slot
    /// of a window that is not shown, then the panes in no window.
    pub(crate) fn not_shown(&self, cap: usize) -> Vec<u64> {
        let in_windows = self.windows.iter().flat_map(|w| w.panes.iter().filter(|p| !p.is_dormant()));
        in_windows
            .chain(&self.shadows)
            .filter_map(Pane::shelld_session_id)
            .filter(|s| !self.on_visible_screen(*s))
            .take(cap)
            .collect()
    }

    /// The slot holding `sid` when it is in a window and not shown.
    pub(crate) fn slot_not_shown(&self, sid: u64) -> Option<(usize, usize)> {
        self.find_window_pane_by_sid(sid).filter(|_| !self.on_visible_screen(sid))
    }

    /// Slot `idx` of window `wj`, which is not drawn, shown in window
    /// `wi` beside the pane in use instead, as a hidden pane is.
    pub(crate) fn show_undrawn_here(&mut self, wi: usize, wj: usize, idx: usize) {
        if Self::slot_drawn(&win!(self, wj), idx) || idx >= win!(self, wj).panes.len() || (wj != wi && self.window_full(wi)) {
            return;
        }
        // past the cells, so the window keeps every pane it draws
        let pane = win!(self, wj).panes.remove(idx);
        let w = &mut win!(self, wj);
        if w.focused_idx > idx {
            w.focused_idx -= 1;
        } else if w.focused_idx == idx {
            w.focused_idx = idx.min(w.panes.len().saturating_sub(1));
        }
        if w.selection.is_some_and(|s| s.session_idx >= idx) {
            w.selection = None;
            w.selection_dragging = false;
        }
        let sid = pane.shelld_session_id().unwrap_or(0);
        let at = self.insert_beside_focus(wi, pane);
        if wj != wi {
            self.rebuild_layout(wj);
        }
        self.save_session_state();
        lx_event!("PANE_UNDRAWN_SHOWN", "a pane in a slot past its window's cells was given a cell", session = sid,
            from_window = win!(self, wj).window_id, window_id = win!(self, wi).window_id, slot = at as u32);
    }

    /// `pane` into window `wi` beside the pane in use, and focused.
    pub(crate) fn insert_beside_focus(&mut self, wi: usize, pane: Pane) -> usize {
        let at = self.insert_drawn(wi, win!(self, wi).focused_idx + 1, pane);
        win!(self, wi).focused_idx = at;
        at
    }

    /// `pane` into window `wi` at slot `at`, or last; the grid grows a
    /// column, else a row, when it has no cell for one more, so a pane
    /// put among the drawn ones is drawn.
    pub(crate) fn insert_drawn(&mut self, wi: usize, at: usize, pane: Pane) -> usize {
        let w = &mut win!(self, wi);
        let at = at.min(w.panes.len());
        if w.panes.len() + 1 > w.grid_cols * w.grid_rows {
            if w.grid_cols < GRID_MAX {
                w.grid_cols += 1;
            } else if w.grid_rows < GRID_MAX {
                w.grid_rows += 1;
            }
        }
        w.panes.insert(at, pane);
        if w.focused_idx >= at {
            w.focused_idx += 1;
        }
        if w.selection.is_some_and(|s| s.session_idx >= at) {
            w.selection = None;
            w.selection_dragging = false;
        }
        self.rebuild_layout(wi);
        at
    }

    /// Slot `idx` of window `wi` and slot `j` of window `wj` change
    /// panes; neither leaves the windows.
    pub(crate) fn swap_slots(&mut self, (wi, idx): (usize, usize), (wj, j): (usize, usize)) {
        if wi == wj {
            win!(self, wi).panes.swap(idx, j);
        } else {
            let (lo, hi) = (wi.min(wj), wi.max(wj));
            let (left, right) = self.windows.split_at_mut(hi);
            let (a, b) = (&mut left[lo], &mut right[0]);
            let (ia, ib) = if wi < wj { (idx, j) } else { (j, idx) };
            std::mem::swap(&mut a.panes[ia], &mut b.panes[ib]);
        }
        for (w, slot) in [(wi, idx), (wj, j)] {
            let w = &mut win!(self, w);
            if w.selection.is_some_and(|s| s.session_idx == slot) {
                w.selection = None;
                w.selection_dragging = false;
            }
        }
        self.rebuild_layout(wi);
        if wj != wi {
            self.rebuild_layout(wj);
        }
    }
}

#[cfg(test)]
#[path = "drawn_tests.rs"]
mod tests;

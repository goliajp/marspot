//! What a slot of a window shows: the pane in it, or the shadow pane
//! viewed in its place.
//!
//! What the user sees and touches asks for the pane that is shown: the
//! terminal drawn, keys, paste, the pointer, the wheel, a selection,
//! search, links.  What is about the slot asks for the pane in it
//! (`WindowState::focused_pane_mut`, `panes[idx]`): its row in the
//! sidebar, its badge and dim, its place in the saved layout, moving
//! and closing it.

use marspot::pane::Peek;

use super::*;

/// The pane shown in `host`'s place.
pub(super) fn shown_of<'a>(host: &'a Pane, shadows: &'a [Pane]) -> &'a Pane {
    match &host.peek {
        None => host,
        Some(Peek::Ended { last, .. }) => last,
        Some(Peek::Live(sid)) => shadows.iter().find(|p| p.shelld_session_id() == Some(*sid)).unwrap_or(host),
    }
}

fn shown_of_mut<'a>(host: &'a mut Pane, shadows: &'a mut [Pane]) -> &'a mut Pane {
    if let Some(sid) = host.peeked() {
        return shadows.iter_mut().find(|p| p.shelld_session_id() == Some(sid)).unwrap_or(host);
    }
    if !matches!(host.peek, Some(Peek::Ended { .. })) {
        return host;
    }
    match &mut host.peek {
        Some(Peek::Ended { last, .. }) => last,
        _ => unreachable!(),
    }
}

impl CoreApp {
    /// What slot `idx` of window `wi` shows.
    pub(super) fn shown_pane(&self, wi: usize, idx: usize) -> Option<&Pane> {
        win!(self, wi).panes.get(idx).map(|p| shown_of(p, &self.shadows))
    }

    pub(super) fn shown_pane_mut(&mut self, wi: usize, idx: usize) -> Option<&mut Pane> {
        win!(self, wi).panes.get_mut(idx).map(|p| shown_of_mut(p, &mut self.shadows))
    }

    /// As `shown_pane`, for a slot that is there.
    pub(super) fn shown(&self, wi: usize, idx: usize) -> &Pane {
        shown_of(&win!(self, wi).panes[idx], &self.shadows)
    }

    pub(super) fn shown_mut(&mut self, wi: usize, idx: usize) -> &mut Pane {
        shown_of_mut(&mut win!(self, wi).panes[idx], &mut self.shadows)
    }

    /// What the focused slot shows.
    pub(super) fn focused_shown(&self, wi: usize) -> Option<&Pane> {
        self.shown_pane(wi, win!(self, wi).focused_idx)
    }

    pub(super) fn focused_shown_mut(&mut self, wi: usize) -> Option<&mut Pane> {
        self.shown_pane_mut(wi, win!(self, wi).focused_idx)
    }

    /// The session the focused slot's keys go to.  A view that has
    /// ended takes none, and the slot answers for itself.
    pub(super) fn focused_session(&self, wi: usize) -> Option<u64> {
        let w = self.windows.get(wi)?;
        let host = w.panes.get(w.focused_idx)?;
        host.peeked().or_else(|| host.shelld_session_id())
    }

    /// The window and slot `sid` is on the screen in: its own, or the
    /// one it is viewed in.
    pub(super) fn slot_showing(&self, sid: u64) -> Option<(usize, usize)> {
        self.find_window_pane_by_sid(sid).or_else(|| self.viewer_of(sid))
    }
}

//! A view waiting for the helper's frame of the host's size.

use marspot::shell_proto::USER_TYPING_TEXT;

use super::super::held_keys::HeldKeys;
use super::*;

/// How long the helper has to publish a frame of the host's size.
pub(super) const RESIZED_WITHIN: Duration = Duration::from_millis(300);

/// A view waiting for the helper's frame of the host's size.
pub(super) struct Pending {
    pub(super) host: u64,
    pub(super) sid: u64,
    req_id: u32,
    /// Shown even if the user types into the slot meanwhile.
    force: bool,
    deadline: Instant,
}

#[derive(Default)]
pub(crate) struct Peeks {
    /// At most one per host.
    pub(super) pending: Vec<Pending>,
    pub(crate) held: HeldKeys,
}

impl Peeks {
    pub(crate) fn wait(&self) -> Option<Duration> {
        let now = Instant::now();
        self.pending.iter().map(|p| p.deadline.saturating_duration_since(now)).min()
    }
}

impl CoreApp {
    /// Whether it is shown already; else it will be, and `req_id` is
    /// answered then.
    pub(crate) fn start_view(&mut self, host: u64, sid: u64, req_id: u32, force: bool) -> Result<bool, Refused> {
        let other = |why| (DoneCode::Other, why);
        let (wi, idx) = self.find_window_pane_by_sid(host).ok_or(other("the parent pane is in no window"))?;
        let cell =
            win!(self, wi).layout.cells.get(idx).copied().ok_or(other("the parent pane is not in its window's grid"))?;
        if self.pane_control.shadows.family.root(sid) != host {
            return Err(other("the pane is not below that parent"));
        }
        let i = self.shadow_index(sid).ok_or((DoneCode::NoSuchSession, ""))?;
        if !self.shadows[i].is_l3() || self.shadows[i].is_exited() {
            return Err((DoneCode::NotAttached, ""));
        }
        self.drop_pending(host, "another view was asked for");
        if win!(self, wi).panes[idx].peeked() == Some(sid) {
            return Ok(true);
        }
        // before its frame is read: one published in between wakes the loop
        unwatched::set(sid, false);
        let pane = &mut self.shadows[i];
        pane.resize(cell.cols, cell.rows);
        pane.pump();
        if has_size(pane, &cell) {
            self.show(wi, idx, sid);
            return Ok(true);
        }
        let deadline = Instant::now() + RESIZED_WITHIN;
        self.pane_control.shadows.peeks.pending.push(Pending { host, sid, req_id, force, deadline });
        Ok(false)
    }

    /// Once per pump: read the helpers about to be shown, and show
    /// those whose frame has the size, or whose time is up.
    pub(crate) fn advance_views(&mut self, now: Instant) {
        let mut at = 0;
        while let Some(p) = self.pane_control.shadows.peeks.pending.get(at) {
            let (host, sid, late) = (p.host, p.sid, p.deadline <= now);
            let slot = self.find_window_pane_by_sid(host);
            let cell = slot.and_then(|(wi, idx)| win!(self, wi).layout.cells.get(idx).copied());
            let (Some((wi, idx)), Some(cell), Some(i)) = (slot, cell, self.shadow_index(sid)) else {
                self.drop_pending(host, "the parent pane left its window's grid");
                continue;
            };
            let pane = &mut self.shadows[i];
            pane.resize(cell.cols, cell.rows);
            pane.pump();
            if !late && !has_size(pane, &cell) {
                at += 1;
                continue;
            }
            if !has_size(pane, &cell) {
                lx_warn!("core.peek.size_late", "shown before its frame had the host's size", session = sid, host = host);
            }
            let p = self.pane_control.shadows.peeks.pending.remove(at);
            self.show(wi, idx, sid);
            self.answer(MsgType::PanePlace, p.req_id, sid, DoneCode::Ok, "");
        }
    }

    /// Give up the view `host` waits for.
    pub(crate) fn drop_pending(&mut self, host: u64, why: &str) {
        let pending = &mut self.pane_control.shadows.peeks.pending;
        let Some(at) = pending.iter().position(|p| p.host == host) else { return };
        let p = pending.remove(at);
        if self.shadow_index(p.sid).is_some() {
            unwatched::set(p.sid, true);
        }
        self.answer(MsgType::PanePlace, p.req_id, p.sid, DoneCode::Other, why);
    }

    /// The user typed into window `wi`'s focused slot: what they type
    /// next is not given to another session on anyone else's word.
    pub(crate) fn typed_before_view(&mut self, wi: usize) {
        let pending = &self.pane_control.shadows.peeks.pending;
        if pending.is_empty() {
            return;
        }
        let w = &win!(self, wi);
        let host = w.panes.get(w.focused_idx).and_then(Pane::shelld_session_id);
        if let Some(host) = host.filter(|h| pending.iter().any(|p| p.host == *h && !p.force)) {
            self.drop_pending(host, USER_TYPING_TEXT);
        }
    }
}

pub(crate) fn has_size(pane: &Pane, cell: &marspot::layout::CellRect) -> bool {
    let g = pane.session().grid();
    (g.cols(), g.rows()) == (cell.cols, cell.rows)
}

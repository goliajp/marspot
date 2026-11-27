//! A new pane is answered to L1 once its session has said it reads the
//! frames that type into it and watch it.  The socket's handshake comes
//! first, and a request sent on the answer got to the core before the
//! session's word did, to be refused as for an older build.
//!
//! The session's word can also come before the pane: the core reads a
//! new connection from the moment it is made, and the worker that made
//! it reports the landed spawn (or the restored window) afterwards.
//! What a session says about itself before its pane is there is kept
//! and handed to the pane once it is.

use std::collections::VecDeque;

use super::*;

/// How long a session is given to say it; one of an older build never
/// does, and the pane is there all the same.
const SAY_WITHIN: Duration = Duration::from_secs(1);

/// A spawn that landed, its request not answered yet.
struct Unsaid {
    sid: u64,
    req_id: u32,
    deadline: Instant,
}

#[derive(Default)]
pub(super) struct SpawnsUnsaid(Vec<Unsaid>);

/// Three frames per connection from every session the core may hold
/// (the process cap is 64), with room to spare; past it the oldest go.
const EARLY_CAP: usize = 256;

/// What sessions said before their panes landed, oldest first.
#[derive(Default)]
pub(super) struct EarlySays(VecDeque<(u64, Frame)>);

impl EarlySays {
    fn keep(&mut self, sid: u64, frame: Frame) {
        if self.0.len() >= EARLY_CAP
            && let Some((gone, f)) = self.0.pop_front()
        {
            lx_warn!(
                "core.early_says.dropped",
                "a session's word before its pane landed was dropped at the cap",
                session_id = gone,
                msg_type = f.msg_type as u32
            );
        }
        self.0.push_back((sid, frame));
    }

    /// The session's pane will not land (closed, failed, its window
    /// gone): what it said is dropped with it.
    pub(super) fn forget(&mut self, sid: u64) {
        self.0.retain(|(s, _)| *s != sid);
    }
}

impl SpawnsUnsaid {
    pub(super) fn wait(&self) -> Option<Duration> {
        let now = Instant::now();
        self.0.iter().map(|u| u.deadline.saturating_duration_since(now)).min()
    }
}

impl CoreApp {
    pub(super) fn answer_spawn_when_said(&mut self, req_id: u32, sid: u64) {
        let deadline = Instant::now() + SAY_WITHIN;
        self.pane_control.shadows.unsaid.0.push(Unsaid { sid, req_id, deadline });
    }

    /// A frame a session sent about itself.
    pub(super) fn session_says(&mut self, sid: u64, frame: &Frame) {
        if frame.msg_type == MsgType::PromptJumped {
            return self.prompt_jumped(sid, &frame.payload);
        }
        match self.pane_by_sid_mut(sid) {
            Some(pane) => pane.session_mut().l3_session_says(frame),
            None => self.pane_control.shadows.early.keep(sid, frame.clone()),
        }
        if frame.msg_type == MsgType::SessionReadsPaneControl {
            self.answer_unsaid(|u| u.sid == sid);
        }
    }

    /// Panes landed: each is told what its session said before, in the
    /// order it was said.  What is for a pane still to come is kept.
    pub(super) fn replay_early_says(&mut self) {
        for (sid, frame) in std::mem::take(&mut self.pane_control.shadows.early.0) {
            self.session_says(sid, &frame);
        }
    }

    /// A restored window's panes are in place: they hear what their
    /// sessions said meanwhile, then any not on the staged build is
    /// asked to move.  Returns how many were asked.
    pub(super) fn restored_panes_landed(&mut self, wi: usize) -> usize {
        self.replay_early_says();
        marspot::session_signal::ask_all_to_update(win!(self, wi).panes.iter_mut())
    }

    /// The session moved its view to a prompt: the pane follows and
    /// marks the row on top for a moment.
    fn prompt_jumped(&mut self, sid: u64, payload: &[u8]) {
        let Ok(said) = marspot::shell_proto::decode_prompt_jumped(payload) else { return };
        let Some(pane) = self.pane_by_sid_mut(sid) else { return };
        pane.prompt_jumped(said.view_offset, Instant::now());
        if let Some((wi, _)) = self.slot_showing(sid) {
            win!(self, wi).needs_render = true;
        }
    }

    /// Unmark the rows prompt jumps marked whose time is up.
    pub(super) fn expire_prompt_flashes(&mut self, now: Instant) {
        for w in self.windows.iter_mut() {
            for p in w.panes.iter_mut() {
                w.needs_render |= p.expire_prompt_flash(now);
            }
        }
        if self.shadows.iter_mut().fold(false, |any, p| p.expire_prompt_flash(now) | any) {
            self.windows.iter_mut().for_each(|w| w.needs_render = true);
        }
    }

    pub(super) fn expire_unsaid(&mut self, now: Instant) {
        self.answer_unsaid(|u| u.deadline <= now);
    }

    fn answer_unsaid(&mut self, over: impl Fn(&Unsaid) -> bool) {
        while let Some(i) = self.pane_control.shadows.unsaid.0.iter().position(&over) {
            let u = self.pane_control.shadows.unsaid.0.remove(i);
            self.spawned(u.req_id, u.sid, "");
        }
    }
}

#[cfg(test)]
#[path = "spawn_said_tests.rs"]
mod tests;

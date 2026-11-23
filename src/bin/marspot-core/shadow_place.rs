//! Moving a pane between a window and the shadows (`PanePlace`).
//!
//! The session is told first (`SessionShadow`), and the pane moves only
//! once the session has answered: a pane moved into a window whose
//! session still took itself for a shadow would end with its old
//! parent.  The loop does not wait for that answer; it sleeps toward
//! the earliest deadline, and a session silent for 3 s leaves the pane
//! where it was and L1 is told nothing is known (`UNANSWERED`).  An
//! answer that comes after that is undone with a second `SessionShadow`.
//!
//! The core's own `SessionShadow`s carry request ids from a range L1
//! never reaches; their answers stop here and never go on to L1.

use super::pane_control::Route;
use super::*;
use marspot::pane::ControlReach;
use marspot::shell_proto::{
    DoneCode, PanePlace, PlaceWhere, SessionShadow, decode_pane_done, encode_session_shadow,
};
use std::collections::VecDeque;

/// How long a session has to answer a `SessionShadow`.
const PLACE_WITHIN: Duration = Duration::from_secs(3);
/// The core's own request ids: `[L2_REQ_BASE, L2_REQ_END)`.
const L2_REQ_BASE: u32 = 0xF000_0000;
const L2_REQ_END: u32 = 0xFFFF_0000;
/// Moves in flight at once; L1 rate-limits far below this.
const PLACING_CAP: usize = 64;
/// Moves that timed out, kept to undo a late answer.
const EXPIRED_CAP: usize = 16;
/// The text of a `PaneDone` for a move the session did not answer in
/// time; L1 reads it as `unknown`.
pub(super) const UNANSWERED: &str = "unanswered";
const LAST_PANE_STAYS: &str = "the last pane in the windows stays there";

#[derive(Clone, Copy, Debug)]
pub(super) struct Placing {
    l2_req: u32,
    l1_req: u32,
    sid: u64,
    to: PlaceWhere,
    /// The parent the session had before it was asked (0: none).
    was: u64,
    deadline: Instant,
}

#[derive(Default)]
pub(super) struct Places {
    pub(super) placing: Vec<Placing>,
    pub(super) expired: VecDeque<Placing>,
    next: u32,
}

impl Places {
    fn next_req(&mut self) -> u32 {
        self.next = self.next.wrapping_add(1) % (L2_REQ_END - L2_REQ_BASE);
        L2_REQ_BASE + self.next
    }

    pub(super) fn wait(&self) -> Option<Duration> {
        let now = Instant::now();
        self.placing
            .iter()
            .map(|p| p.deadline.saturating_duration_since(now))
            .min()
    }
}

/// A `SessionShadow` the core sends on its own account, whose answer
/// nobody waits for.
pub(super) fn session_shadow_frame(req_id: u32, parent: u64) -> Frame {
    Frame::new(
        MsgType::SessionShadow,
        encode_session_shadow(&SessionShadow { req_id, parent_sid: parent, idle_s: 0 }),
    )
}

/// A request id of the core's own that nobody waits for.
pub(super) const L2_REQ_UNTRACKED: u32 = L2_REQ_BASE;

impl CoreApp {
    /// `PanePlace`: 0 into the shadows under `host_sid`, 1 into a
    /// window (the one holding the pane's parent, else the key window).
    /// Peeking is not done by this core.
    pub(super) fn place_pane(&mut self, m: PanePlace) -> Route {
        let in_window = self.find_window_pane_by_sid(m.sid).is_some();
        let in_shadows = self.shadow_index(m.sid).is_some();
        let answer = |app: &mut Self, code: DoneCode, text: &str| app.answer(MsgType::PanePlace, m.req_id, m.sid, code, text);
        match m.to {
            PlaceWhere::Window if in_window => answer(self, DoneCode::Ok, ""),
            PlaceWhere::Shadow if in_shadows => answer(self, DoneCode::Ok, ""),
            PlaceWhere::Window | PlaceWhere::Shadow if !in_window && !in_shadows => {
                answer(self, DoneCode::NoSuchSession, "")
            }
            PlaceWhere::Window if self.windows.is_empty() => answer(self, DoneCode::Other, "no window is open"),
            PlaceWhere::Window => self.start_place(m, 0, m.host_sid),
            PlaceWhere::Shadow if m.host_sid == 0 || m.host_sid == m.sid => {
                answer(self, DoneCode::Other, "a shadow pane needs a parent")
            }
            PlaceWhere::Shadow if !self.panes_mut().any(|p| p.shelld_session_id() == Some(m.host_sid)) => {
                answer(self, DoneCode::Other, "no such parent pane")
            }
            PlaceWhere::Shadow if self.last_drawn_pane(m.sid) => {
                answer(self, DoneCode::Other, LAST_PANE_STAYS)
            }
            PlaceWhere::Shadow => self.start_place(m, m.host_sid, 0),
            _ => answer(self, DoneCode::Other, "not_supported"),
        }
    }

    /// Moving `sid` out would leave no live pane in any window.
    fn last_drawn_pane(&self, sid: u64) -> bool {
        !self
            .windows
            .iter()
            .flat_map(|w| w.panes.iter())
            .any(|p| !p.is_dormant() && p.shelld_session_id() != Some(sid))
    }

    fn start_place(&mut self, m: PanePlace, parent: u64, was: u64) -> Route {
        let reconnecting = self.reconnecting.contains(&m.sid);
        if self.pane_control.shadows.places.placing.len() >= PLACING_CAP {
            return self.answer(MsgType::PanePlace, m.req_id, m.sid, DoneCode::Other, "too many moves at once");
        }
        let l2_req = self.pane_control.shadows.places.next_req();
        let reach = match self.pane_by_sid_mut(m.sid) {
            Some(_) if reconnecting => ControlReach::NotAttached,
            None => ControlReach::NotAttached,
            Some(p) => match p.session().pane_control_reach() {
                ControlReach::Reads if !p.session().send_pane_control(session_shadow_frame(l2_req, parent)) => {
                    return self.answer(MsgType::PanePlace, m.req_id, m.sid, DoneCode::Other, "session queue full");
                }
                r => r,
            },
        };
        let code = match reach {
            ControlReach::Reads => None,
            ControlReach::OldSession => Some(DoneCode::OldSession),
            _ => Some(DoneCode::NotAttached),
        };
        if let Some(code) = code {
            return self.answer(MsgType::PanePlace, m.req_id, m.sid, code, "");
        }
        let deadline = Instant::now() + PLACE_WITHIN;
        let p = Placing { l2_req, l1_req: m.req_id, sid: m.sid, to: m.to, was, deadline };
        self.pane_control.shadows.places.placing.push(p);
        lx_event!(
            "PANE_PLACE_ASKED",
            "asked a session to change whose shadow it is before moving its pane",
            session = m.sid,
            to = format!("{:?}", m.to),
            parent = parent
        );
        Route::Forwarded
    }

    /// A `PaneDone` from session `sid`: one answering the core's own
    /// `SessionShadow` is consumed here (`true`).
    pub(super) fn session_done(&mut self, sid: u64, f: &Frame) -> bool {
        let Ok(done) = decode_pane_done(&f.payload) else { return false };
        if !(L2_REQ_BASE..L2_REQ_END).contains(&done.req_id) {
            return false;
        }
        let places = &mut self.pane_control.shadows.places;
        if let Some(i) = places.placing.iter().position(|p| p.l2_req == done.req_id && p.sid == sid) {
            let p = places.placing.remove(i);
            self.finish_place(p, done.code, &done.text);
        } else if let Some(i) = places.expired.iter().position(|p| p.l2_req == done.req_id && p.sid == sid) {
            let p = places.expired.remove(i).unwrap_or_else(|| unreachable!());
            if done.code == DoneCode::Ok {
                // the pane stayed where it was; so does the session
                if let Some(pane) = self.pane_by_sid_mut(sid) {
                    pane.session().send_pane_control(session_shadow_frame(L2_REQ_UNTRACKED, p.was));
                }
                lx_warn!("core.shadow.late_answer_undone", "a session answered a move after its time; undone",
                    session = sid);
            }
        }
        true
    }

    /// Moves whose session did not answer in time stay undone.
    pub(super) fn expire_places(&mut self, now: Instant) {
        let places = &mut self.pane_control.shadows.places;
        let (late, kept): (Vec<_>, Vec<_>) = places.placing.drain(..).partition(|p| p.deadline <= now);
        places.placing = kept;
        for p in late {
            let places = &mut self.pane_control.shadows.places;
            if places.expired.len() >= EXPIRED_CAP {
                places.expired.pop_front();
            }
            places.expired.push_back(p);
            lx_warn!("core.shadow.place_unanswered", "a session did not answer a move; the pane stays",
                session = p.sid);
            self.answer(MsgType::PanePlace, p.l1_req, p.sid, DoneCode::Other, UNANSWERED);
        }
    }

    fn finish_place(&mut self, p: Placing, code: DoneCode, text: &str) {
        if code != DoneCode::Ok {
            self.answer(MsgType::PanePlace, p.l1_req, p.sid, code, text);
            return;
        }
        let moved = match p.to {
            PlaceWhere::Window => self.shadow_index(p.sid).map(|i| {
                let mut pane = self.shadows.remove(i);
                unwatched::set(p.sid, false);
                // a shadow's frames were not read while it was one
                pane.pump();
                let wi = self
                    .find_window_pane_by_sid(p.was)
                    .map(|(wi, _)| wi)
                    .unwrap_or(self.key_window)
                    .min(self.windows.len().saturating_sub(1));
                win!(self, wi).panes.push(pane);
                self.rebuild_layout(wi);
                win!(self, wi).needs_render = true;
            }),
            // the other panes may have gone while the session answered
            _ if self.find_window_pane_by_sid(p.sid).is_some() && self.last_drawn_pane(p.sid) => {
                let sent = self
                    .pane_by_sid_mut(p.sid)
                    .is_some_and(|pane| pane.session().send_pane_control(session_shadow_frame(L2_REQ_UNTRACKED, 0)));
                if !sent {
                    // the session keeps its parent until the next boot puts it back by the layout
                    lx_warn!("core.shadow.unshadow_unsent", "SessionShadow(0) for the pane that stays was not sent",
                        session = p.sid);
                }
                lx_warn!("core.shadow.place_last_pane", "the pane became the last one in the windows while it moved; it stays",
                    session = p.sid);
                self.answer(MsgType::PanePlace, p.l1_req, p.sid, DoneCode::Other, LAST_PANE_STAYS);
                return;
            }
            _ => self.find_window_pane_by_sid(p.sid).map(|(wi, idx)| {
                let pane = self.take_out_of_window(wi, idx);
                unwatched::set(p.sid, true);
                self.shadows.push(pane);
                self.rebuild_layout(wi);
                self.request_close_if_empty(wi);
            }),
        };
        if moved.is_none() {
            self.answer(MsgType::PanePlace, p.l1_req, p.sid, DoneCode::NoSuchSession, "the pane went while it moved");
            return;
        }
        self.save_session_state();
        lx_event!("PANE_PLACED", "moved a pane between a window and the shadows", session = p.sid,
            to = format!("{:?}", p.to));
        self.answer(MsgType::PanePlace, p.l1_req, p.sid, DoneCode::Ok, "");
    }
}

#[cfg(test)]
#[path = "shadow_place_tests.rs"]
mod tests;

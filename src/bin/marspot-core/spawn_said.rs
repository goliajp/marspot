//! A new pane is answered to L1 once its session has said it reads the
//! frames that type into it and watch it.  The socket's handshake comes
//! first, and a request sent on the answer got to the core before the
//! session's word did, to be refused as for an older build.

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
        if let Some(pane) = self.pane_by_sid_mut(sid) {
            pane.session_mut().l3_session_says(frame);
        }
        if frame.msg_type == MsgType::SessionReadsPaneControl {
            self.answer_unsaid(|u| u.sid == sid);
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

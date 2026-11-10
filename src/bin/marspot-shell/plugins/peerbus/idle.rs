//! Idle notices, both ways.  A peer that sends `notify_when_idle` to a
//! Codex inbox is told once, when that session next goes idle or goes
//! away; a `peer_idle_notice` for a subscription the Codex session
//! placed is shown to it in the words Claude Code uses.

use std::time::{Duration, Instant};

use marspot_peer::frame::{self, IdleState};
use marspot_peer::registry::Status;
use marspot_term::{lx_info, lx_warn};

use super::deliver;
use super::worker::{now_ms, Binding, Worker};

/// Subscriptions one inbox holds at once.
pub(super) const MAX_SUBS: usize = 32;
/// How long a subscription placed while the session is idle waits
/// before it is answered.
pub(super) const QUIET_FOR: Duration = Duration::from_secs(2);

impl Worker {
    pub(super) fn subscribe(&mut self, key: u64, from: String, msg_id: String) {
        let Some((sid, b)) = self.bindings.iter_mut().find(|(_, b)| b.key == key) else {
            return self.subscribe_gemini(key, from, msg_id);
        };
        let sid = *sid;
        if b.subs.len() >= MAX_SUBS {
            lx_warn!("peerbus.subscribe.full", "idle subscription refused: too many held", sid = sid);
            return;
        }
        b.subs.retain(|(to, _)| *to != from);
        b.subs.push((from, msg_id));
        // Idle now: look again shortly.  The message a subscription
        // usually comes with may still be on its way (the two travel on
        // separate connections), and the turn it starts ends in a
        // notice of its own.
        if b.record.status == Status::Idle && b.active_turn.is_none() {
            self.idle_checks.push((sid, Instant::now() + QUIET_FOR));
        }
    }

    /// Subscriptions placed while idle, whose wait is over: still idle
    /// and nothing handed over since means no turn is coming.
    pub(super) fn idle_checks_due(&mut self, now: Instant) {
        let (due, later): (Vec<_>, Vec<_>) = self.idle_checks.drain(..).partition(|(_, t)| *t <= now);
        self.idle_checks = later;
        for (sid, at) in due {
            let Some(b) = self.bindings.get(&sid) else {
                self.kimi_quiet(sid);
                continue;
            };
            let since = at - QUIET_FOR;
            let quiet = b.last_delivered.is_none_or(|t| t < since);
            if b.record.status == Status::Idle && b.active_turn.is_none() && quiet {
                self.fire(sid, IdleState::Idle);
            }
        }
    }

    /// Tell every subscriber of `sid`'s session that it is idle.
    pub(super) fn fire(&mut self, sid: u64, state: IdleState) {
        if let Some(b) = self.bindings.get_mut(&sid) {
            send_notices(sid, b, state);
        }
    }

    /// A notice for a subscription this session placed: shown to it the
    /// way Claude Code shows one.  Only from a live registered session,
    /// named by its registry name.
    pub(super) fn notice(&mut self, key: u64, from: &str, state: &str, finished_at: Option<i64>) {
        let codex = self.bindings.iter().find(|(_, b)| b.key == key).map(|(sid, b)| (*sid, b.home.clone(), b.thread.clone()));
        let Some((sid, home, thread)) = codex else {
            return self.notice_gemini(key, from, state, finished_at);
        };
        let Some(text) = notice_text(sid, from, state, finished_at) else { return };
        let (result, via) = self.hand_over(&home, &thread, text, None, marspot_peer::rand::uuid_v4());
        match result {
            Ok(_) => lx_info!("peerbus.notice", "idle notice queued for the codex session", sid = sid, via = via),
            Err(e) => lx_warn!("peerbus.notice_failed", &e.to_string(), sid = sid),
        }
    }
}

/// What the session in pane `sid` is shown for an idle notice from
/// `from`; None for a sender that is not a live registered session, or
/// a state this side does not know.
pub(super) fn notice_text(sid: u64, from: &str, state: &str, finished_at: Option<i64>) -> Option<String> {
    let entries = marspot_peer::peers::live_entries();
    let Some(sender) = entries.iter().find(|e| e.socket.as_deref().map(|s| format!("uds:{s}")).as_deref() == Some(from)) else {
        lx_info!("peerbus.notice.unknown_sender", "dropped an idle notice from an unregistered address", sid = sid);
        return None;
    };
    let label = sender.name.clone().unwrap_or_else(|| from.to_string());
    marspot_peer::notice::idle_text(&label, state, finished_at)
}

/// Tell every subscriber how the session ended up; each subscription
/// is good for one notice.
pub(super) fn send_notices(sid: u64, b: &mut Binding, state: IdleState) {
    let from = format!("uds:{}", b.inbox.path.display());
    let mode = b.class.as_str();
    for (to, orig) in b.subs.drain(..) {
        let line = frame::idle_notice_line(&orig, state, now_ms(), &from, mode, &marspot_peer::rand::uuid_v4());
        deliver::send_line(&b.inbox.relay, &to, &line);
        lx_info!("peerbus.idle_notice", "sent", sid = sid, state = state.as_str());
    }
}

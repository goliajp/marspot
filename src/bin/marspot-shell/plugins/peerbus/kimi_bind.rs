//! Kimi panes as peers.  The record, the inbox and the waiting mail
//! are the ones a Gemini pane gets (`gemini_bind`); two things differ.
//!
//! Kimi's own record says when a turn starts and ends, so a Kimi
//! pane's registry record carries a status, offers the idle notice,
//! and the notice is sent when the session next goes idle.
//!
//! And mail reaches a Kimi session at two moments only: when the user
//! submits a prompt, and when a turn is about to end.  An idle session
//! has neither, so the kimi plugin types one fixed line into an idle
//! pane that has mail (`mail_waiting`); the prompt that makes is what
//! the hook then puts the mail in front of.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use marspot_peer::envelope::Mode;
use marspot_peer::frame::{self, IdleState};
use marspot_peer::registry::{self, Agent, Status};
use marspot_term::{lx_info, lx_warn};

use super::deliver;
use super::gemini_bind::GeminiBinding;
use super::idle::{MAX_SUBS, QUIET_FOR};
use super::worker::{now_ms, write_record, Worker};
use crate::plugins::{gemini, kimi};

static SPOOLS: Mutex<Option<HashMap<u64, PathBuf>>> = Mutex::new(None);

/// Whether mail waits for the Kimi session in pane `sid`.  One `stat`:
/// the hook removes the file when it takes the mail.
pub(crate) fn mail_waiting(sid: u64) -> bool {
    let path = SPOOLS.lock().unwrap().as_ref().and_then(|m| m.get(&sid).cloned());
    path.is_some_and(|p| waiting(&p))
}

fn waiting(spool: &std::path::Path) -> bool {
    std::fs::metadata(spool).is_ok_and(|m| m.len() > 0)
}

impl GeminiBinding {
    /// Tell every subscriber how the session ended up; each
    /// subscription is good for one notice.
    pub(super) fn notify(&mut self, state: IdleState) {
        let from = format!("uds:{}", self.inbox.path.display());
        for (to, orig) in self.subs.drain(..) {
            let line = frame::idle_notice_line(&orig, state, now_ms(), &from, self.class.as_str(), &marspot_peer::rand::uuid_v4());
            deliver::send_line(&self.inbox.relay, &to, &line);
            lx_info!("peerbus.idle_notice", "sent", session = self.record.session_id.clone(), state = state.as_str());
        }
    }
}

impl Worker {
    /// Bring the Kimi bindings in line with the panes, and each
    /// record's status with its session.
    pub(super) fn apply_kimi(&mut self, peers: &kimi::Peers) {
        let shaped = gemini::Peers {
            bound: peers
                .bound
                .iter()
                .map(|p| gemini::Peer { sid: p.sid, gemini: p.kimi, session: p.session.clone(), cwd: p.cwd.clone() })
                .collect(),
            all_known: peers.all_known,
        };
        let class = |p: &gemini::Peer| peers.bound.iter().find(|k| k.sid == p.sid).map_or(Mode::Prompting, |k| k.class);
        self.apply_spooled(Agent::Kimi, &shaped, class);
        for p in &peers.bound {
            let Some(g) = self.gemini.get_mut(&p.sid).filter(|g| g.record.agent == Agent::Kimi) else { continue };
            // a turn shorter than the plugin's tick is never seen
            // running; the number of the last ended turn still moves
            let turn_ended = p.status == Status::Idle && (g.record.status != Status::Idle || p.ended != g.ended);
            g.ended = p.ended;
            if g.record.status != p.status || g.class != p.class {
                g.class = p.class;
                g.record.class = p.class;
                g.record.status = p.status;
                write_record(&registry::default_dirs(), &g.record);
            }
            // with mail waiting the pane is about to be woken for it,
            // and the turn that follows ends in a notice of its own
            if turn_ended && !waiting(&g.spool) {
                g.notify(IdleState::Idle);
            }
        }
        let spools = self.gemini.iter().filter(|(_, g)| g.record.agent == Agent::Kimi).map(|(sid, g)| (*sid, g.spool.clone()));
        *SPOOLS.lock().unwrap() = Some(spools.collect());
    }

    /// `notify_when_idle` sent to a Kimi inbox.
    pub(super) fn subscribe_kimi(&mut self, sid: u64, from: String, msg_id: String) {
        let Some(g) = self.gemini.get_mut(&sid) else { return };
        if g.subs.len() >= MAX_SUBS {
            lx_warn!("peerbus.subscribe.full", "idle subscription refused: too many held", sid = sid);
            return;
        }
        g.subs.retain(|(to, _)| *to != from);
        g.subs.push((from, msg_id));
        // Idle now: look again shortly, as for Codex.  The message a
        // subscription usually comes with may still be on its way.
        if g.record.status == Status::Idle {
            self.idle_checks.push((sid, Instant::now() + QUIET_FOR));
        }
    }

    /// A subscription placed while the Kimi session was idle, whose
    /// wait is over: still idle with no mail waiting means no turn is
    /// coming.
    pub(super) fn kimi_quiet(&mut self, sid: u64) {
        if let Some(g) = self.gemini.get_mut(&sid).filter(|g| g.record.agent == Agent::Kimi)
            && g.record.status == Status::Idle
            && !waiting(&g.spool)
        {
            g.notify(IdleState::Idle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mail_waits_while_the_file_holds_something() {
        let d = std::env::temp_dir().join(format!("pb-kimi-{}", marspot_peer::rand::hex(6)));
        std::fs::create_dir_all(&d).unwrap();
        let spool = marspot_peer::spool::path_for(&d, "session_1");
        assert!(!waiting(&spool), "no file, no mail");
        marspot_peer::spool::append(&spool, "hello", 4).unwrap();
        assert!(waiting(&spool));
        assert_eq!(marspot_peer::spool::take(&spool), vec!["hello".to_string()]);
        assert!(!waiting(&spool), "taken by the hook");
        std::fs::remove_dir_all(&d).unwrap();
    }
}

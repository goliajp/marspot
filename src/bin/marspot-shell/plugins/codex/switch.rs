//! The badge menu's account switch: the same thread, by id, under
//! another profile, and only when peerbus says it may move.
//!
//! The thread lives in the old home's daemon, not in the TUI.  Ending
//! the TUI mid-turn leaves that daemon running the turn while the new
//! home's TUI loads the same thread, so a switch waits for a session
//! that is idle, has no sub-agent at work and no mail queued, and
//! peerbus keeps messages away from the old home while it happens.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marspot::pidtree::ProcId;
use marspot::ui::strings::{t, Msg};

use super::pane::{is_thread_id, profile_switch_op};
use super::{update, CodexPlugin};
use crate::plugins::peerbus::moving::{self, Ask, Held, Verdict};
use crate::plugins::{handoff, pidtree, LogLevel, PluginHost};

/// How long a reason not to switch stays beside the badge.
const HINT_FOR: Duration = Duration::from_secs(6);

/// A switch asked for and not over yet.
pub(super) struct Move {
    codex: ProcId,
    profile: u8,
    dir: PathBuf,
    /// Its op has been submitted.
    started: bool,
}

/// What the switch keeps between ticks.
#[derive(Default)]
pub(super) struct Switches {
    moves: HashMap<u64, Move>,
    hints: HashMap<u64, (Msg, Instant)>,
    /// Panes whose op has ended, told from the op's own thread.
    ended: Arc<Mutex<Vec<u64>>>,
}

/// `codex` or `codex resume <id>`: a command line the resume line can
/// stand in for.
pub(super) fn movable_argv(argv: &[String]) -> bool {
    update::plain_argv(argv) && argv.get(2).is_none_or(|id| is_thread_id(id))
}

fn hint_of(h: Held) -> Msg {
    match h {
        Held::Busy => Msg::BadgeCodexTurnRunning,
        Held::ChildRunning => Msg::BadgeCodexAgentRunning,
        Held::Queued => Msg::BadgeCodexQueued,
        Held::Moving => Msg::BadgeCodexSwitching,
        Held::NotBound | Held::Unknown => Msg::BadgeCodexStateUnknown,
    }
}

impl CodexPlugin {
    /// A pick of profile `profile` (at `dir`) on pane `sid`'s badge.
    pub(super) fn switch_profile(&mut self, host: &dyn PluginHost, sid: u64, profile: u8, dir: PathBuf) {
        let Some(pane) = self.panes.get(&sid).cloned() else {
            host.log(LogLevel::Warn, "cycle.menu_no_codex", &format!("menu pick on shelld_session={sid} with no codex"));
            return;
        };
        if pane.profile == Some(profile) {
            return; // already there; a stale menu is not a request
        }
        if self.switches.moves.contains_key(&sid) {
            return self.hint(host, sid, Msg::BadgeCodexSwitching);
        }
        if !crate::plugins::peerbus::threads().get(&sid).is_some_and(|v| v.codex == pane.codex) {
            // no thread id to resume by, and `--last` can pick another
            // thread of the directory: the conversation is carried over
            host.log(LogLevel::Info, "switch.no_thread", &format!("pane {sid}: thread not known, handing off to P{profile}"));
            return self.hand_off(host, sid, handoff::Agent::Codex, profile);
        }
        if !pane.argv.as_deref().is_some_and(movable_argv) {
            return self.hint(host, sid, Msg::BadgeCodexCustomCommand);
        }
        if !moving::ask(Ask::Begin { sid, codex: pane.codex }) {
            return self.hint(host, sid, Msg::BadgeCodexStateUnknown);
        }
        self.switches.moves.insert(sid, Move { codex: pane.codex, profile, dir, started: false });
    }

    /// Act on what peerbus decided, and forget switches that are over.
    pub(super) fn follow_switches(&mut self, host: &dyn PluginHost) {
        for (sid, codex, v) in moving::verdicts() {
            if !self.switches.moves.get(&sid).is_some_and(|m| m.codex == codex && !m.started) {
                continue;
            }
            match v {
                Verdict::Held(h) => {
                    self.switches.moves.remove(&sid);
                    self.hint(host, sid, hint_of(h));
                }
                Verdict::Go { thread, home } => self.start_switch(host, sid, &thread, &home),
            }
        }
        let ended = std::mem::take(&mut *self.switches.ended.lock().unwrap());
        for sid in ended {
            self.switches.moves.remove(&sid);
        }
        // one still waiting for its verdict, whose codex has gone
        self.switches.moves.retain(|sid, m| m.started || self.panes.get(sid).is_some_and(|p| p.codex == m.codex));
        let now = Instant::now();
        self.switches.hints.retain(|_, (_, until)| *until > now);
    }

    fn start_switch(&mut self, host: &dyn PluginHost, sid: u64, thread: &str, home: &std::path::Path) {
        let (Some(m), Some(pane)) = (self.switches.moves.get_mut(&sid), self.panes.get(&sid)) else { return };
        let codex = m.codex;
        let cwd = pidtree::proc_cwd(codex.pid).map(|c| c.to_string_lossy().into_owned());
        let Some(op) = profile_switch_op(pane, m.profile, &m.dir, thread, cwd) else {
            host.log(LogLevel::Warn, "switch.no_command", &format!("pane {sid}: cannot build the resume line"));
            self.switches.moves.remove(&sid);
            moving::ask(Ask::End { sid, codex });
            return;
        };
        m.started = true;
        let ended = Arc::clone(&self.switches.ended);
        let op = op.on_end(Arc::new(move |o| {
            if !o.is_done() {
                moving::ask(Ask::End { sid, codex });
            }
            ended.lock().unwrap().push(sid);
        }));
        host.log(LogLevel::Info, "switch.start", &format!("pane {sid}: thread {thread} {} → {}", home.display(), m.dir.display()));
        if let Err(e) = host.submit_pty_op(sid, op) {
            host.log(LogLevel::Warn, "cycle.submit_failed", &format!("shelld_session={sid}: {e}"));
            self.switches.moves.remove(&sid);
            moving::ask(Ask::End { sid, codex });
        }
    }

    fn hint(&mut self, host: &dyn PluginHost, sid: u64, why: Msg) {
        host.log(LogLevel::Info, "switch.held", &format!("pane {sid}: {why:?}"));
        self.switches.hints.insert(sid, (why, Instant::now() + HINT_FOR));
    }

    /// The badge, with the reason a switch did not happen beside it.
    pub(super) fn hinted(&self, sid: u64, badge: String) -> String {
        match self.switches.hints.get(&sid) {
            Some((why, until)) if *until > Instant::now() => format!("{badge} {}", t(*why)),
            _ => badge,
        }
    }

    #[cfg(test)]
    pub(super) fn hint_for(&self, sid: u64) -> Option<Msg> {
        self.switches.hints.get(&sid).map(|(m, _)| *m)
    }
}

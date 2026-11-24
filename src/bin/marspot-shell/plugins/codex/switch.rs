//! The badge menu's account switch: the same thread, by id, under
//! another profile, and only when peerbus says it may move.
//!
//! The thread lives in the old home's daemon, not in the TUI.  Ending
//! the TUI mid-turn leaves that daemon running the turn while the new
//! home's TUI loads the same thread, so a switch waits for a session
//! that is idle, has no sub-agent at work and no mail queued, and
//! peerbus keeps messages away from the old home while it happens.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marspot::pane_state::{BusyKind, PaneStatus};
use marspot::pidtree::ProcId;
use marspot::ui::strings::{t, Msg};

use super::pane::{is_thread_id, profile_switch_op, resume_op, PaneCodex, RELEASED};
use super::{update, CodexPlugin};
use crate::plugins::peerbus::moving::{self, Ask, Held, Verdict};
use crate::plugins::{handoff, pidtree, pty_op, LogLevel, PluginHost};

/// How long a reason not to switch stays beside the badge.
const HINT_FOR: Duration = Duration::from_secs(6);
/// A codex at work redraws its spinner many times a second; a terminal
/// this long without output has none running.
const QUIET_FOR: Duration = Duration::from_secs(3);

/// A switch asked for and not over yet.
pub(super) struct Move {
    codex: ProcId,
    profile: u8,
    dir: PathBuf,
    /// Its op has been submitted.
    started: bool,
    /// Where it came from, to go back to when the old home kept the
    /// session: the pane, the home, the thread and its directory.
    back: Option<(PaneCodex, PathBuf, String, Option<String>)>,
}

/// What the switch keeps between ticks.
#[derive(Default)]
pub(super) struct Switches {
    moves: HashMap<u64, Move>,
    hints: HashMap<u64, (Msg, Instant)>,
    /// Panes whose op has ended, told from the op's own thread, and
    /// whether it ended because the old home kept the session.
    ended: Arc<Mutex<Vec<(u64, bool)>>>,
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
        Held::NotBound => Msg::BadgeCodexNotTracked,
        Held::Unknown => Msg::BadgeCodexStateUnknown,
    }
}

/// The pane's terminal has been quiet long enough that no turn is
/// drawing in it.
fn terminal_quiet(host: &dyn PluginHost, sid: u64) -> bool {
    matches!(host.pane_status(sid), Ok(Some(v)) if v.status == PaneStatus::Busy(BusyKind::ForegroundJob) && v.held >= QUIET_FOR)
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
        if !pane.argv.as_deref().is_some_and(movable_argv) {
            return self.hint(host, sid, Msg::BadgeCodexCustomCommand);
        }
        if !crate::plugins::peerbus::threads().get(&sid).is_some_and(|v| v.codex == pane.codex) {
            // no daemon to ask whether a turn is running: the terminal
            // is the only witness
            if !terminal_quiet(host, sid) {
                return self.hint(host, sid, Msg::BadgeCodexTerminalBusy);
            }
            // no thread id to resume by, and `--last` can pick another
            // thread of the directory: the conversation is carried over
            host.log(LogLevel::Info, "switch.no_thread", &format!("pane {sid}: thread not known, handing off to P{profile}"));
            return self.hand_off(host, sid, handoff::Agent::Codex, profile);
        }
        if !moving::ask(Ask::Begin { sid, codex: pane.codex }) {
            return self.hint(host, sid, Msg::BadgeCodexStateUnknown);
        }
        self.switches.moves.insert(sid, Move { codex: pane.codex, profile, dir, started: false, back: None });
    }

    /// Act on what peerbus decided, and forget switches that are over.
    pub(super) fn follow_switches(&mut self, host: &dyn PluginHost) {
        for (sid, codex, v) in moving::verdicts() {
            let pending = self.switches.moves.get(&sid).filter(|m| m.codex == codex).map(|m| m.started);
            match (pending, v) {
                (Some(true), _) | (None, Verdict::Held(_)) => {}
                (None, Verdict::Go { .. }) => {
                    // marked moving for a switch that is gone: unmark it
                    host.log(LogLevel::Info, "switch.dropped", &format!("pane {sid}: a go for a switch no longer asked for"));
                    moving::ask(Ask::End { sid, codex });
                }
                (Some(false), Verdict::Held(h)) => {
                    self.switches.moves.remove(&sid);
                    self.hint(host, sid, hint_of(h));
                }
                (Some(false), Verdict::Go { thread, home, released }) => self.start_switch(host, sid, &thread, &home, released.0),
            }
        }
        let ended = std::mem::take(&mut *self.switches.ended.lock().unwrap());
        for (sid, kept) in ended {
            let Some(m) = self.switches.moves.remove(&sid) else { continue };
            if kept {
                self.switch_back(host, sid, m);
            }
        }
        // one still waiting for its verdict, whose codex has gone
        let gone: Vec<(u64, ProcId)> = self
            .switches
            .moves
            .iter()
            .filter(|(sid, m)| !m.started && !self.panes.get(sid).is_some_and(|p| p.codex == m.codex))
            .map(|(sid, m)| (*sid, m.codex))
            .collect();
        for (sid, codex) in gone {
            self.switches.moves.remove(&sid);
            moving::ask(Ask::End { sid, codex });
        }
        let now = Instant::now();
        self.switches.hints.retain(|_, (_, until)| *until > now);
    }

    fn start_switch(&mut self, host: &dyn PluginHost, sid: u64, thread: &str, home: &Path, released: Arc<pty_op::Job>) {
        let Some(m) = self.switches.moves.get(&sid) else { return };
        let codex = m.codex;
        let Some(pane) = self.panes.get(&sid).filter(|p| p.codex == codex).cloned() else {
            host.log(LogLevel::Info, "switch.no_pane", &format!("pane {sid}: its codex went before the switch began"));
            self.switches.moves.remove(&sid);
            moving::ask(Ask::End { sid, codex });
            return;
        };
        let cwd = pidtree::proc_cwd(codex.pid).map(|c| c.to_string_lossy().into_owned());
        let Some(op) = profile_switch_op(&pane, m.profile, &m.dir, thread, cwd.clone(), released) else {
            host.log(LogLevel::Warn, "switch.no_command", &format!("pane {sid}: cannot build the resume line"));
            self.switches.moves.remove(&sid);
            moving::ask(Ask::End { sid, codex });
            return;
        };
        let to = m.dir.display().to_string();
        let ended = Arc::clone(&self.switches.ended);
        let op = op.on_end(Arc::new(move |o| {
            if !o.is_done() {
                moving::ask(Ask::End { sid, codex });
            }
            let kept = matches!(o, pty_op::OpOutcome::Failed { label, .. } | pty_op::OpOutcome::TimedOut { label, .. } if *label == RELEASED);
            ended.lock().unwrap().push((sid, kept));
        }));
        host.log(LogLevel::Info, "switch.start", &format!("pane {sid}: thread {thread} {} → {to}", home.display()));
        if let Err(e) = host.submit_pty_op(sid, op) {
            host.log(LogLevel::Warn, "cycle.submit_failed", &format!("shelld_session={sid}: {e}"));
            self.switches.moves.remove(&sid);
            moving::ask(Ask::End { sid, codex });
            return;
        }
        if let Some(m) = self.switches.moves.get_mut(&sid) {
            m.started = true;
            m.back = Some((pane, home.to_path_buf(), thread.to_string(), cwd));
        }
    }

    /// The old home did not let go in time: the TUI is gone, so put it
    /// back where the session still is.
    fn switch_back(&mut self, host: &dyn PluginHost, sid: u64, m: Move) {
        self.hint(host, sid, Msg::BadgeCodexStillLoaded);
        let Some((pane, home, thread, cwd)) = m.back else { return };
        let Some(op) = resume_op("codex.switch_back", "↩".into(), &pane, &home, &thread, cwd, None) else { return };
        host.log(LogLevel::Warn, "switch.back", &format!("pane {sid}: {} kept thread {thread}; resuming there", home.display()));
        if let Err(e) = host.submit_pty_op(sid, op) {
            host.log(LogLevel::Warn, "cycle.submit_failed", &format!("shelld_session={sid}: {e}"));
        }
    }

    fn hint(&mut self, host: &dyn PluginHost, sid: u64, why: Msg) {
        host.log(LogLevel::Info, "switch.held", &format!("pane {sid}: {why:?}"));
        // said while the TUI comes back, which takes longer than a hint lasts
        let for_ = if why == Msg::BadgeCodexStillLoaded { 4 * HINT_FOR } else { HINT_FOR };
        self.switches.hints.insert(sid, (why, Instant::now() + for_));
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

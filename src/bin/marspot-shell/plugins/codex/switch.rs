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

use super::pane::{is_thread_id, profile_switch_op, resume_op, Carry, PaneCodex, RELEASED};
use super::{last_model, update, CodexPlugin};
use crate::plugins::peerbus::moving::{self, Ask, Held, Verdict};
use crate::plugins::{handoff, pidtree, pty_op, LogLevel, PluginHost};

/// How long a reason not to switch stays beside the badge.
const HINT_FOR: Duration = Duration::from_secs(6);
/// How long a pane must have counted as quiet.  It counts so only once
/// its terminal has had no output for `PTY_QUIET_AFTER` (30 s), so the
/// wait is that and this; a codex at work redraws its spinner many
/// times a second and never gets there.
const QUIET_FOR: Duration = Duration::from_secs(3);
/// How codex's approval prompt begins.  A codex waiting on one prints
/// nothing, and looks as quiet as one that is done.
const CODEX_ASKS: &str = "Would you like to ";
/// Rows at the bottom of the screen the prompt is looked for in.
const ASK_ROWS: usize = 24;

/// A switch asked for and not over yet.
pub(super) struct Move {
    codex: ProcId,
    profile: u8,
    dir: PathBuf,
    /// Its op has been submitted.
    started: bool,
    back: Option<Back>,
}

/// Where a switch came from, to go back to when the old home kept the
/// session: the pane, the home, the thread and its directory, and what
/// was unsent in the composer of the TUI the switch ended.
struct Back {
    pane: PaneCodex,
    home: PathBuf,
    thread: String,
    cwd: Option<String>,
    held: Arc<pty_op::Job>,
}

/// What the switch keeps between ticks.
pub(super) struct Switches {
    moves: HashMap<u64, Move>,
    hints: HashMap<u64, (Msg, Instant)>,
    /// Panes whose op has ended, told from the op's own thread, and
    /// whether it ended because the old home kept the session.
    ended: Arc<Mutex<Vec<(u64, bool)>>>,
    /// A pane's screen, as text.
    pub(super) screen: fn(u64) -> Option<String>,
}

impl Default for Switches {
    fn default() -> Switches {
        let screen: fn(u64) -> Option<String> = |sid| marspot::pane_read::screen(sid, 0).ok();
        Switches { moves: HashMap::new(), hints: HashMap::new(), ended: Arc::default(), screen }
    }
}

/// `codex` or `codex resume <id>`, either with `-m <model>`: a command
/// line the resume line can stand in for.
pub(super) fn movable_argv(argv: &[String]) -> bool {
    let resumes = argv.get(1).is_some_and(|a| a == "resume");
    update::plain_argv(argv) && (!resumes || argv.get(2).is_some_and(|id| is_thread_id(id)))
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
/// drawing in it, and it is not showing codex's approval prompt.
fn terminal_settled(host: &dyn PluginHost, screen: fn(u64) -> Option<String>, sid: u64) -> bool {
    let quiet = matches!(host.pane_status(sid), Ok(Some(v)) if v.status == PaneStatus::Busy(BusyKind::ForegroundJob) && v.held >= QUIET_FOR);
    quiet && !screen(sid).is_some_and(|s| asks_approval(&s))
}

/// A row near the bottom of `screen` is codex asking for approval.
pub(super) fn asks_approval(screen: &str) -> bool {
    let rows: Vec<&str> = screen.lines().filter(|r| !r.trim().is_empty()).collect();
    let near = &rows[rows.len().saturating_sub(ASK_ROWS)..];
    near.iter().any(|r| r.trim_start().starts_with(CODEX_ASKS) && r.trim_end().ends_with('?'))
}

impl Switches {
    /// A switch is under way in pane `sid`.
    pub(super) fn moving(&self, sid: u64) -> bool {
        self.moves.contains_key(&sid)
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
        if !pane.argv.as_deref().is_some_and(movable_argv) {
            return self.hint(host, sid, Msg::BadgeCodexCustomCommand);
        }
        if !crate::plugins::peerbus::threads().get(&sid).is_some_and(|v| v.codex == pane.codex) {
            // no daemon to ask whether a turn is running: the terminal
            // is the only witness
            if !terminal_settled(host, self.switches.screen, sid) {
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
        let held = pty_op::Job::new();
        let gone: pty_op::CallFn = Arc::new(move || {
            moving::ask(Ask::Terminated { sid, codex });
            Ok(())
        });
        let model = last_model::thread_model(home, thread);
        log_model(host, sid, &model);
        let carry = Carry { held: Arc::clone(&held), taken: false, release: Some((gone, released)), model };
        let Some(op) = profile_switch_op(&pane, m.profile, &m.dir, thread, cwd.clone(), carry) else {
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
        // before the op can end: its end comes after this
        moving::ask(Ask::Submitted { sid, codex });
        if let Err(e) = host.submit_pty_op(sid, op) {
            host.log(LogLevel::Warn, "cycle.submit_failed", &format!("shelld_session={sid}: {e}"));
            self.switches.moves.remove(&sid);
            moving::ask(Ask::End { sid, codex });
            return;
        }
        if let Some(m) = self.switches.moves.get_mut(&sid) {
            m.started = true;
            m.back = Some(Back { pane, home: home.to_path_buf(), thread: thread.to_string(), cwd, held });
        }
    }

    /// The old home did not let go in time: the TUI is gone, so put it
    /// back where the session still is, with the composer the switch took.
    fn switch_back(&mut self, host: &dyn PluginHost, sid: u64, m: Move) {
        self.hint(host, sid, Msg::BadgeCodexStillLoaded);
        let Some(Back { pane, home, thread, cwd, held }) = m.back else { return };
        let model = last_model::thread_model(&home, &thread);
        log_model(host, sid, &model);
        let carry = Carry { held, taken: true, release: None, model };
        let Some(op) = resume_op("codex.switch_back", "↩".into(), &pane, &home, &thread, cwd, carry) else { return };
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

/// Which model a resume names, or that it names none.
fn log_model(host: &dyn PluginHost, sid: u64, model: &Option<String>) {
    match model {
        Some(m) => host.log(LogLevel::Info, "switch.model", &format!("pane {sid}: resuming on {m}, as its rollouts last recorded")),
        None => host.log(LogLevel::Info, "switch.model", &format!("pane {sid}: no model observed for this thread or account; resuming on config.toml's")),
    }
}

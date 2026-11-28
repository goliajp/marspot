//! Putting codex back in panes whose session process started afresh.
//!
//! Each codex pane's thread is kept on disk (`last_bound.tsv`) once it
//! is known: from peerbus's binding, or from a `codex resume <id>` argv.
//! After the machine restarts, a pane that comes back with a new idle
//! shell, young and untouched, gets `codex resume <thread>` under the
//! home the thread ran under.  Always that home: the thread lives in
//! that home's daemon and rollouts, and another account would have to
//! take it over the way a switch does.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use marspot::pidtree::ProcId;

use super::cold_record::{self as record, LastThread};
use super::{last_model, looks_like_codex, CodexPlugin};
use crate::observe::{Observation, Session};
use crate::plugins::cold_restore::{self as cold, Held, Pace, PaneNow, Unbound, CAP};
use crate::plugins::{handoff, launches, pty_op, LogLevel, PluginHost};

const OP_NAME: &str = "codex.cold_restore";
const FILE: &str = "last_bound.tsv";

/// The plugin's cold-restore state.
pub(super) struct Cold {
    pub(super) rows: Vec<LastThread>,
    gone_since: HashMap<u64, Instant>,
    /// Threads a restore was started for by this process: once each.
    /// Panes are claimed on the pace, shared with every agent.
    tried_threads: HashSet<String>,
    /// Panes whose restore this plugin started and has not seen end.
    running: HashSet<u64>,
    pub(super) pace: Pace,
    pub(super) last_input: fn(u64) -> Option<SystemTime>,
    pub(super) ends: Arc<Mutex<Vec<(u64, pty_op::OpOutcome)>>>,
}

impl Default for Cold {
    fn default() -> Self {
        Cold {
            rows: Vec::new(),
            gone_since: HashMap::new(),
            tried_threads: HashSet::new(),
            running: HashSet::new(),
            pace: Pace::default(),
            last_input: cold::hub_last_input,
            ends: Arc::default(),
        }
    }
}

/// A pane running codex this tick, its thread known.
pub(super) struct Bound {
    pub(super) sid: u64,
    pub(super) thread: String,
    pub(super) home: String,
    pub(super) cwd: Option<String>,
    pub(super) model: Option<String>,
}

/// The pane as the kernel has it in `s`.
fn pane_now(s: &Session, last_input: fn(u64) -> Option<SystemTime>) -> Option<PaneNow> {
    let idle_shell = s.front().is_none() && s.find("codex").is_none();
    Some(PaneNow { shell: s.shell?, idle_shell, last_input: last_input(s.entry.id) })
}

impl CodexPlugin {
    pub(super) fn load_last_bound(&mut self, host: &dyn PluginHost) {
        let Ok(dir) = host.state_dir() else { return };
        if let Ok(text) = std::fs::read_to_string(dir.join(FILE)) {
            self.cold.rows = record::decode(&text);
        }
    }

    fn persist_last_bound(&self, host: &dyn PluginHost) {
        let Ok(dir) = host.state_dir() else { return };
        if let Err(e) = handoff::write_atomic(&dir.join(FILE), record::encode(&self.cold.rows).as_bytes()) {
            host.log(LogLevel::Warn, "cold_restore.persist_failed", &format!("{e}"));
        }
    }

    /// Rows waiting on a pane whose session process runs and has no
    /// codex: they are owed a restore or about to go, and the tick has
    /// to keep coming until they are settled.  A pane another agent
    /// restored waits on nothing of ours; one we restore, on its end.
    pub(super) fn cold_pending(&self, obs: &Observation) -> bool {
        let waits = |sid: u64| self.cold.running.contains(&sid) || !self.cold.pace.claimed(sid);
        self.cold.rows.iter().any(|r| waits(r.sid) && obs.session(r.sid).is_some_and(|s| s.live && s.shell.is_some() && super::pane_codex(s).is_none()))
    }

    /// Everything this module does on a tick; `bound` the codex panes
    /// whose thread is known, `with_codex` every pane running codex.
    pub(super) fn cold_tick(&mut self, host: &dyn PluginHost, obs: &Observation, bound: &[Bound], with_codex: &HashSet<u64>) {
        self.settle_cold_ends(host);
        self.note_threads(host, obs, bound, with_codex);
        let last_input = self.cold.last_input;
        self.cold_restore(host, obs, bound, with_codex, |sid| {
            // the latest observation picks the candidates; one that is
            // to be typed into is asked again of a fresh one
            if !cold::owed_shell(&pane_now(obs.session(sid)?, last_input)?, SystemTime::now()) {
                return None;
            }
            pane_now(crate::observe::shared().fresh(crate::observe::NOW).session(sid)?, last_input)
        });
    }

    /// Keep each bound pane's row current and drop the rows that no
    /// longer owe anything.
    pub(super) fn note_threads(&mut self, host: &dyn PluginHost, obs: &Observation, bound: &[Bound], with_codex: &HashSet<u64>) {
        let shell = |sid: u64| obs.session(sid).and_then(|s| s.shell);
        let mut dirty = false;
        let helpers = (!bound.is_empty()).then(|| crate::plugins::claudecode::with_helpers::HelperRoles::of_observation(obs));
        for b in bound {
            if helpers.as_ref().is_some_and(|h| h.is_helper(b.sid)) {
                continue;
            }
            let row = shell(b.sid).and_then(|sh| record::row_of(b.sid, &b.thread, &b.home, b.cwd.clone(), b.model.clone(), sh));
            if let Some(row) = row {
                dirty |= record::upsert(&mut self.cold.rows, row);
            }
        }
        let u = Unbound {
            bound: &|sid| with_codex.contains(&sid),
            listed: &cold::pane_open,
            shell: &shell,
            last_input: &self.cold.last_input,
            now: Instant::now(),
            wall: SystemTime::now(),
        };
        let dropped = cold::forget(&mut self.cold.rows, &mut self.cold.gone_since, &u);
        for sid in &dropped {
            host.log(LogLevel::Info, "cold_restore.forget", &format!("pane {sid}: closed, codex left it, or its new shell is past the window or was typed into"));
        }
        if dirty || !dropped.is_empty() {
            self.persist_last_bound(host);
        }
    }

    /// Start the restore of at most one pane that is owed one.
    pub(super) fn cold_restore(&mut self, host: &dyn PluginHost, obs: &Observation, bound: &[Bound], with_codex: &HashSet<u64>, pane: impl Fn(u64) -> Option<PaneNow>) {
        // a thread already open in a pane, or already being resumed, is
        // not resumed in another
        let open: HashSet<&str> = bound.iter().map(|b| b.thread.as_str()).collect();
        let mut waiting: Vec<&LastThread> = self
            .cold
            .rows
            .iter()
            .filter(|r| obs.session(r.sid).is_some_and(|s| s.live) && !with_codex.contains(&r.sid))
            .filter(|r| !open.contains(r.thread.as_str()) && !self.cold.tried_threads.contains(&r.thread))
            .filter(|r| !self.cold.pace.claimed(r.sid) && !self.switches.moving(r.sid))
            .collect();
        if waiting.is_empty() {
            return;
        }
        match self.cold.pace.may_start() {
            Ok(()) => {}
            Err(Held::Capped { first: true }) => {
                return host.log(LogLevel::Warn, "cold_restore.cap", &format!("{CAP} restores started; the rest are left to the person"));
            }
            Err(_) => return,
        }
        waiting.sort_by_key(|r| r.sid);
        let wall = SystemTime::now();
        let Some((row, now)) = waiting.into_iter().find_map(|r| pane(r.sid).filter(|p| cold::owed(r, Some(p), wall)).map(|p| (r.clone(), p))) else {
            return;
        };
        if !self.cold.pace.claim(row.sid) {
            return;
        }
        self.cold.tried_threads.insert(row.thread.clone());
        self.start_cold_restore(host, &row, now.shell);
    }

    fn start_cold_restore(&mut self, host: &dyn PluginHost, row: &LastThread, shell: ProcId) {
        // the thread's own model or none: `-m` with another thread's
        // would run the resume on the wrong one
        let model = row.model.clone().or_else(|| last_model::own_thread_model(std::path::Path::new(&row.home), &row.thread)).filter(|m| record::model_ok(m));
        let sid = row.sid;
        let Some(op) = cold_restore_op(row, model.as_deref(), shell) else {
            host.log(LogLevel::Warn, "cold_restore.no_line", &format!("pane {sid}: its resume line cannot be quoted; not tried again"));
            return self.drop_row(host, sid);
        };
        let ends = Arc::clone(&self.cold.ends);
        let op = op.on_end(Arc::new(move |o| ends.lock().unwrap().push((sid, o.clone()))));
        let line = format!(
            "pane {sid}: resuming {} on {} (the account it ran on) model={}",
            row.thread,
            launches::account_of_home(Some(&row.home)),
            model.as_deref().unwrap_or("-")
        );
        match host.submit_pty_op(sid, op) {
            Ok(()) => {
                self.cold.pace.note_start();
                self.cold.running.insert(sid);
                host.log(LogLevel::Info, "cold_restore.start", &line);
            }
            Err(e) => {
                host.log(LogLevel::Warn, "cold_restore.submit_failed", &format!("pane {sid}: {e}; not tried again"));
                self.drop_row(host, sid);
            }
        }
    }

    fn drop_row(&mut self, host: &dyn PluginHost, sid: u64) {
        self.cold.rows.retain(|r| r.sid != sid);
        self.persist_last_bound(host);
    }

    /// Forget the row of every pane whose restore did not bring codex
    /// back: the next start of this process does not try it again.
    pub(super) fn settle_cold_ends(&mut self, host: &dyn PluginHost) {
        let ended = std::mem::take(&mut *self.cold.ends.lock().unwrap());
        for (sid, _) in &ended {
            self.cold.running.remove(sid);
        }
        let failed: Vec<(u64, pty_op::OpOutcome)> = ended.into_iter().filter(|(_, o)| !o.is_done()).collect();
        for (sid, outcome) in &failed {
            host.log(LogLevel::Warn, "cold_restore.failed", &format!("pane {sid}: {outcome:?}; not tried again"));
            self.cold.rows.retain(|r| r.sid != *sid);
        }
        if !failed.is_empty() {
            self.persist_last_bound(host);
        }
    }
}

/// Type the resume line at the pane's idle shell and hold the restored
/// picture until codex has drawn.  No interrupt goes first: the shell is
/// new and nobody has typed into it, which `untouched` checks right
/// before the line, so there is nothing on its line to clear.
pub(super) fn cold_restore_op(row: &LastThread, model: Option<&str>, shell: ProcId) -> Option<pty_op::PtyOp> {
    let mut cmd = pty_op::PtyCommand::new("codex").env("CODEX_HOME", row.home.clone()).arg("resume").arg(row.thread.clone());
    if let Some(m) = model {
        cmd = cmd.arg("-m").arg(m);
    }
    let line = cmd.in_dir(row.cwd.clone()).clear_screen_first(true).to_bytes()?;
    let pid = shell.pid;
    let codex = || pty_op::Step::stop_if_process(pid, looks_like_codex);
    let from = cold::started(shell);
    Some(
        pty_op::PtyOp::new(OP_NAME)
            .hold_screen(true)
            .step(codex())
            .step(pty_op::Step::await_shell(pid).timeout(Duration::from_secs(5)))
            .step(codex())
            .step(pty_op::Step::untouched(from, from + cold::WINDOW))
            .step(pty_op::Step::send(line).named("resume").at_shell(pid))
            .step(pty_op::Step::await_process(pid, looks_like_codex).timeout(Duration::from_secs(20)))
            .step(pty_op::Step::watch_process(pid, looks_like_codex))
            .step(
                pty_op::Step::await_quiet(Duration::from_millis(1500))
                    .after_bytes(2048)
                    .timeout(Duration::from_secs(30))
                    // a resumed thread still at work keeps its spinner
                    // going; past the floor the codex is up
                    .or_drawn_late()
                    .named("first_frame"),
            ),
    )
}

#[cfg(test)]
#[path = "cold_restore_tests.rs"]
mod tests;

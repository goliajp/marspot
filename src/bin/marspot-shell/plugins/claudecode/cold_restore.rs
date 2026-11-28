//! Putting claude back in panes whose session process started afresh.
//!
//! After the machine restarts, every pane's session process comes back
//! with its screen and a new shell, and nothing of the claude that ran
//! there.  This resumes, in each such pane, the session it last ran,
//! under the account it ran on (another only when that account cannot
//! take it: a session moved to another account's organization has to be
//! read again from its first line), on the model it ran on.  One pane at
//! a time, `STAGGER` apart: each resume reads a whole transcript.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::SystemTime;

use super::cold_record::{self as record, LastBound, PaneNow, Unbound};
use super::*;
use crate::plugins::launches;

#[cfg(test)]
pub(super) use crate::plugins::cold_restore::STAGGER;
use crate::plugins::cold_restore::{Held, Pace, CAP};

const OP_NAME: &str = "cc.cold_restore";

/// The plugin's cold-restore state.
pub(super) struct Cold {
    pub(super) rows: Vec<LastBound>,
    gone_since: HashMap<u64, Instant>,
    /// Panes, and sessions, a restore was started for by this process:
    /// once each.  At most `CAP` of each.
    tried: HashSet<u64>,
    tried_uuids: HashSet<String>,
    pub(super) pace: Pace,
    /// When a pane last had input; the control hub's record.
    pub(super) last_input: fn(u64) -> Option<SystemTime>,
    pub(super) ends: Arc<Mutex<Vec<(u64, pty_op::OpOutcome)>>>,
    pub(super) seen: launches::Seen,
}

impl Default for Cold {
    fn default() -> Self {
        Cold {
            rows: Vec::new(),
            gone_since: HashMap::new(),
            tried: HashSet::new(),
            tried_uuids: HashSet::new(),
            pace: Pace::default(),
            last_input: crate::plugins::cold_restore::hub_last_input,
            ends: Arc::default(),
            seen: launches::Seen::default(),
        }
    }
}

impl ClaudecodePlugin {
    pub(super) fn load_last_bound(&mut self, host: &dyn PluginHost) {
        let Ok(dir) = host.state_dir() else { return };
        if let Ok(text) = fs::read_to_string(dir.join("last_bound.tsv")) {
            self.cold.rows = record::decode(&text);
        }
    }

    fn persist_last_bound(&self, host: &dyn PluginHost) {
        let Ok(dir) = host.state_dir() else { return };
        if let Err(e) = crate::plugins::handoff::write_atomic(&dir.join("last_bound.tsv"), record::encode(&self.cold.rows).as_bytes()) {
            host.log(LogLevel::Warn, "cold_restore.persist_failed", &format!("{e}"));
        }
    }

    /// Everything this module does on a scan.
    pub(super) fn cold_tick(&mut self, host: &dyn PluginHost, result: &ScanResult) {
        let obs = crate::observe::shared().latest();
        self.settle_cold_ends(host);
        self.note_bindings(host, result, &obs);
        for (sid, meta) in &result.new_meta {
            self.cold.seen.saw(*sid, "claude", meta.claude, &launches::account_of_profile(meta.profile_num));
        }
        self.cold.seen.end_pass(|sid| result.sessions_seen.contains(&sid));
        let last_input = self.cold.last_input;
        let now = |s: &crate::observe::Session| {
            let idle_shell = s.front().is_none() && s.find("claude").is_none();
            Some(PaneNow { shell: s.shell?, idle_shell, last_input: last_input(s.entry.id) })
        };
        self.cold_restore(host, result, |sid| {
            // the latest observation picks the candidates; one that is
            // to be typed into is asked again of a fresh one
            if !record::owed_shell(&now(obs.session(sid)?)?, SystemTime::now()) {
                return None;
            }
            now(crate::observe::shared().fresh(crate::observe::NOW).session(sid)?)
        });
    }

    /// Keep each bound pane's row current and drop the rows that no
    /// longer owe anything.
    pub(super) fn note_bindings(&mut self, host: &dyn PluginHost, result: &ScanResult, obs: &crate::observe::Observation) {
        let shell = |sid: u64| obs.session(sid).and_then(|s| s.shell);
        let mut dirty = false;
        for (sid, meta) in &result.new_meta {
            if result.helpers.is_helper(*sid) {
                continue;
            }
            let row = shell(*sid).and_then(|sh| record::row_of(*sid, meta, result.choices.get(sid), sh));
            if let Some(row) = row {
                dirty |= record::upsert(&mut self.cold.rows, row);
            }
        }
        let u = Unbound {
            bound: &|sid| result.new_meta.contains_key(&sid),
            listed: &record::pane_open,
            shell: &shell,
            last_input: &self.cold.last_input,
            now: Instant::now(),
            wall: SystemTime::now(),
        };
        let dropped = record::forget(&mut self.cold.rows, &mut self.cold.gone_since, &u);
        for sid in &dropped {
            host.log(LogLevel::Info, "cold_restore.forget", &format!("pane {sid}: closed, claude left it, or its new shell is past the window or was typed into"));
        }
        if dirty || !dropped.is_empty() {
            self.persist_last_bound(host);
        }
    }

    /// Start the restore of at most one pane that is owed one.
    pub(super) fn cold_restore(&mut self, host: &dyn PluginHost, result: &ScanResult, pane: impl Fn(u64) -> Option<PaneNow>) {
        if self.shelld.is_none() {
            return;
        }
        // a session already bound in a pane, or already being resumed,
        // is not resumed in another
        let bound: HashSet<&str> = result.new_meta.values().map(|m| m.uuid.as_str()).collect();
        let mut waiting: Vec<&LastBound> = self
            .cold
            .rows
            .iter()
            .filter(|r| result.sessions_seen.contains(&r.sid) && !result.new_meta.contains_key(&r.sid))
            .filter(|r| !bound.contains(r.uuid.as_str()) && !self.cold.tried_uuids.contains(&r.uuid))
            .filter(|r| !self.cold.tried.contains(&r.sid) && !self.armed.contains(&r.sid))
            .filter(|r| !self.dormant.iter().any(|d| d.shelld_sid == r.sid))
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
        let Some((row, now)) = waiting.into_iter().find_map(|r| pane(r.sid).filter(|p| record::owed(r, Some(p), wall)).map(|p| (r.clone(), p))) else {
            return;
        };
        self.cold.tried.insert(row.sid);
        self.cold.tried_uuids.insert(row.uuid.clone());
        self.start_cold_restore(host, result, &row, now.shell);
    }

    fn start_cold_restore(&mut self, host: &dyn PluginHost, result: &ScanResult, row: &LastBound, shell: pidtree::ProcId) {
        let (profile, why) = self.cold_account(result, row);
        let config_dir = if profile == row.profile {
            row.config_dir.clone()
        } else {
            std::env::var("HOME").ok().map(|h| format!("{h}/.claude-profile-{profile}"))
        };
        let choice = cold_choice(row, config_dir.as_deref());
        let Some(op) = cold_restore_op(&row.uuid, config_dir.as_deref(), shell, row.project_dir.as_deref(), &choice) else {
            host.log(LogLevel::Warn, "cold_restore.no_line", &format!("pane {}: its resume line cannot be quoted", row.sid));
            return;
        };
        let ends = Arc::clone(&self.cold.ends);
        let sid = row.sid;
        let op = op.on_end(Arc::new(move |o| ends.lock().unwrap().push((sid, o.clone()))));
        let line = format!(
            "pane {sid}: resuming {} on {} ({why}) model={} effort={}",
            row.uuid,
            launches::account_of_profile(profile),
            choice.model.as_deref().unwrap_or("-"),
            choice.effort.as_deref().unwrap_or("-")
        );
        match host.submit_pty_op(sid, op) {
            Ok(()) => {
                self.cold.pace.note_start();
                host.log(LogLevel::Info, "cold_restore.start", &line);
            }
            Err(e) => host.log(LogLevel::Warn, "cold_restore.submit_failed", &format!("pane {sid}: {e}")),
        }
    }

    /// The account a restore goes to: the one the session ran on, unless
    /// the usage feed says it cannot take it and the allocator names
    /// another.
    fn cold_account(&mut self, result: &ScanResult, row: &LastBound) -> (u8, &'static str) {
        let Some(usage) = result.usage.as_ref().filter(|_| row.profile != 0) else {
            return (row.profile, "the account it ran on");
        };
        let profiles: Vec<u8> = result.profiles.iter().map(|(n, _)| *n).collect();
        let email = |p: u8| result.profiles.iter().find(|(n, _)| *n == p).and_then(|(_, e)| e.clone());
        let rooms = quota::rooms_for(&profiles, &usage.accounts, email, rooms::unix_now());
        let model = row.choice.model.as_deref();
        if !rooms.iter().any(|(p, r)| *p == row.profile && r.refused_for(model)) {
            return (row.profile, "the account it ran on");
        }
        let class = allocation_view::class_of_judged(model);
        match self.view_for(result).must_move(&rooms, row.sid, row.profile, class).0 {
            quota::Move::To(to) => (to, "the account it ran on cannot take it"),
            quota::Move::Hold(why) => (row.profile, why),
        }
    }

    /// Forget the row of every pane whose restore did not bring claude
    /// back: the next start of this process does not try it again.
    pub(super) fn settle_cold_ends(&mut self, host: &dyn PluginHost) {
        let ended = std::mem::take(&mut *self.cold.ends.lock().unwrap());
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

/// The model and effort a restore starts on: the row's, else the
/// session's own record, the account's last observed model, the default.
fn cold_choice(row: &LastBound, config_dir: Option<&str>) -> CliChoice {
    if row.choice.model.is_some() {
        return row.choice.clone();
    }
    use launch_model::Source;
    let account = config_dir_or_default(config_dir);
    let (model, _) = launch_model::first_known(&[
        (Source::Session, &|| launch_model::session_model(&row.uuid, None)),
        (Source::Account, &|| launch_model::account_model(&account)),
    ]);
    CliChoice { model: Some(model), effort: row.choice.effort.clone() }
}

/// Type the resume line at the pane's idle shell and hold the restored
/// picture until claude has drawn.  No interrupt goes first: the shell
/// is new and nobody has typed into it, which `untouched` checks right
/// before the line, so there is nothing on its line to clear.
pub(super) fn cold_restore_op(uuid: &str, config_dir: Option<&str>, shell: pidtree::ProcId, project_dir: Option<&str>, choice: &CliChoice) -> Option<pty_op::PtyOp> {
    let mut cmd = pty_op::PtyCommand::new("claude").clear_screen_first(true);
    if let Some(dir) = config_dir {
        cmd = cmd.env("CLAUDE_CONFIG_DIR", dir);
    }
    cmd = crate::plugins::peerbus::claude_mcp::with_config(cmd);
    cmd = cmd.arg("--resume").arg(uuid).in_dir(project_dir);
    cmd = cmd.arg("--model").quoted_arg(choice.model.clone()?);
    if let Some(effort) = &choice.effort {
        cmd = cmd.arg("--effort").arg(effort.clone());
    }
    let line = cmd.to_bytes()?;
    let pid = shell.pid;
    let claude = || pty_op::Step::stop_if_process(pid, looks_like_claudecode);
    let from = record::started(shell);
    Some(
        pty_op::PtyOp::new(OP_NAME)
            .hold_screen(true)
            .step(claude())
            .step(pty_op::Step::await_shell(pid).timeout(Duration::from_secs(5)))
            .step(claude())
            .step(pty_op::Step::untouched(from, from + record::WINDOW))
            .step(pty_op::Step::send(line).named("resume").at_shell(pid))
            .step(pty_op::Step::await_process(pid, looks_like_claudecode).timeout(Duration::from_secs(20)))
            .step(pty_op::Step::watch_process(pid, looks_like_claudecode))
            .step(
                pty_op::Step::await_quiet(WAKE_QUIET_FOR)
                    .after_bytes(FIRST_FRAME_BYTES)
                    .timeout(WAKE_WATCHDOG)
                    .or_composer()
                    .named("first_frame"),
            ),
    )
}

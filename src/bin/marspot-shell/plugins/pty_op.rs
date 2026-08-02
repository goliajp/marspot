//! Scripted PTY operations — the infrastructure under "do something to
//! a pane on the user's behalf".
//!
//! Two of these existed before this module, hand-written: the profile
//! cycle (kill claude, retype `claudeN --resume …`) and idle
//! reclamation (hold the picture, kill claude, park, resume on focus).
//! Both spelled out the same skeleton — freeze, signal, escalate to
//! SIGKILL, poll the process table, type a line, settle, watchdog,
//! spinner, clean up — and differed only in two places that are
//! genuinely business: *what to type* and *how long to wait for what*.
//!
//! Written twice, the skeleton belongs in one place.  A caller here
//! declares the sequence; this module owns running it, and owns the
//! parts that are easy to get wrong the second time:
//!
//! - **Cleanup happens exactly once**, on every exit path — finished,
//!   timed out, user pressed Esc, the pane closed under it.  A pane
//!   left holding its picture with nobody to release it is the worst
//!   failure this code has, and it is not the caller's job to remember.
//! - **Every wait has a deadline** except the one that is deliberately
//!   indefinite ([`StepKind::AwaitUser`]), and the deadline is per
//!   step, so a slow step cannot silently eat the next one's budget.
//! - **The screen hold is L3's**, not the core's: a silent update
//!   restarts the core, so a core-side freeze evaporates mid-operation.
//! - **Effects go through [`OpEnv`]**, so the sequencing is testable
//!   without a PTY, a process table, or a wall clock.
//!
//! What is deliberately *not* here: anything that knows what claude is.
//! Matching a process, choosing a command line, deciding when a session
//! is idle — all of that stays with the plugin that has an opinion.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::plugins::{EndReason, KeyHandling, LogLevel, PaneSession, PaneSessionHost};
use marspot::pidtree;

/// Bytes to a pane's PTY, and holds on its picture.
///
/// Implemented over whatever can reach the pane (in production, the
/// plugin host's inject-input channel).  Kept separate from [`OpEnv`]
/// so the process/clock half can be real while this half is a fake.
pub trait PtyIo: Send + Sync {
    fn send(&self, sid: u64, bytes: &[u8]) -> std::io::Result<()>;
    /// Ask L3 to hold this pane's picture where it is (or release it).
    fn hold(&self, sid: u64, on: bool) -> std::io::Result<()>;
    /// Deliver text the way a paste would arrive.
    ///
    /// Not the same as [`send`](Self::send): only L3 knows whether the
    /// program in the pane has bracketed paste on, and multi-line text
    /// delivered without it is executed a line at a time.  Anything
    /// handing a *message* to a running program takes this route.
    fn paste(&self, sid: u64, text: &str) -> std::io::Result<()>;
}

/// Everything a running op needs from the world.
///
/// One trait so a test can drive a whole script in microseconds with a
/// clock it controls — the alternative is sleeping through real
/// timeouts, which is how these paths went untested the first time.
pub trait OpEnv: Send {
    fn io(&self) -> &Arc<dyn PtyIo>;
    fn now(&self) -> SystemTime;
    fn signal(&self, pid: i32, sig: i32);
    fn pid_alive(&self, pid: i32) -> bool;
    /// A descendant of `under` matching `pred`, if any.
    fn find_descendant(&self, under: i32, pred: fn(&pidtree::ProcRow) -> bool) -> Option<i32>;
    /// Bytes this pane's PTY has produced, ever.  Monotone; only the
    /// differences matter.
    fn output_len(&self, sid: u64) -> u64;
}

/// What one step of a script does.
#[derive(Clone)]
pub enum StepKind {
    /// Do nothing for a while.  For letting an earlier effect land —
    /// a hold crosses two process boundaries, a signal crosses none.
    Settle,
    /// Take `pid` down and wait for it to go, escalating if the polite
    /// signal does not take.
    ///
    /// One step rather than "signal" followed by "wait", which is how
    /// this read at first: the escalation was configured on the signal
    /// but performed by the wait, so the runner had to look back a step
    /// to find it and a reader had to know the two were a pair.  They
    /// are one thing — "make this process go away" — and every caller
    /// used them together.
    Terminate { pid: i32, signal: i32, escalate: Option<(Duration, i32)>, sent: bool },
    /// Wait for a descendant of `under` to match.
    AwaitProcess { under: i32, matching: fn(&pidtree::ProcRow) -> bool },
    /// Finish the run early, successfully, if the process is already
    /// there.
    ///
    /// "What we were about to arrange has already happened."  A script
    /// that parks a session and later types it back has a gap between
    /// deciding to type and typing — the user's own click is in that
    /// gap — and in that gap the session can return by other means.
    /// Typing then puts the command into the running program's prompt,
    /// where it sits as text the user has to delete.
    StopIfProcess { under: i32, matching: fn(&pidtree::ProcRow) -> bool },
    /// Wait for the user to come back — focus or a keystroke.  The one
    /// step with no deadline: a parked pane may sit for days.
    AwaitUser,
    /// Type bytes into the pane's PTY, verbatim.
    Send(Vec<u8>),
    /// Deliver text as a paste — mode-aware, and safe for text with
    /// newlines in it.  This is the one that carries a message to
    /// whatever is running in the pane.
    Paste(String),
    /// Wait until the pane has produced output and then gone still for
    /// `still`.
    ///
    /// Measured from this step's own start, which is what makes it
    /// composable: put it after `AwaitProcess` and it means "wait for
    /// the thing that just appeared to finish drawing", with no need
    /// to guess how many bytes a first frame costs.
    AwaitQuiet { still: Duration },
}

/// One step: what to do, how long it may take, and what to call it in
/// the log.
#[derive(Clone)]
pub struct Step {
    pub kind: StepKind,
    /// `None` = no deadline.  Only [`StepKind::AwaitUser`] should use
    /// it; the builder is where that is enforced by construction.
    pub timeout: Option<Duration>,
    pub label: &'static str,
}

impl Step {
    pub fn settle(d: Duration) -> Self {
        Self { kind: StepKind::Settle, timeout: Some(d), label: "settle" }
    }
    /// Signal `pid` and wait for it to leave the process table.
    pub fn terminate(pid: i32, signal: i32) -> Self {
        Self {
            kind: StepKind::Terminate { pid, signal, escalate: None, sent: false },
            timeout: Some(Duration::from_secs(10)),
            label: "terminate",
        }
    }
    /// Escalate to `sig` if the process is still there after `after`.
    pub fn escalate_after(mut self, after: Duration, sig: i32) -> Self {
        if let StepKind::Terminate { escalate, .. } = &mut self.kind {
            *escalate = Some((after, sig));
        }
        self
    }
    /// Finish here if `matching` is already running under `under`.
    pub fn stop_if_process(under: i32, matching: fn(&pidtree::ProcRow) -> bool) -> Self {
        Self {
            kind: StepKind::StopIfProcess { under, matching },
            timeout: Some(Duration::from_secs(5)),
            label: "stop_if_process",
        }
    }
    pub fn await_process(under: i32, matching: fn(&pidtree::ProcRow) -> bool) -> Self {
        Self {
            kind: StepKind::AwaitProcess { under, matching },
            timeout: Some(Duration::from_secs(30)),
            label: "await_process",
        }
    }
    pub fn await_user() -> Self {
        Self { kind: StepKind::AwaitUser, timeout: None, label: "await_user" }
    }
    pub fn send(bytes: Vec<u8>) -> Self {
        Self { kind: StepKind::Send(bytes), timeout: Some(Duration::from_secs(5)), label: "send" }
    }
    pub fn paste(text: impl Into<String>) -> Self {
        Self { kind: StepKind::Paste(text.into()), timeout: Some(Duration::from_secs(5)), label: "paste" }
    }
    pub fn await_quiet(still: Duration) -> Self {
        Self {
            kind: StepKind::AwaitQuiet { still },
            timeout: Some(Duration::from_secs(30)),
            label: "await_quiet",
        }
    }
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }
    pub fn named(mut self, label: &'static str) -> Self {
        self.label = label;
        self
    }
}

/// How a run ended.  Handed to the caller's completion hook, and
/// logged either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpOutcome {
    /// Every step ran.
    Done,
    /// A step outlived its deadline.
    TimedOut { step: usize, label: &'static str },
    /// The user pressed Esc, or the host ended the session.
    Cancelled(&'static str),
    /// An effect failed (the PTY is gone, the core is down).
    Failed { step: usize, label: &'static str, err: String },
}

impl OpOutcome {
    pub fn is_done(&self) -> bool {
        matches!(self, Self::Done)
    }
}

/// A script plus the presentation choices that go with it.
#[derive(Clone)]
pub struct PtyOp {
    /// Log prefix; also what shows up in the outcome line.  Namespaced
    /// by convention: `cc.wake`, `cc.profile_cycle`.
    pub name: &'static str,
    pub steps: Vec<Step>,
    /// Hold the pane's picture for the whole run.  Released exactly
    /// once, whatever the outcome.
    pub hold_screen: bool,
    /// Swallow the user's keys while it runs.  Without this, a
    /// keystroke lands in the shell between one step and the next.
    pub lock_keys: bool,
    /// Esc ends the run.  Off for scripts where a half-finished state
    /// is worse than waiting (nothing uses that yet, but a "park" is
    /// exactly that shape).
    pub escape_hatch: bool,
    /// Badge while running; a spinner is appended.
    pub badge: Option<String>,
}

impl PtyOp {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            steps: Vec::new(),
            hold_screen: false,
            lock_keys: true,
            escape_hatch: true,
            badge: None,
        }
    }
    pub fn step(mut self, s: Step) -> Self {
        self.steps.push(s);
        self
    }
    /// Swallow the user's keys while it runs.  On by default: a script
    /// that types needs the pane to itself.  Off for a *delivery* — the
    /// user may keep typing in a pane something was handed to.
    pub fn lock_keys(mut self, on: bool) -> Self {
        self.lock_keys = on;
        self
    }
    pub fn hold_screen(mut self, on: bool) -> Self {
        self.hold_screen = on;
        self
    }
    pub fn escape_hatch(mut self, on: bool) -> Self {
        self.escape_hatch = on;
        self
    }
    pub fn badge(mut self, text: impl Into<String>) -> Self {
        self.badge = Some(text.into());
        self
    }
}

/// Most a single step may deliver into a pane.
///
/// Not a performance limit — a bound on blast radius.  Everything that
/// reaches a PTY this way is, from the program's point of view,
/// something the user typed; a runaway caller pasting megabytes into
/// someone else's session is the failure worth making impossible before
/// there are callers rather than after.
pub const MAX_PAYLOAD: usize = 64 * 1024;

/// Braille spinner — the eight standard frames, one per tick.
pub fn spinner_frame(phase: u8) -> char {
    const FRAMES: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];
    FRAMES[(phase as usize) % FRAMES.len()]
}

/// Runs a [`PtyOp`] as a PaneSession.
pub struct OpRunner {
    op: PtyOp,
    env: Box<dyn OpEnv>,
    /// Index of the step being run.
    at: usize,
    /// When the current step started — every deadline is relative to
    /// this, so one slow step cannot eat the next one's budget.
    entered: SystemTime,
    /// `AwaitQuiet` bookkeeping: output length and when it last moved.
    seen_len: u64,
    still_since: SystemTime,
    drew: bool,
    /// Set once the run is over; the next tick ends the session.  Two
    /// ticks rather than one because ending is the host's to do.
    finished: Option<OpOutcome>,
    /// Cleanup is idempotent, and this is what makes it so.
    cleaned: bool,
    /// Whether the first tick has run.  An explicit flag rather than
    /// something inferred from the other fields: the first cut tried
    /// to recognise "have we started?" from the clock and the byte
    /// counter, and since entering a step sets both to exactly what
    /// that test looked for, every tick re-entered step 0 — the run
    /// could never time out because its deadline restarted 60 times a
    /// second.
    started: bool,
    spin_phase: u8,
    on_finish: Option<Box<dyn FnMut(&dyn PaneSessionHost, &OpOutcome) + Send>>,
}

impl OpRunner {
    pub fn new(op: PtyOp, env: Box<dyn OpEnv>) -> Self {
        let now = env.now();
        Self {
            op,
            env,
            at: 0,
            entered: now,
            seen_len: 0,
            still_since: now,
            drew: false,
            finished: None,
            cleaned: false,
            started: false,
            spin_phase: 0,
            on_finish: None,
        }
    }

    /// Start partway in.  An L1 restart replaces the process while the
    /// pane keeps its state: a parked pane is re-armed at its
    /// `AwaitUser` step rather than being killed and parked again.
    pub fn start_at(mut self, step: usize) -> Self {
        self.at = step.min(self.op.steps.len());
        self
    }

    /// Called once when the run ends, with the outcome.  This is where
    /// a caller does its own bookkeeping (drop a record, re-badge).
    pub fn on_finish(
        mut self,
        f: impl FnMut(&dyn PaneSessionHost, &OpOutcome) + Send + 'static,
    ) -> Self {
        self.on_finish = Some(Box::new(f));
        self
    }

    /// Which step is running.  For a caller that persists progress.
    pub fn step_index(&self) -> usize {
        self.at
    }

    fn finish(&mut self, host: &dyn PaneSessionHost, outcome: OpOutcome) {
        if self.finished.is_some() {
            return;
        }
        host.log(
            if outcome.is_done() { LogLevel::Info } else { LogLevel::Warn },
            &format!("{}.outcome", self.op.name),
            &format!("{outcome:?}"),
        );
        self.finished = Some(outcome);
    }

    /// Release the hold.  Runs on every exit path, at most once.
    fn cleanup(&mut self, host: &dyn PaneSessionHost) {
        if self.cleaned {
            return;
        }
        self.cleaned = true;
        if self.op.hold_screen {
            if let Err(e) = self.env.io().hold(host.shelld_session_id(), false) {
                host.log(
                    LogLevel::Warn,
                    &format!("{}.release_failed", self.op.name),
                    &format!("{e}"),
                );
            }
        }
    }

    fn enter_step(&mut self, host: &dyn PaneSessionHost) {
        self.entered = self.env.now();
        self.still_since = self.entered;
        self.seen_len = self.env.output_len(host.shelld_session_id());
        self.drew = false;
        if let Some(s) = self.op.steps.get(self.at) {
            host.log(
                LogLevel::Info,
                &format!("{}.step", self.op.name),
                &format!("{}/{} {}", self.at + 1, self.op.steps.len(), s.label),
            );
        }
    }

    fn advance(&mut self, host: &dyn PaneSessionHost) {
        self.at += 1;
        if self.at >= self.op.steps.len() {
            self.finish(host, OpOutcome::Done);
        } else {
            self.enter_step(host);
        }
    }

    /// One step's worth of work.  `true` = this step is done.
    fn run_step(&mut self, host: &dyn PaneSessionHost, elapsed: Duration) -> bool {
        let sid = host.shelld_session_id();
        let Some(step) = self.op.steps.get(self.at).cloned() else {
            return true;
        };
        match step.kind {
            StepKind::Settle => elapsed >= step.timeout.unwrap_or_default(),
            StepKind::Terminate { pid, signal, escalate, sent } => {
                if !sent {
                    self.env.signal(pid, signal);
                    if let Some(StepKind::Terminate { sent, .. }) =
                        self.op.steps.get_mut(self.at).map(|s| &mut s.kind)
                    {
                        *sent = true;
                    }
                }
                if !self.env.pid_alive(pid) {
                    return true;
                }
                if let Some((after, sig)) = escalate {
                    if elapsed >= after {
                        self.env.signal(pid, sig);
                    }
                }
                false
            }
            StepKind::AwaitProcess { under, matching } => {
                self.env.find_descendant(under, matching).is_some()
            }
            StepKind::StopIfProcess { under, matching } => {
                if self.env.find_descendant(under, matching).is_some() {
                    host.log(
                        LogLevel::Info,
                        &format!("{}.already_done", self.op.name),
                        "the process is already there; nothing left to do",
                    );
                    self.finish(host, OpOutcome::Done);
                }
                true
            }
            StepKind::AwaitUser => false, // only `wake()` moves this on
            StepKind::Paste(text) => {
                if text.len() > MAX_PAYLOAD {
                    self.finish(
                        host,
                        OpOutcome::Failed {
                            step: self.at,
                            label: step.label,
                            err: format!("{} B exceeds the {MAX_PAYLOAD} B cap", text.len()),
                        },
                    );
                    return true;
                }
                host.log(
                    LogLevel::Info,
                    &format!("{}.paste", self.op.name),
                    &format!("sid={sid} bytes={}", text.len()),
                );
                if let Err(e) = self.env.io().paste(sid, &text) {
                    self.finish(
                        host,
                        OpOutcome::Failed { step: self.at, label: step.label, err: e.to_string() },
                    );
                }
                true
            }
            StepKind::Send(bytes) => {
                if bytes.len() > MAX_PAYLOAD {
                    self.finish(
                        host,
                        OpOutcome::Failed {
                            step: self.at,
                            label: step.label,
                            err: format!("{} B exceeds the {MAX_PAYLOAD} B cap", bytes.len()),
                        },
                    );
                    return true;
                }
                if let Err(e) = self.env.io().send(sid, &bytes) {
                    self.finish(
                        host,
                        OpOutcome::Failed { step: self.at, label: step.label, err: e.to_string() },
                    );
                }
                true
            }
            StepKind::AwaitQuiet { still } => {
                let len = self.env.output_len(sid);
                if len != self.seen_len {
                    self.seen_len = len;
                    self.still_since = self.env.now();
                    self.drew = true;
                }
                let quiet = self
                    .env
                    .now()
                    .duration_since(self.still_since)
                    .unwrap_or_default();
                self.drew && quiet >= still
            }
        }
    }

    /// The user is back — moves an `AwaitUser` step on.  Returns true
    /// if that is what it did.
    pub fn wake(&mut self, host: &dyn PaneSessionHost, trigger: &str) -> bool {
        if !matches!(
            self.op.steps.get(self.at).map(|s| &s.kind),
            Some(StepKind::AwaitUser)
        ) {
            return false;
        }
        host.log(
            LogLevel::Info,
            &format!("{}.woken", self.op.name),
            &format!("by {trigger}"),
        );
        self.advance(host);
        true
    }

    /// Is the run parked on its `AwaitUser` step?
    pub fn is_awaiting_user(&self) -> bool {
        matches!(
            self.op.steps.get(self.at).map(|s| &s.kind),
            Some(StepKind::AwaitUser)
        )
    }
}

impl PaneSession for OpRunner {
    fn caps(&self) -> u32 {
        let mut caps = marspot::shell_proto::PANE_SESSION_CAP_INPUT;
        if self.op.lock_keys {
            caps |= marspot::shell_proto::PANE_SESSION_CAP_LOCK_KEYS;
        }
        if self.op.hold_screen {
            // Belt to L3's braces: the core-side freeze covers the
            // window between this session starting and the hold
            // reaching L3, and costs nothing when the hold is there.
            caps |= marspot::shell_proto::PANE_SESSION_CAP_FREEZE_GRID;
        }
        caps
    }

    fn on_focus(&mut self, host: &dyn PaneSessionHost) {
        self.wake(host, "focus");
    }

    fn on_user_key(
        &mut self,
        host: &dyn PaneSessionHost,
        ev: &marspot::shell_proto::WireKeyEvent,
    ) -> KeyHandling {
        // A key at the parked step means the same as focus: the user
        // wants this pane back.  Swallowed either way — it was typed
        // at whatever is about to be replaced.
        if self.wake(host, "keypress") {
            return KeyHandling::Swallow;
        }
        let escape = matches!(ev.kind, marspot::shell_proto::WireLogicalKind::Named)
            && ev.key_data == marspot::shell_proto::WireNamedKey::Escape as u32;
        if escape && self.op.escape_hatch {
            self.finish(host, OpOutcome::Cancelled("escape"));
            return KeyHandling::EndSession;
        }
        KeyHandling::Swallow
    }

    fn on_tick(&mut self, host: &dyn PaneSessionHost) {
        self.spin_phase = self.spin_phase.wrapping_add(1);
        if self.finished.is_some() {
            host.end();
            return;
        }
        if !self.started {
            // First tick: take the hold before anything else runs, and
            // log where we are starting.
            self.started = true;
            if self.op.hold_screen {
                if let Err(e) = self.env.io().hold(host.shelld_session_id(), true) {
                    host.log(
                        LogLevel::Warn,
                        &format!("{}.hold_failed", self.op.name),
                        &format!("{e}"),
                    );
                }
            }
            self.enter_step(host);
        }
        if let Some(b) = &self.op.badge {
            host.set_badge(&format!("{b} {}", spinner_frame(self.spin_phase)));
        }
        let elapsed = self
            .env
            .now()
            .duration_since(self.entered)
            .unwrap_or_default();
        if self.run_step(host, elapsed) {
            self.advance(host);
            return;
        }
        if let Some(step) = self.op.steps.get(self.at) {
            if let Some(limit) = step.timeout {
                if elapsed >= limit {
                    self.finish(
                        host,
                        OpOutcome::TimedOut { step: self.at, label: step.label },
                    );
                }
            }
        }
    }

    fn on_end(&mut self, host: &dyn PaneSessionHost, reason: EndReason) {
        if self.finished.is_none() {
            self.finish(host, OpOutcome::Cancelled(match reason {
                EndReason::UserEscape => "escape",
                EndReason::PaneClosed => "pane_closed",
                EndReason::Timeout => "host_timeout",
                EndReason::PluginRequested => "plugin",
            }));
        }
        self.cleanup(host);
        let outcome = self.finished.clone().unwrap_or(OpOutcome::Done);
        if let Some(f) = self.on_finish.as_mut() {
            f(host, &outcome);
        }
    }
}

/// A shell command line built from parts that are checked, not hoped
/// about.
///
/// The hand-written version of this was `format!("CLAUDE_CONFIG_DIR='{}'
/// claude --resume {}\r", dir, uuid)` with a separate "is this dir
/// safe?" predicate that the caller had to remember to call.  Here the
/// check is the constructor: an unsafe value cannot reach the line.
pub struct PtyCommand {
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    clear_first: bool,
}

impl PtyCommand {
    pub fn new(program: impl Into<String>) -> Self {
        Self { program: program.into(), args: Vec::new(), env: Vec::new(), clear_first: false }
    }

    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }

    /// `KEY='value'` in front of the command.  A value that could break
    /// out of its quoting is **refused**, not escaped: a path that
    /// strange is not one to guess about, and the caller gets `None`
    /// rather than a line that does something else.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Wipe the screen as the line's first act.
    ///
    /// The typed line is echoed by the shell; behind a screen hold
    /// nobody watches that happen, but it is still there when the hold
    /// lifts.  Erasing the display first leaves the pane showing only
    /// what the command itself paints.  `\033[2J` and nothing else —
    /// `\033[3J` would take the scrollback with it, and the scrollback
    /// is the user's.
    pub fn clear_screen_first(mut self, on: bool) -> Self {
        self.clear_first = on;
        self
    }

    /// The bytes to type, or `None` if any part is unsafe to quote.
    pub fn to_bytes(&self) -> Option<Vec<u8>> {
        let mut line = String::new();
        if self.clear_first {
            line.push_str("printf '\\033[H\\033[2J'; ");
        }
        for (k, v) in &self.env {
            if !shell_safe(k) || !shell_safe(v) {
                return None;
            }
            line.push_str(&format!("{k}='{v}' "));
        }
        if !shell_safe(&self.program) {
            return None;
        }
        line.push_str(&self.program);
        for a in &self.args {
            if !shell_safe(a) {
                return None;
            }
            line.push(' ');
            line.push_str(a);
        }
        line.push('\r');
        Some(line.into_bytes())
    }
}

/// A value that can go on a command line without changing its meaning.
///
/// Rejects rather than escapes, and rejects the whole class: quotes,
/// newlines, backslashes, and the three shells' expansion characters.
pub fn shell_safe(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 512
        && !s.contains(['\'', '"', '\n', '\r', '\\', '$', '`', ';', '&', '|', '<', '>'])
}

/// The production [`OpEnv`]: real signals, the real process table, the
/// real clock, and the pane's own bytelog as the output counter.
pub struct RealEnv {
    io: Arc<dyn PtyIo>,
}

impl RealEnv {
    pub fn new(io: Arc<dyn PtyIo>) -> Self {
        Self { io }
    }
}

impl OpEnv for RealEnv {
    fn io(&self) -> &Arc<dyn PtyIo> {
        &self.io
    }
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
    fn signal(&self, pid: i32, sig: i32) {
        if pid > 0 {
            unsafe { libc::kill(pid, sig) };
        }
    }
    fn pid_alive(&self, pid: i32) -> bool {
        if pid <= 0 {
            return false;
        }
        unsafe {
            if libc::kill(pid, 0) == 0 {
                return true;
            }
            *libc::__error() != libc::ESRCH
        }
    }
    fn find_descendant(&self, under: i32, pred: fn(&pidtree::ProcRow) -> bool) -> Option<i32> {
        if under <= 0 {
            return None;
        }
        let procs = pidtree::list_all_procs();
        pidtree::descendants_of(under, &procs)
            .iter()
            .find(|r| pred(r))
            .map(|r| r.pid)
    }
    fn output_len(&self, sid: u64) -> u64 {
        // L3 appends every byte the PTY produces to the session's
        // bytelog, so its length is the cheapest "has anything been
        // drawn" counter there is — one `stat`, and only while an op
        // is running.
        marspot_term::paths::sessions_dir()
            .join(sid.to_string())
            .join("bytelog")
            .metadata()
            .map(|m| m.len())
            .unwrap_or(0)
    }
}

// ── the service ──────────────────────────────────────────────────────

/// Identifies one submitted run, so a caller can recognise its own
/// outcome later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct OpId(pub u64);

/// What a finished run reports back.
#[derive(Debug, Clone)]
pub struct OpReport {
    pub id: OpId,
    pub sid: u64,
    pub name: &'static str,
    pub outcome: OpOutcome,
}

/// What the queue needs from whoever owns it.
///
/// Deliberately not `PluginHost`: that trait's `begin_pane_session`
/// checks the *current plugin's* permissions, and the queue has
/// submitters that are not plugins.  Routing through it denied the
/// first CLI delivery with `missing permission: PermissionSet(16)` —
/// the supervisor asking itself for permission it has no identity to
/// hold.  Plugins still submit through their host; starting the run is
/// the owner's business.
pub trait OpHost {
    fn begin(&self, sid: u64, session: Box<dyn PaneSession>) -> Result<(), String>;
    fn log(&self, level: LogLevel, tag: &str, msg: &str);
}

/// Any plugin host is an op host — that is how a plugin's own tests
/// (and the reclamation path) drive the queue without a supervisor.
impl<T: crate::plugins::PluginHost + ?Sized> OpHost for T {
    fn begin(&self, sid: u64, session: Box<dyn PaneSession>) -> Result<(), String> {
        crate::plugins::PluginHost::begin_pane_session(self, sid, session)
            .map_err(|e| e.to_string())
    }
    fn log(&self, level: LogLevel, tag: &str, msg: &str) {
        crate::plugins::PluginHost::log(self, level, tag, msg)
    }
}

/// Most runs that may wait their turn on one pane.
///
/// A pane is a single-threaded thing — one program, one keyboard — so
/// the queue exists to serialise, not to buffer.  Anything past this is
/// a caller in a loop, and dropping the newest with a loud line is a
/// better failure than growing forever (CLAUDE.md §3).
const MAX_QUEUED_PER_PANE: usize = 8;

/// The entry point for "do something to a pane".
///
/// Everything goes through here rather than each caller reaching for
/// `begin_pane_session` itself, because the invariant that matters
/// cannot be enforced anywhere else: **one run at a time per pane**.
/// Two scripts typing into the same PTY interleave their keystrokes,
/// and the result is neither of the commands they meant to send.
///
/// That invariant is the reason this exists now rather than when there
/// is a second caller: session-to-session delivery means ops aimed at
/// panes their sender does not own, arriving whenever the sender feels
/// like it, at panes that may already be mid-reclamation.
pub struct PtyOps {
    io: Arc<dyn PtyIo>,
    queued: std::collections::HashMap<u64, std::collections::VecDeque<(OpId, PtyOp, usize)>>,
    active: std::collections::HashMap<u64, (OpId, &'static str)>,
    /// Where runners post their outcomes; drained by `pump`.
    sink: Arc<std::sync::Mutex<Vec<OpReport>>>,
    next_id: u64,
}

impl PtyOps {
    pub fn new(io: Arc<dyn PtyIo>) -> Self {
        Self {
            io,
            queued: std::collections::HashMap::new(),
            active: std::collections::HashMap::new(),
            sink: Arc::new(std::sync::Mutex::new(Vec::new())),
            next_id: 1,
        }
    }

    /// Queue `op` against `sid`.  Runs immediately if the pane is free.
    ///
    /// `None` when the pane's queue is full — the caller is told, and
    /// can decide whether losing this one matters.
    pub fn submit(&mut self, sid: u64, op: PtyOp) -> Option<OpId> {
        self.submit_at(sid, op, 0)
    }

    /// Queue a run that starts partway in — how a parked script is
    /// re-armed after the process that owned it was replaced.
    pub fn submit_at(&mut self, sid: u64, op: PtyOp, step: usize) -> Option<OpId> {
        let q = self.queued.entry(sid).or_default();
        if q.len() >= MAX_QUEUED_PER_PANE {
            return None;
        }
        let id = OpId(self.next_id);
        self.next_id += 1;
        q.push_back((id, op, step));
        Some(id)
    }

    /// Is a run in flight on this pane?
    pub fn is_busy(&self, sid: u64) -> bool {
        self.active.contains_key(&sid)
    }

    /// What is running on this pane, if anything.
    pub fn running(&self, sid: u64) -> Option<&'static str> {
        self.active.get(&sid).map(|(_, name)| *name)
    }

    /// Post an outcome as a runner would.  Test-only: exercising the
    /// queue's hand-off should not require driving a real script to
    /// completion.
    #[cfg(test)]
    pub fn report_for_test(&self, r: OpReport) {
        self.sink.lock().unwrap().push(r);
    }

    /// Start whatever can start, and collect whatever finished.
    ///
    /// Called once per plugin tick.  Returns the finished runs so the
    /// caller can do its own bookkeeping — this module has no opinion
    /// about what a completed op means.
    pub fn pump(&mut self, host: &dyn OpHost) -> Vec<OpReport> {
        let done: Vec<OpReport> = std::mem::take(&mut *self.sink.lock().unwrap());
        for r in &done {
            // Only clear the slot if the report belongs to the run that
            // holds it: a late report from a superseded run must not
            // free a pane someone else is using.
            if self.active.get(&r.sid).map(|(id, _)| *id) == Some(r.id) {
                self.active.remove(&r.sid);
            }
        }
        let mut free: Vec<u64> = self
            .queued
            .iter()
            .filter(|(sid, q)| !q.is_empty() && !self.active.contains_key(sid))
            .map(|(sid, _)| *sid)
            .collect();
        // Sorted, because the map's order is not one: two panes going
        // first in a different order on every tick would make the logs
        // — and any test of them — read as noise.
        free.sort_unstable();
        for sid in free {
            let Some((id, op, step)) = self.queued.get_mut(&sid).and_then(|q| q.pop_front()) else {
                continue;
            };
            let name = op.name;
            let sink = Arc::clone(&self.sink);
            let runner = OpRunner::new(op, Box::new(RealEnv::new(Arc::clone(&self.io))))
                .start_at(step)
                .on_finish(move |_, outcome| {
                    sink.lock().unwrap().push(OpReport {
                        id,
                        sid,
                        name,
                        outcome: outcome.clone(),
                    });
                });
            match host.begin(sid, Box::new(runner)) {
                Ok(()) => {
                    self.active.insert(sid, (id, name));
                    host.log(
                        LogLevel::Info,
                        "pty_op.started",
                        &format!("{name} on pane {sid} (id={})", id.0),
                    );
                }
                Err(e) => {
                    host.log(
                        LogLevel::Warn,
                        "pty_op.begin_failed",
                        &format!("{name} on pane {sid}: {e}"),
                    );
                    self.sink.lock().unwrap().push(OpReport {
                        id,
                        sid,
                        name,
                        outcome: OpOutcome::Failed { step: 0, label: "begin", err: e },
                    });
                }
            }
        }
        // Panes with nothing left to run are forgotten, so the maps are
        // bounded by what is actually in flight.
        self.queued.retain(|_, q| !q.is_empty());
        done
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A world the test owns: a clock it advances by hand, a process
    /// table it edits, and a record of everything the op did.
    struct FakeEnv {
        io: Arc<dyn PtyIo>,
        state: Arc<FakeState>,
    }

    /// One pane runs one script at a time; the rest wait their turn.
    ///
    /// This is the invariant the service exists for.  Two scripts
    /// typing into the same PTY interleave their keystrokes, and what
    /// arrives is neither command — a failure that only shows up once
    /// something submits ops it does not own, which is exactly what
    /// session-to-session delivery is.
    #[test]
    fn one_run_at_a_time_per_pane_and_the_rest_queue() {
        let state = Arc::new(FakeState::default());
        let mut ops = PtyOps::new(Arc::new(FakeIo(Arc::clone(&state))) as Arc<dyn PtyIo>);
        let host = FakePluginHost::default();

        let first = ops.submit(7, PtyOp::new("test.a").step(Step::settle(Duration::from_secs(1))));
        let second = ops.submit(7, PtyOp::new("test.b").step(Step::settle(Duration::from_secs(1))));
        let other_pane = ops.submit(9, PtyOp::new("test.c").step(Step::settle(Duration::from_secs(1))));
        assert!(first.is_some() && second.is_some() && other_pane.is_some());

        ops.pump(&host);
        assert_eq!(
            *host.begun.lock().unwrap(),
            vec![(7, "test.a".to_string()), (9, "test.c".to_string())],
            "one per pane starts; a different pane is not blocked by it"
        );
        assert_eq!(ops.running(7), Some("test.a"));

        ops.pump(&host);
        assert_eq!(
            host.begun.lock().unwrap().len(),
            2,
            "the queued run must not start while the first is in flight"
        );

        // The first finishes; the queued one takes the pane.
        ops.report_for_test(OpReport {
            id: first.unwrap(),
            sid: 7,
            name: "test.a",
            outcome: OpOutcome::Done,
        });
        let done = ops.pump(&host);
        assert_eq!(done.len(), 1, "the finished run is reported once");
        assert_eq!(ops.running(7), Some("test.b"));
    }

    /// A queue is for serialising, not for buffering.
    #[test]
    fn a_pane_queue_is_bounded() {
        let state = Arc::new(FakeState::default());
        let mut ops = PtyOps::new(Arc::new(FakeIo(Arc::clone(&state))) as Arc<dyn PtyIo>);
        for _ in 0..MAX_QUEUED_PER_PANE {
            assert!(ops.submit(1, PtyOp::new("test.x")).is_some());
        }
        assert!(
            ops.submit(1, PtyOp::new("test.x")).is_none(),
            "past the cap the caller is told, not silently queued forever"
        );
    }

    /// A late report from a superseded run must not free a pane that
    /// someone else has since taken.
    #[test]
    fn a_stale_report_does_not_free_someone_elses_pane() {
        let state = Arc::new(FakeState::default());
        let mut ops = PtyOps::new(Arc::new(FakeIo(Arc::clone(&state))) as Arc<dyn PtyIo>);
        let host = FakePluginHost::default();
        let a = ops.submit(3, PtyOp::new("test.a")).unwrap();
        ops.pump(&host);
        ops.report_for_test(OpReport { id: a, sid: 3, name: "test.a", outcome: OpOutcome::Done });
        ops.pump(&host);
        let b = ops.submit(3, PtyOp::new("test.b")).unwrap();
        ops.pump(&host);
        assert_eq!(ops.running(3), Some("test.b"));
        // `a`'s report arriving again (a duplicate, a retry) must not
        // release the pane `b` is using.
        ops.report_for_test(OpReport { id: a, sid: 3, name: "test.a", outcome: OpOutcome::Done });
        ops.pump(&host);
        assert_eq!(ops.running(3), Some("test.b"), "still b's pane");
        assert_ne!(a, b);
    }

    /// Multi-line text goes as a paste, and oversized payloads are
    /// refused rather than typed.
    #[test]
    fn text_is_delivered_as_a_paste_and_is_size_capped() {
        let (state, env, host) = setup();
        let op = PtyOp::new("test.msg").step(Step::paste("first line\nsecond line"));
        let mut r = OpRunner::new(op, env);
        run(&mut r, &host, &state, 100);
        assert_eq!(
            *state.pasted.lock().unwrap(),
            vec!["first line\nsecond line".to_string()],
            "text with newlines must not be typed a line at a time"
        );
        assert!(state.sent.lock().unwrap().is_empty(), "and not as raw input");

        let (state2, env2, host2) = setup();
        let huge = "x".repeat(MAX_PAYLOAD + 1);
        let finished: Arc<Mutex<Option<OpOutcome>>> = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&finished);
        let mut r = OpRunner::new(PtyOp::new("test.huge").step(Step::paste(huge)), env2)
            .on_finish(move |_, o| *sink.lock().unwrap() = Some(o.clone()));
        run(&mut r, &host2, &state2, 100);
        r.on_end(&host2, EndReason::PluginRequested);
        assert!(state2.pasted.lock().unwrap().is_empty(), "nothing that big is delivered");
        assert!(
            matches!(finished.lock().unwrap().as_ref(), Some(OpOutcome::Failed { .. })),
            "and the caller is told why"
        );
    }

    /// Records which pane got a session, in order.  The op's name is
    /// read out of the log line the service writes, so the test sees
    /// what an operator would.
    #[derive(Default)]
    struct FakePluginHost {
        begun: Mutex<Vec<(u64, String)>>,
    }

    impl crate::plugins::PluginHost for FakePluginHost {
        fn pane_count(&self) -> usize {
            0
        }
        fn pane_pty_device(
            &self,
            _pane: usize,
        ) -> Result<Option<std::path::PathBuf>, crate::plugins::PluginError> {
            Ok(None)
        }
        fn pane_pty_pid_tree(
            &self,
            _pane: usize,
        ) -> Result<Vec<crate::plugins::PtyChild>, crate::plugins::PluginError> {
            Ok(Vec::new())
        }
        fn pane_focused(&self) -> Option<usize> {
            None
        }
        fn state_dir(&self) -> Result<std::path::PathBuf, crate::plugins::PluginError> {
            Ok(std::env::temp_dir())
        }
        fn log(&self, _l: LogLevel, tag: &str, msg: &str) {
            if tag == "pty_op.started" {
                // "<name> on pane <sid> (id=N)"
                let name = msg.split_whitespace().next().unwrap_or("?").to_string();
                let sid = msg
                    .split("on pane ")
                    .nth(1)
                    .and_then(|r| r.split_whitespace().next())
                    .and_then(|n| n.parse::<u64>().ok())
                    .unwrap_or(0);
                self.begun.lock().unwrap().push((sid, name));
            }
        }
        fn begin_pane_session(
            &self,
            _sid: u64,
            _session: Box<dyn PaneSession>,
        ) -> Result<(), crate::plugins::PluginError> {
            Ok(())
        }
    }

    /// The queue must not need a plugin's permissions to start a run.
    ///
    /// This shipped: `pump` went through `PluginHost::begin_pane_session`,
    /// which checks the *current plugin's* permissions — so the very
    /// first CLI delivery was refused by the supervisor's own gate with
    /// `missing permission`, and the text never reached the pane.  The
    /// queue's owner is the authority; starting a run is its business.
    #[test]
    fn the_queue_starts_runs_through_its_owner_not_a_plugin_gate() {
        struct Owner {
            started: Mutex<Vec<u64>>,
        }
        impl OpHost for Owner {
            fn begin(&self, sid: u64, _s: Box<dyn PaneSession>) -> Result<(), String> {
                self.started.lock().unwrap().push(sid);
                Ok(())
            }
            fn log(&self, _l: LogLevel, _t: &str, _m: &str) {}
        }
        let state = Arc::new(FakeState::default());
        let mut ops = PtyOps::new(Arc::new(FakeIo(Arc::clone(&state))) as Arc<dyn PtyIo>);
        let owner = Owner { started: Mutex::new(Vec::new()) };
        ops.submit(390, PtyOp::new("cli.send").step(Step::paste("hi")));
        ops.pump(&owner);
        assert_eq!(*owner.started.lock().unwrap(), vec![390]);
        assert!(ops.is_busy(390));
    }

    #[derive(Default)]
    struct FakeState {
        now_ms: Mutex<u64>,
        alive: Mutex<Vec<i32>>,
        present: Mutex<Vec<i32>>,
        out_len: Mutex<u64>,
        signals: Mutex<Vec<(i32, i32)>>,
        sent: Mutex<Vec<Vec<u8>>>,
        holds: Mutex<Vec<bool>>,
        pasted: Mutex<Vec<String>>,
    }

    struct FakeIo(Arc<FakeState>);
    impl PtyIo for FakeIo {
        fn send(&self, _sid: u64, bytes: &[u8]) -> std::io::Result<()> {
            self.0.sent.lock().unwrap().push(bytes.to_vec());
            Ok(())
        }
        fn hold(&self, _sid: u64, on: bool) -> std::io::Result<()> {
            self.0.holds.lock().unwrap().push(on);
            Ok(())
        }
        fn paste(&self, _sid: u64, text: &str) -> std::io::Result<()> {
            self.0.pasted.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    impl OpEnv for FakeEnv {
        fn io(&self) -> &Arc<dyn PtyIo> {
            &self.io
        }
        fn now(&self) -> SystemTime {
            SystemTime::UNIX_EPOCH + Duration::from_millis(*self.state.now_ms.lock().unwrap())
        }
        fn signal(&self, pid: i32, sig: i32) {
            self.state.signals.lock().unwrap().push((pid, sig));
            if sig == libc::SIGKILL {
                self.state.alive.lock().unwrap().retain(|p| *p != pid);
            }
        }
        fn pid_alive(&self, pid: i32) -> bool {
            self.state.alive.lock().unwrap().contains(&pid)
        }
        fn find_descendant(&self, _under: i32, _pred: fn(&pidtree::ProcRow) -> bool) -> Option<i32> {
            self.state.present.lock().unwrap().first().copied()
        }
        fn output_len(&self, _sid: u64) -> u64 {
            *self.state.out_len.lock().unwrap()
        }
    }

    struct Host {
        ended: Mutex<bool>,
        badges: Mutex<Vec<String>>,
    }
    impl PaneSessionHost for Host {
        fn shelld_session_id(&self) -> u64 {
            1
        }
        fn end(&self) {
            *self.ended.lock().unwrap() = true;
        }
        fn set_badge(&self, t: &str) {
            self.badges.lock().unwrap().push(t.into());
        }
        fn set_pane_title(&self, _t: &str) {}
        fn log(&self, _l: LogLevel, _t: &str, _m: &str) {}
    }

    fn setup() -> (Arc<FakeState>, Box<dyn OpEnv>, Host) {
        let state = Arc::new(FakeState::default());
        let env = FakeEnv {
            io: Arc::new(FakeIo(Arc::clone(&state))) as Arc<dyn PtyIo>,
            state: Arc::clone(&state),
        };
        let host = Host { ended: Mutex::new(false), badges: Mutex::new(Vec::new()) };
        (state, Box::new(env), host)
    }

    fn advance(state: &FakeState, ms: u64) {
        *state.now_ms.lock().unwrap() += ms;
    }

    /// Ticks the runner the way the host does, advancing the clock a
    /// frame at a time.  Returns when the run ends or the budget runs
    /// out.
    fn run(r: &mut OpRunner, host: &Host, state: &FakeState, ms: u64) {
        for _ in 0..(ms / 16) {
            if *host.ended.lock().unwrap() {
                return;
            }
            r.on_tick(host);
            advance(state, 16);
        }
    }

    /// The reclamation script, end to end, in the world the test
    /// controls: hold → settle → TERM → wait for it to die → park →
    /// user comes back → type → wait for the process → wait for it to
    /// finish drawing → release.
    #[test]
    fn a_park_and_wake_script_runs_in_order() {
        let (state, env, host) = setup();
        state.alive.lock().unwrap().push(4242);
        let op = PtyOp::new("test.park")
            .hold_screen(true)
            .badge("zZ")
            .step(Step::settle(Duration::from_millis(250)))
            .step(
                Step::terminate(4242, libc::SIGTERM)
                    .escalate_after(Duration::from_secs(3), libc::SIGKILL),
            )
            .step(Step::await_user())
            .step(Step::send(b"resume\r".to_vec()))
            .step(Step::await_process(1, |_| true))
            .step(Step::await_quiet(Duration::from_millis(500)));
        let mut r = OpRunner::new(op, env);

        // The hold is taken before anything else happens — the whole
        // point is that the kill never reaches the screen.
        r.on_tick(&host);
        assert_eq!(*state.holds.lock().unwrap(), vec![true]);
        assert!(state.signals.lock().unwrap().is_empty(), "settle first");

        run(&mut r, &host, &state, 300);
        assert_eq!(*state.signals.lock().unwrap(), vec![(4242, libc::SIGTERM)]);

        // Parked: it waits for the user however long that takes.
        state.alive.lock().unwrap().clear();
        run(&mut r, &host, &state, 200);
        assert!(r.is_awaiting_user(), "the script parks here");
        advance(&state, 3_600_000);
        run(&mut r, &host, &state, 100);
        assert!(r.is_awaiting_user(), "an hour later, still parked");
        assert!(state.sent.lock().unwrap().is_empty(), "nothing typed while parked");

        // The user comes back.
        r.on_focus(&host);
        run(&mut r, &host, &state, 100);
        assert_eq!(state.sent.lock().unwrap().len(), 1, "the line goes out");
        assert_eq!(state.sent.lock().unwrap()[0], b"resume\r");

        // Still waiting: the process is not there yet.
        run(&mut r, &host, &state, 200);
        assert!(!*host.ended.lock().unwrap());
        state.present.lock().unwrap().push(99);
        // …and once it is, the run waits for it to draw and go still.
        run(&mut r, &host, &state, 100);
        *state.out_len.lock().unwrap() = 4096;
        run(&mut r, &host, &state, 100);
        assert!(!*host.ended.lock().unwrap(), "still drawing");
        run(&mut r, &host, &state, 600);
        assert!(*host.ended.lock().unwrap(), "quiet for long enough — done");

        r.on_end(&host, EndReason::PluginRequested);
        assert_eq!(*state.holds.lock().unwrap(), vec![true, false], "released exactly once");
    }

    /// A run that exists to bring something back must not do it twice.
    ///
    /// The gap between parking a session and typing it back holds the
    /// user's own click, and in that gap the session can return by
    /// other means — a second wake armed on the same pane, or the user
    /// starting it themselves.  Typing then puts `claude --resume …`
    /// into the running session's prompt, as text they have to delete.
    /// Seen on the real machine twice in one day, which is what this
    /// step is for.
    #[test]
    fn a_run_stops_when_what_it_was_arranging_has_already_happened() {
        let (state, env, host) = setup();
        let op = PtyOp::new("test.wake")
            .step(Step::await_user())
            .step(Step::stop_if_process(1, |_| true))
            .step(Step::send(b"resume\r".to_vec()));
        let mut r = OpRunner::new(op, env);
        run(&mut r, &host, &state, 100);
        assert!(r.is_awaiting_user());

        // It came back on its own while we were parked.
        state.present.lock().unwrap().push(99);
        r.on_focus(&host);
        run(&mut r, &host, &state, 100);
        assert!(
            state.sent.lock().unwrap().is_empty(),
            "nothing to type — it is already back"
        );
        assert!(*host.ended.lock().unwrap(), "and the run is over, successfully");
    }

    /// …and when it has not, the run carries on as before.
    #[test]
    fn a_run_carries_on_when_nothing_has_come_back() {
        let (state, env, host) = setup();
        let op = PtyOp::new("test.wake")
            .step(Step::await_user())
            .step(Step::stop_if_process(1, |_| true))
            .step(Step::send(b"resume\r".to_vec()));
        let mut r = OpRunner::new(op, env);
        run(&mut r, &host, &state, 100);
        r.on_focus(&host);
        run(&mut r, &host, &state, 100);
        assert_eq!(*state.sent.lock().unwrap(), vec![b"resume\r".to_vec()]);
    }

    /// A signal that doesn't take gets escalated, and the wait that
    /// follows is the one that notices.
    #[test]
    fn a_stubborn_process_gets_escalated() {
        let (state, env, host) = setup();
        state.alive.lock().unwrap().push(7);
        let op = PtyOp::new("test.kill").step(
            Step::terminate(7, libc::SIGTERM)
                .escalate_after(Duration::from_secs(3), libc::SIGKILL)
                .timeout(Duration::from_secs(10)),
        );
        let mut r = OpRunner::new(op, env);
        run(&mut r, &host, &state, 1_000);
        assert_eq!(*state.signals.lock().unwrap(), vec![(7, libc::SIGTERM)], "polite first");
        run(&mut r, &host, &state, 3_000);
        assert!(
            state.signals.lock().unwrap().contains(&(7, libc::SIGKILL)),
            "then the deadline"
        );
        assert!(*host.ended.lock().unwrap(), "and the wait completes once it is gone");
    }

    /// Every wait has a deadline, and blowing it is reported rather
    /// than hung on.
    #[test]
    fn a_step_that_never_completes_times_out() {
        let (state, env, host) = setup();
        state.alive.lock().unwrap().push(7);
        let finished: Arc<Mutex<Option<OpOutcome>>> = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&finished);
        let op = PtyOp::new("test.stuck").hold_screen(true).step(
            Step::terminate(7, libc::SIGTERM).timeout(Duration::from_secs(2)),
        );
        let mut r = OpRunner::new(op, env)
            .on_finish(move |_, o| *sink.lock().unwrap() = Some(o.clone()));
        run(&mut r, &host, &state, 5_000);
        r.on_end(&host, EndReason::PluginRequested);
        assert_eq!(
            *finished.lock().unwrap(),
            Some(OpOutcome::TimedOut { step: 0, label: "terminate" })
        );
        assert_eq!(
            *state.holds.lock().unwrap(),
            vec![true, false],
            "a timed-out op still releases the pane"
        );
    }

    /// Esc gets the pane back, and the cleanup still runs.
    #[test]
    fn escape_ends_the_run_and_releases_the_pane() {
        let (state, env, host) = setup();
        let op = PtyOp::new("test.esc")
            .hold_screen(true)
            .step(Step::settle(Duration::from_secs(30)));
        let mut r = OpRunner::new(op, env);
        r.on_tick(&host);
        let esc = marspot::shell_proto::WireKeyEvent {
            state: marspot::shell_proto::WireKeyState::Pressed,
            mods: 0,
            kind: marspot::shell_proto::WireLogicalKind::Named,
            key_data: marspot::shell_proto::WireNamedKey::Escape as u32,
            text: String::new(),
        };
        assert!(matches!(r.on_user_key(&host, &esc), KeyHandling::EndSession));
        r.on_end(&host, EndReason::UserEscape);
        assert_eq!(*state.holds.lock().unwrap(), vec![true, false]);
    }

    /// A value that could break out of its quoting is refused, not
    /// escaped — the caller gets nothing rather than a line that does
    /// something else.
    #[test]
    fn a_command_refuses_values_it_cannot_quote() {
        let ok = PtyCommand::new("claude")
            .env("CLAUDE_CONFIG_DIR", "/Users/x/.claude-profile-3")
            .arg("--resume")
            .arg("abc-123")
            .clear_screen_first(true)
            .to_bytes()
            .expect("safe parts build a line");
        assert_eq!(
            String::from_utf8(ok).unwrap(),
            "printf '\\033[H\\033[2J'; CLAUDE_CONFIG_DIR='/Users/x/.claude-profile-3' \
             claude --resume abc-123\r"
        );
        for bad in ["/tmp/a'; rm -rf ~; '", "/tmp/$(whoami)", "/tmp/`id`", "", "a\nb"] {
            assert!(
                PtyCommand::new("claude").env("D", bad).to_bytes().is_none(),
                "{bad:?} must be refused"
            );
        }
        // The wipe never takes the scrollback with it.
        let line = PtyCommand::new("claude").clear_screen_first(true).to_bytes().unwrap();
        assert!(!String::from_utf8(line).unwrap().contains("[3J"));
    }
}

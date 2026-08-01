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
    /// Signal `pid`, escalating if it is still there after a while.
    Signal { pid: i32, signal: i32, escalate: Option<(Duration, i32)> },
    /// Wait for `pid` to leave the process table.
    AwaitGone { pid: i32 },
    /// Wait for a descendant of `under` to match.
    AwaitProcess { under: i32, matching: fn(&pidtree::ProcRow) -> bool },
    /// Wait for the user to come back — focus or a keystroke.  The one
    /// step with no deadline: a parked pane may sit for days.
    AwaitUser,
    /// Type bytes into the pane's PTY.
    Send(Vec<u8>),
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
    pub fn signal(pid: i32, signal: i32) -> Self {
        Self {
            kind: StepKind::Signal { pid, signal, escalate: None },
            timeout: Some(Duration::from_secs(1)),
            label: "signal",
        }
    }
    /// Escalate to `sig` if the process is still alive after `after`.
    /// Applies to the following `AwaitGone`, which is where the waiting
    /// happens.
    pub fn escalate_after(mut self, after: Duration, sig: i32) -> Self {
        if let StepKind::Signal { escalate, .. } = &mut self.kind {
            *escalate = Some((after, sig));
        }
        self
    }
    pub fn await_gone(pid: i32) -> Self {
        Self { kind: StepKind::AwaitGone { pid }, timeout: Some(Duration::from_secs(10)), label: "await_gone" }
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
            StepKind::Signal { pid, signal, .. } => {
                self.env.signal(pid, signal);
                true
            }
            StepKind::AwaitGone { pid } => {
                if !self.env.pid_alive(pid) {
                    return true;
                }
                // The escalation belongs to the signal that preceded
                // this wait; look back one step for it rather than
                // making the caller repeat it.
                if let Some(Step { kind: StepKind::Signal { pid: p, escalate: Some((after, sig)), .. }, .. }) =
                    self.at.checked_sub(1).and_then(|i| self.op.steps.get(i))
                {
                    if elapsed >= *after {
                        self.env.signal(*p, *sig);
                    }
                }
                false
            }
            StepKind::AwaitProcess { under, matching } => {
                self.env.find_descendant(under, matching).is_some()
            }
            StepKind::AwaitUser => false, // only `wake()` moves this on
            StepKind::Send(bytes) => {
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

    #[derive(Default)]
    struct FakeState {
        now_ms: Mutex<u64>,
        alive: Mutex<Vec<i32>>,
        present: Mutex<Vec<i32>>,
        out_len: Mutex<u64>,
        signals: Mutex<Vec<(i32, i32)>>,
        sent: Mutex<Vec<Vec<u8>>>,
        holds: Mutex<Vec<bool>>,
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
            .step(Step::signal(4242, libc::SIGTERM).escalate_after(Duration::from_secs(3), libc::SIGKILL))
            .step(Step::await_gone(4242))
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

    /// A signal that doesn't take gets escalated, and the wait that
    /// follows is the one that notices.
    #[test]
    fn a_stubborn_process_gets_escalated() {
        let (state, env, host) = setup();
        state.alive.lock().unwrap().push(7);
        let op = PtyOp::new("test.kill")
            .step(Step::signal(7, libc::SIGTERM).escalate_after(Duration::from_secs(3), libc::SIGKILL))
            .step(Step::await_gone(7).timeout(Duration::from_secs(10)));
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
        let op = PtyOp::new("test.stuck")
            .hold_screen(true)
            .step(Step::await_gone(7).timeout(Duration::from_secs(2)));
        let mut r = OpRunner::new(op, env)
            .on_finish(move |_, o| *sink.lock().unwrap() = Some(o.clone()));
        run(&mut r, &host, &state, 5_000);
        r.on_end(&host, EndReason::PluginRequested);
        assert_eq!(
            *finished.lock().unwrap(),
            Some(OpOutcome::TimedOut { step: 0, label: "await_gone" })
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

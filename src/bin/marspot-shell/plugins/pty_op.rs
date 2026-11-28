// CARVE-OUT: pre-existing file: scripted pane operations (freeze, signal, retype, settle, watchdog) as one skeleton; shrink on every touch
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
use marspot::pidtree::{self, ProcId, SignalError};

pub use super::pty_command::{PtyCommand, quotable, shell_safe};

/// Everything a running op needs from the world.
///
/// One trait so a test can drive a whole script in microseconds with a
/// clock it controls — the alternative is sleeping through real
/// timeouts, which is how these paths went untested the first time.
pub trait OpEnv: Send {
    fn io(&self) -> &Arc<dyn PtyIo>;
    fn now(&self) -> SystemTime;
    /// Signal the process `id` names and no other; `Gone` when its pid
    /// now names another process or none.
    fn signal(&self, id: ProcId, sig: i32) -> Result<(), SignalError>;
    fn pid_alive(&self, id: ProcId) -> bool;
    /// A descendant of `under` matching `pred`, if any, wherever it runs.
    fn find_descendant(&self, under: i32, pred: fn(&pidtree::ProcRow) -> bool) -> Option<ProcId>;
    /// The program in front of shell `under`'s terminal, if it is a
    /// job of that shell and matches `pred`.
    fn find_in_front(&self, under: i32, pred: fn(&pidtree::ProcRow) -> bool) -> Option<ProcId>;
    /// Who holds shell `shell`'s terminal right now.
    fn front(&self, shell: i32) -> front::Front;
    /// Bytes this pane's PTY has produced, ever.  Monotone; only the
    /// differences matter.
    fn output_len(&self, sid: u64) -> u64;
    /// The last `bytes` the pane's PTY produced, as raw bytes.
    fn output_tail(&self, sid: u64, bytes: usize) -> Vec<u8>;
    /// `(published frame sequence, terminal mode flags)` for this
    /// pane, as the program running in it last declared them.
    ///
    /// The flags are the only honest answer to "is the thing in this
    /// pane ready to be typed at" — counting bytes and waiting for
    /// quiet is guessing at it, and guessed wrong in both directions
    /// (2026-09-29 too early, 2026-09-30 never).  `None` means the
    /// question could not be asked: no session dir, no region, or a
    /// region written by something else.
    fn pane_modes(&self, sid: u64) -> Option<(u64, u32)>;
    /// The pane's screen as text, the way `--read` shows it: the frame
    /// the session publishes, copied once.
    fn pane_screen(&self, sid: u64) -> Option<String>;
    /// What sits typed and unsent in the pane's input box, read off
    /// the grid so the program's own suggestion and the messages
    /// already sent are not taken for it.
    fn pane_unsent(&self, sid: u64) -> Option<String>;

    /// Did this pane's own agent report submitting this text?
    ///
    /// `Some(true)` is the only evidence here that is not a guess
    /// about another program's screen: it came from the program
    /// itself. `Some(false)` means a receipt was expected and has not
    /// arrived yet; `None` means this pane has no receipt channel at
    /// all -- no agent, no hook, an older one -- and the screen is
    /// still the only thing to read.
    ///
    /// Claims it, so one receipt answers one submit.
    fn submit_receipt(&self, _sid: u64, _text: &str) -> Option<bool> {
        None
    }
}

/// Is `needle` anywhere in `haystack`?
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
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
    /// Named by `ProcId`, so a pid recycled since the decision is never
    /// signalled: the step then ends as already gone.
    Terminate { target: ProcId, signal: i32, escalate: Option<(Duration, i32)>, sent: bool },
    /// Wait for the program in front of `under`'s terminal to match:
    /// the program started at the shell's prompt, not one that some
    /// other program in the pane started.
    AwaitProcess { under: i32, matching: fn(&pidtree::ProcRow) -> bool },
    /// Wait for `shell` to hold its own terminal again; the run fails,
    /// naming who holds it, if it does not by the deadline -- see
    /// [`front`].
    AwaitShell { shell: i32 },
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
    /// From here on the run depends on the program `matching`, in front
    /// of `under`'s terminal, staying up: if that process exits, the run fails at
    /// once instead of waiting out whatever step it is in.
    ///
    /// A resume the program refuses (`No conversation found`) exits
    /// within a second, and the steps after it -- waiting for the
    /// program to take the terminal, then to draw -- had nothing to
    /// wait for and held the screen frozen until their deadlines.
    WatchProcess { under: i32, matching: fn(&pidtree::ProcRow) -> bool },
    /// Wait for the user to come back — focus or a keystroke.  The one
    /// step with no deadline: a parked pane may sit for days.
    AwaitUser,
    /// Type bytes into the pane's PTY, verbatim.
    Send(Vec<u8>),
    /// Deliver text as a paste — mode-aware, and safe for text with
    /// newlines in it.  This is the one that carries a message to
    /// whatever is running in the pane.
    Paste(String),
    /// Wait until the pane has drawn at least `min_bytes` and then
    /// gone still for `still`.
    ///
    /// Measured from this step's own start, which is what makes it
    /// composable: put it after `AwaitProcess` and it means "wait for
    /// the thing that just appeared to finish drawing".
    ///
    /// `min_bytes` is what makes it *true*.  A program that has just
    /// started emits a trickle — mode sets, a cursor query, a screen
    /// clear — and then goes quiet for as long as it takes to load
    /// whatever it is resuming.  With a byte floor of zero, that pause
    /// reads as "finished drawing", and the freeze lifts onto the
    /// screen it just cleared: **the black flash**.  A first frame is
    /// tens of kilobytes; startup noise is a few hundred bytes.
    ///
    /// `or_composer` is for an agent that may start working straight
    /// away: its spinner animates several times a second (about 1 KB
    /// per second, measured) and the pane never goes still. With it,
    /// "still" means no more than a spinner's trickle over `still`
    /// ([`quiet::TRICKLE_BYTES_PER_SEC`]), and it counts only while an input
    /// box is on screen, whatever the byte floor: a new conversation's
    /// first screen is a few hundred bytes. The trickle bound keeps a
    /// resume still repainting from counting; the box, a loading spinner.
    ///
    /// `or_drawn_late`: see [`Step::or_drawn_late`].
    AwaitQuiet { still: Duration, min_bytes: u64, or_composer: bool, or_drawn_late: bool },
    /// Wait for work running on another thread, and fail the run with
    /// its error if it fails.
    ///
    /// For anything too slow for the tick — reading a gigabyte of
    /// history, say — that a later step depends on.  Putting it BEFORE
    /// the destructive steps is the point: a script that would kill a
    /// program and replace it can find out it has nothing to replace
    /// it with while the program is still there.
    AwaitJob(Arc<Job>),
    /// Paste the text an earlier job produced.  A job that produced
    /// none ends the run here, successfully: there was nothing to say,
    /// so the steps that would have said it are skipped.
    PasteJob(Arc<Job>),
    /// Put back what the person was typing, and stop there if there
    /// was anything.
    ///
    /// The mirror of [`StepKind::PasteJob`], and the asymmetry is the
    /// point. A pane holding someone's half-written sentence does not
    /// also need to be told something: pasting our line after theirs
    /// would send the two as one prompt -- their sentence with ours
    /// stapled on, which is the thing `autorun` refuses to do when it
    /// sees an unsent line. Their text goes back, the run ends, and
    /// what to do with it is theirs to decide.
    ///
    /// Nothing held: the run carries on to whatever was going to be
    /// said.
    RestoreComposer(Arc<Job>),
    /// Take whatever is sitting unsent in the pane's composer and put
    /// it in `job`, for a later [`StepKind::PasteJob`] to put back.
    ///
    /// Switching profiles restarts the agent, and a half-written
    /// sentence in its input box is the user's -- it belongs to them,
    /// not to the process that happened to be displaying it. Read
    /// before the agent is taken down, pasted back once the new one is
    /// reading input.
    ///
    /// Nothing there is the ordinary case and is not a failure: the
    /// job finishes with `None`, and `PasteJob` ends the run quietly.
    CaptureComposer {
        job: Arc<Job>,
        /// A line this run is responsible for having put there.
        ///
        /// Carrying the composer across is for the person's own
        /// half-written sentence. Anything marspot said is not theirs,
        /// and putting it back makes it immortal: once it was in the
        /// composer, every later switch read it as unsent text, held
        /// it, and restored it -- five panes carrying the same 34
        /// characters for a day, looking exactly like a product that
        /// keeps typing into the input box.
        ours: Option<String>,
    },
    /// Run a quick effect, failing the run with its error.  For
    /// committing a decision only once the steps before it worked.
    Call(CallFn),
    /// Wait until the program in the pane has taken the terminal over
    /// and is reading input: alt screen on, bracketed paste on, in a
    /// frame published after this step began.
    ///
    /// Asking the terminal replaces two guesses that were both wrong.
    /// "Quiet for a while" fires on the trickle a CLI emits before it
    /// loads anything, and the message then lands in something not
    /// reading input yet.  "Quiet after N bytes" fixes that and
    /// invents N, which half the panes never reach.  Alt screen plus
    /// bracketed paste is not a proxy for readiness — it *is* the
    /// program saying it has the terminal.
    ///
    /// Freshness cannot come from the published frame.  A pane whose
    /// program was just killed still has that program's last frame in
    /// the region, flags and all — but this step runs inside a script
    /// that holds the screen, so the region stops being republished
    /// and its sequence number never moves.  Waiting for a newer frame
    /// under a hold waits forever, which is exactly what it did: every
    /// switch sat out the full budget with both flags already set
    /// (`flags=0x25`, 2026-09-30).
    ///
    /// So the evidence is the byte stream, which keeps flowing whether
    /// or not anything is drawn: the mode-enabling sequences the new
    /// program sends when it takes the terminal, looked for only in
    /// the bytes written after this step began.  That is the program's
    /// own declaration, read off the wire instead of off a frame.
    AwaitTuiReady { from_len: Option<u64> },
    /// End the run, successfully, if the pane is showing something
    /// that is waiting for a keypress.
    ///
    /// A resumed CLI does not always come back to its composer.  It
    /// can come back to "do you trust the files in this folder", to a
    /// compaction prompt, to a permission confirmation — a list with
    /// one entry highlighted and `Enter` bound to *choose it*.  Typing
    /// a line there does nothing and pressing Enter answers a question
    /// nobody read.  The trust dialog's default answer is "No, exit".
    ///
    /// So: look at what the pane has drawn, and if a prompt is
    /// waiting, stop and leave it for the person.  The run ends
    /// successfully because there is nothing wrong — the pane moved to
    /// the account it needed, it just is not a pane that can be typed
    /// at right now.
    ///
    /// This is a veto, not a trigger, and it fails in the safe
    /// direction: a stale match in the tail costs a sentence the
    /// person can type, while a miss is exactly today's behaviour.
    // no script builds this step since the resume sentence moved onto
    // claude's argv; it stays until the submit-receipt path that only it
    // consumes is retired along with it
    #[allow(dead_code)]
    StopIfWaiting { needles: &'static [&'static str] },
    /// Put a line in, then submit it.
    ///
    /// Everything about this step is about the gap between the text
    /// and the newline.  Too wide and the user's own keystroke lands
    /// in between.  Too narrow — the two in one write — and claude's
    /// composer reads the carriage return as part of the pasted block
    /// and turns it into a second line: the sentence sits there
    /// unsent, which is what 0.7.82 and 2026-09-29 both did.
    ///
    /// So: paste, wait for the composer to redraw itself with the text
    /// in it, then send the carriage return on its own.  The redraw is
    /// the proof that the paste was consumed as a paste, and it is
    /// what makes the return unambiguously a key.  The gap is one
    /// frame wide, not a fixed sleep.
    ///
    /// Needs `AwaitTuiReady` in front of it, because the wrapping
    /// depends on the pane having bracketed paste on.
    // no script builds this step since the resume sentence moved onto
    // claude's argv; it stays until the submit-receipt path that only it
    // consumes is retired along with it
    #[allow(dead_code)]
    Submit {
        line: String,
        /// When the paste went out, the output length at that moment,
        /// and — once the pane starts painting — the last length seen
        /// and when it last changed.
        pasted: Option<SubmitState>,
    },
}

/// A quick effect run on the tick — see [`StepKind::Call`].
pub type CallFn = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// One step: what to do, how long it may take, and what to call it in
/// the log.
#[derive(Clone)]
pub struct Step {
    pub kind: StepKind,
    /// `None` = no deadline.  Only [`StepKind::AwaitUser`] should use
    /// it; the builder is where that is enforced by construction.
    pub timeout: Option<Duration>,
    /// What the deadline means.  For most steps it is a failure — the
    /// process never died, the program never appeared — and the run
    /// has nothing left to do.  For a wait that only improves the
    /// *timing* of what follows, it is not: running late is better
    /// than not running, and aborting there throws away the rest of
    /// the script.  Five of ten switches died this way on 2026-09-30,
    /// at a step whose only job was to hold the paste back until the
    /// CLI had painted.
    pub proceed_on_timeout: bool,
    /// Bytes this step types are for this shell's line editor: they are
    /// written only if the shell holds its terminal at that moment, and
    /// the run fails instead -- see [`front`].
    pub at_shell: Option<i32>,
    pub label: &'static str,
}

impl Step {
    /// Type nothing unless `shell` holds its own terminal at the moment
    /// of writing.  For bytes meant for a shell's line editor: written
    /// into any other program they are its input.
    pub fn at_shell(mut self, shell: i32) -> Self {
        self.at_shell = Some(shell);
        self
    }
    /// Wait for `shell` to hold its terminal; a program that still
    /// holds it at the deadline ends the run.
    pub fn await_shell(shell: i32) -> Self {
        Self {
            kind: StepKind::AwaitShell { shell },
            // A shell takes its terminal back as soon as it has reaped
            // the job that held it, which is milliseconds.
            timeout: Some(Duration::from_secs(3)),
            proceed_on_timeout: false,
            at_shell: None,
            label: "await_shell",
        }
    }
    pub fn settle(d: Duration) -> Self {
        Self { kind: StepKind::Settle, timeout: Some(d), proceed_on_timeout: false, at_shell: None, label: "settle" }
    }
    /// Signal `target` and wait for it to leave the process table.
    pub fn terminate(target: ProcId, signal: i32) -> Self {
        Self {
            kind: StepKind::Terminate { target, signal, escalate: None, sent: false },
            timeout: Some(Duration::from_secs(10)),
            proceed_on_timeout: false,
            at_shell: None,
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
    /// Fail the rest of the run if this process exits — see
    /// [`StepKind::WatchProcess`].
    pub fn watch_process(under: i32, matching: fn(&pidtree::ProcRow) -> bool) -> Self {
        Self {
            kind: StepKind::WatchProcess { under, matching },
            timeout: Some(Duration::from_secs(1)),
            proceed_on_timeout: false,
            at_shell: None,
            label: "watch_process",
        }
    }
    /// Finish here if `matching` is already running under `under`.
    pub fn stop_if_process(under: i32, matching: fn(&pidtree::ProcRow) -> bool) -> Self {
        Self {
            kind: StepKind::StopIfProcess { under, matching },
            timeout: Some(Duration::from_secs(5)),
            proceed_on_timeout: false,
            at_shell: None,
            label: "stop_if_process",
        }
    }
    pub fn await_process(under: i32, matching: fn(&pidtree::ProcRow) -> bool) -> Self {
        Self {
            kind: StepKind::AwaitProcess { under, matching },
            timeout: Some(Duration::from_secs(30)),
            proceed_on_timeout: false,
            at_shell: None,
            label: "await_process",
        }
    }
    pub fn await_job(job: Arc<Job>) -> Self {
        Self { kind: StepKind::AwaitJob(job), timeout: Some(Duration::from_secs(30)), proceed_on_timeout: false, at_shell: None,
            label: "await_job" }
    }
    /// Give back what `capture_composer` took, and stop if there was
    /// anything to give back.
    pub fn restore_composer(job: Arc<Job>) -> Self {
        Self {
            kind: StepKind::RestoreComposer(job),
            timeout: Some(Duration::from_secs(5)),
            proceed_on_timeout: false,
            at_shell: None,
            label: "restore_composer",
        }
    }

    /// Save what the person has typed but not sent.
    pub fn capture_composer(job: Arc<Job>) -> Self {
        Self {
            kind: StepKind::CaptureComposer { job, ours: None },
            timeout: Some(Duration::from_secs(5)),
            proceed_on_timeout: false,
            at_shell: None,
            label: "capture_composer",
        }
    }

    /// Name the line this run put in the pane, so capturing the
    /// composer does not carry marspot's own words across as if they
    /// were the person's.
    pub fn disowning(mut self, text: impl Into<String>) -> Self {
        if let StepKind::CaptureComposer { ours, .. } = &mut self.kind {
            *ours = Some(text.into());
        }
        self
    }

    pub fn paste_job(job: Arc<Job>) -> Self {
        Self { kind: StepKind::PasteJob(job), timeout: Some(Duration::from_secs(5)), proceed_on_timeout: false, at_shell: None,
            label: "paste_job" }
    }
    pub fn call(f: CallFn) -> Self {
        Self { kind: StepKind::Call(f), timeout: Some(Duration::from_secs(5)), proceed_on_timeout: false, at_shell: None,
            label: "call" }
    }
    pub fn await_user() -> Self {
        Self { kind: StepKind::AwaitUser, timeout: None, proceed_on_timeout: false, at_shell: None,
            label: "await_user" }
    }
    pub fn send(bytes: Vec<u8>) -> Self {
        Self { kind: StepKind::Send(bytes), timeout: Some(Duration::from_secs(5)), proceed_on_timeout: false, at_shell: None,
            label: "send" }
    }
    pub fn paste(text: impl Into<String>) -> Self {
        Self { kind: StepKind::Paste(text.into()), timeout: Some(Duration::from_secs(5)), proceed_on_timeout: false, at_shell: None,
            label: "paste" }
    }
    /// Wait for the program in the pane to take the terminal over.
    pub fn await_tui_ready() -> Self {
        Self {
            kind: StepKind::AwaitTuiReady { from_len: None },
            // Short on purpose.  A terminal that has not declared
            // itself in three seconds is not going to be helped by
            // twenty-seven more, and the pane is frozen for every one
            // of them.
            timeout: Some(Duration::from_secs(3)),
            proceed_on_timeout: false,
            at_shell: None,
            label: "await_tui_ready",
        }
    }
    /// Stop before typing if the pane is waiting on a keypress.
    // no script builds this step since the resume sentence moved onto
    // claude's argv; it stays until the submit-receipt path that only it
    // consumes is retired along with it
    #[allow(dead_code)]
    pub fn stop_if_waiting(needles: &'static [&'static str]) -> Self {
        Self {
            kind: StepKind::StopIfWaiting { needles },
            timeout: Some(Duration::from_secs(5)),
            proceed_on_timeout: false,
            at_shell: None,
            label: "stop_if_waiting",
        }
    }
    /// Put a line in and submit it, in one write.
    // no script builds this step since the resume sentence moved onto
    // claude's argv; it stays until the submit-receipt path that only it
    // consumes is retired along with it
    #[allow(dead_code)]
    pub fn submit(line: impl Into<String>) -> Self {
        Self {
            kind: StepKind::Submit { line: line.into(), pasted: None },
            timeout: Some(submit_deadline()),
            proceed_on_timeout: false,
            at_shell: None,
            label: "submit",
        }
    }
    pub fn await_quiet(still: Duration) -> Self {
        Self {
            kind: StepKind::AwaitQuiet { still, min_bytes: 0, or_composer: false, or_drawn_late: false },
            timeout: Some(Duration::from_secs(30)),
            proceed_on_timeout: false,
            at_shell: None,
            label: "await_quiet",
        }
    }
    /// Require this much output before the quiet counts — see
    /// [`StepKind::AwaitQuiet`].
    pub fn after_bytes(mut self, n: u64) -> Self {
        if let StepKind::AwaitQuiet { min_bytes, .. } = &mut self.kind {
            *min_bytes = n;
        }
        self
    }
    /// Also finish when an agent's input box is on screen — see
    /// [`StepKind::AwaitQuiet`].
    pub fn or_composer(mut self) -> Self {
        if let StepKind::AwaitQuiet { or_composer, .. } = &mut self.kind {
            *or_composer = true;
        }
        self
    }
    /// Treat this step's deadline as "long enough" rather than as a
    /// failure: when it passes, move on.  Only for waits that sharpen
    /// the timing of a later step, never for one whose success the
    /// rest of the script depends on.
    pub fn or_late(mut self) -> Self {
        self.proceed_on_timeout = true;
        self
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
    /// A first-frame wait ran out on a program that had drawn and was
    /// still drawing. Not a failure: the rest of the run went on, minus
    /// the steps that type into the pane, named in `skipped`.
    DrawnLate { step: usize, label: &'static str, skipped: Vec<&'static str> },
    /// The user pressed Esc, or the host ended the session.
    Cancelled(&'static str),
    /// An effect failed (the PTY is gone, the core is down).
    Failed { step: usize, label: &'static str, err: String },
}

impl OpOutcome {
    /// The run did what it was for: every step ran, or it was drawn late.
    pub fn is_done(&self) -> bool {
        matches!(self, Self::Done | Self::DrawnLate { .. })
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
    /// Badge while running; [`WORKING_MARK`] is appended while a step works.
    pub badge: Option<String>,
    /// Told how the run ended: the submitter's own channel back, since
    /// the queue's reports go to whoever owns the queue.
    pub on_end: Option<OnEnd>,
    /// Whose input the run types: see [`Origin`].
    pub origin: Origin,
    /// The longest bringing a pane back may take once woken.
    pub wake_bound: Option<Duration>,
}

/// See [`PtyOp::on_end`].
pub type OnEnd = Arc<dyn Fn(&OpOutcome) + Send + Sync>;

impl PtyOp {
    /// Where the step called `label` sits, for a run that has to
    /// resume part-way through.
    ///
    /// A re-armed run used to be told to start at a number written
    /// down beside the script. Inserting a step anywhere before it
    /// moves what that number points at, silently -- putting a capture
    /// of the composer at the front sent a re-arm into the middle of
    /// the teardown instead of to the wake it was meant to park on,
    /// and only a test standing on that exact step noticed. A name
    /// survives the edit; a position does not.
    pub fn index_of(&self, label: &str) -> Option<usize> {
        self.steps.iter().position(|s| s.label == label)
    }

    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            steps: Vec::new(),
            hold_screen: false,
            lock_keys: true,
            escape_hatch: true,
            badge: None,
            on_end: None,
            origin: Origin::Marspot,
            wake_bound: None,
        }
    }
    pub fn on_end(mut self, f: OnEnd) -> Self {
        self.on_end = Some(f);
        self
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

/// How long [`StepKind::Submit`] waits for the pasted text to appear
/// before sending the return anyway.
///
/// Eight seconds, because the thing being waited for is a busy program
/// getting round to its input queue.  A pane mid-task can take
/// seconds; 1.5 was enough for an idle one and not for that, which is
/// what left a sentence in a composer on 2026-09-30 after the quiet
/// window had already been added.  Sending late costs a slow sentence;
/// sending early costs a message nobody sent.
const SUBMIT_ECHO_GRACE: Duration = Duration::from_millis(8_000);

/// How long the pane has to stay quiet, *after the text has shown up*,
/// before the return counts as arriving behind the paste.
///
/// Two attempts got here.  "It wrote something back" was not evidence
/// of anything — that interface repaints on its own, so any byte
/// satisfied it and a return went out 26 ms behind its paste.  Adding
/// this quiet window fixed four panes out of five; the fifth was
/// mid-task, painted 104 bytes of something else, and went quiet
/// without ever having drawn the paste.  Quiet is a proxy.  The text
/// on screen is the thing itself, and this now only says the repaint
/// that carried it has finished.
const SUBMIT_QUIET: Duration = Duration::from_millis(250);

/// How long to let the composer repaint before asking whether the
/// return landed.
const SUBMIT_VERIFY_AFTER: Duration = Duration::from_millis(600);
/// How many returns to send before leaving the line for the person.
/// Three is not a retry loop, it is "the first one was eaten, and so
/// was the second".
const SUBMIT_MAX_RETURNS: u8 = 3;

/// How long the step is allowed to take, from the four numbers above.
///
/// It was five seconds, flat, while the step was designed to spend up
/// to eight waiting for the paste alone -- so it could never reach its
/// own grace, and the log has it giving up at 5.4 and 5.6 seconds on
/// two panes whose sentences were left sitting in their composers.
/// The grace had been raised from 1.5 to 8 to cover a busy pane; the
/// deadline was not, because it was written somewhere else and nothing
/// tied them together.
///
/// Now it is derived, so raising any of them raises this too: wait out
/// the paste, then the quiet, then up to three returns each followed
/// by a look at the composer, and two seconds of slack for a pane that
/// is slow at every one of those.
fn submit_deadline() -> Duration {
    SUBMIT_ECHO_GRACE
        + SUBMIT_QUIET
        + SUBMIT_VERIFY_AFTER * SUBMIT_MAX_RETURNS as u32
        + Duration::from_secs(2)
}

/// Is `line` still sitting on the pane's prompt, unsent?
///
/// claude's interface marks both the composer and every message
/// already sent with the same glyph, so the question is not "is the
/// text on screen" — after a successful submit it is, in the
/// transcript.  It is "is it on the LAST prompt", which is the
/// composer.  That is the rule that told the five panes of
/// 2026-09-30 apart by hand, and it is the one used here.
/// The characters an agent's composer begins its prompt line with.
///
/// Read off the real panes rather than guessed: Claude Code draws
/// U+276F, Codex draws U+203A, and a plain shell draws `>`. The list
/// being short is the point -- when none of them is on screen this
/// function says so rather than inventing an answer.
const COMPOSER_PROMPTS: [char; 3] = ['\u{276f}', '\u{203a}', '>'];

/// Text that is sitting in the composer because something delivered it
/// there, not because the person typed it.
///
/// Another session's message arrives by being written into the input
/// box, so it is indistinguishable from a half-finished sentence by
/// shape alone. Carrying it across a switch puts someone else's words
/// back under the person's cursor, and the carry writes it in again on
/// every switch after that.
fn delivered_not_typed(t: &str) -> bool {
    t.trim_start()
        .strip_prefix("Message from @")
        .and_then(|rest| rest.split_once(':'))
        .is_some_and(|(from, _)| !from.is_empty() && !from.contains(char::is_whitespace))
}

fn is_composer_line(l: &str) -> bool {
    let t = l.trim_start();
    COMPOSER_PROMPTS.iter().any(|p| t.starts_with(*p))
}

/// Is the line we pasted still sitting unsent on the prompt?
///
/// **A screen this cannot read counts as "still there".** It used to
/// count as "gone", which made every way of failing look like success:
/// a pane mid-repaint, a prompt character we do not know, a composer
/// scrolled out of view -- each returned `false`, the submit was
/// logged `landed`, and the text stayed in the box. On 2026-10-01
/// three panes sat with an unsent line while the log said every one
/// of them had landed, and `.stuck` -- the branch that gives up and
/// says so -- had never once fired in the whole file.
///
/// Not knowing is not the same as knowing it worked. Saying "still
/// there" costs another return, and the retry ceiling turns a wrong
/// guess into a reported failure instead of a silent one.
fn line_is_still_in_the_composer(screen: &str, line: &str) -> bool {
    let needle = std::str::from_utf8(submit_needle(line)).unwrap_or(line);
    match screen.lines().rev().find(|l| is_composer_line(l)) {
        Some(last_prompt) => last_prompt.contains(needle),
        None => true,
    }
}

/// The piece of a submitted line to look for coming back.
///
/// Short, and taken from the front: the composer wraps a long line, so
/// the further in a fragment sits the likelier a line break lands in
/// the middle of it.  A prefix that survives wrapping is enough —
/// nothing else in the pane is going to be painting it.
fn submit_needle(line: &str) -> &[u8] {
    let mut end = line.len().min(12);
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    &line.as_bytes()[..end]
}

/// What a [`StepKind::Submit`] has seen since its paste went out.
#[derive(Clone, Copy, Debug)]
pub struct SubmitState {
    sent_at: SystemTime,
    base_len: u64,
    seen_len: u64,
    changed_at: SystemTime,
    /// Set once the pasted text has been seen in the pane's output.
    /// Until then there is nothing to be quiet after.
    showed_at: Option<SystemTime>,
    /// When the return went out, and how many have.
    returned_at: Option<SystemTime>,
    returns: u8,
}

/// How much longer a step waits when the pane has drawn *nothing*.
///
/// `or_late` exists for a program that is slow, and going on is the
/// right call for one: it has started, the screen is moving, and the
/// step's timeout was only a guess about how long that takes. A pane
/// that has produced zero bytes has not started, and no amount of
/// typing at it helps. Multiplied rather than fixed, so each step
/// keeps its own sense of scale.
const LATE_EMPTY_GRACE: u32 = 10;

/// How often a working run's badge is looked at again, for the note a
/// job step adds to it.
const WORKING_BADGE_EVERY: Duration = Duration::from_millis(125);

/// What a working run's badge ends with.  Not a spinner: one only turns
/// on a timer, and the supervisor ticks once a second at most, so its
/// frames came irregularly and read as frozen.
pub const WORKING_MARK: char = '…';

/// How often a parked run re-asserts its badge.
///
/// Nothing is animating, so this exists only so a core that came up
/// with no badges (a silent update, a crash) gets them back.
const PARKED_BADGE_EVERY: Duration = Duration::from_secs(8);

/// Runs a [`PtyOp`] as a PaneSession.
pub struct OpRunner {
    op: PtyOp,
    env: Box<dyn OpEnv>,
    /// Index of the step being run.
    at: usize,
    /// When the current step started — every deadline is relative to
    /// this, so one slow step cannot eat the next one's budget.
    entered: SystemTime,
    /// `AwaitQuiet` bookkeeping: the output length when the step
    /// started, the length now, and when it last moved.
    entered_len: u64,
    seen_len: u64,
    /// Output length when this run last typed a line into the pane.
    ///
    /// The anchor for "what did that line produce": a step that reads
    /// only what arrived after itself misses a program that was
    /// quicker than the tick.
    sent_len: Option<u64>,
    still_since: SystemTime,
    drew: bool,
    /// The process [`StepKind::WatchProcess`] tied the run to.
    watched: Option<ProcId>,
    /// The program [`StepKind::AwaitProcess`] last found.
    awaited: Option<ProcId>,
    /// `(when, output length)` every [`SAMPLE_EVERY`] while an
    /// `or_composer` wait runs; at most [`SAMPLES_KEPT`].
    samples: std::collections::VecDeque<(SystemTime, u64)>,
    /// Set once the run is over; the next tick ends the session.  Two
    /// ticks rather than one because ending is the host's to do.
    finished: Option<OpOutcome>,
    /// What the last readiness probe saw, kept so a step that gives up
    /// can say what it was looking at rather than only that it waited.
    ready_saw: Option<(u64, u32)>,
    /// The first-frame wait that ran out drawn, and the typing steps
    /// left out since; see [`Step::or_drawn_late`].
    late: Option<(usize, &'static str)>,
    skipped: Vec<&'static str>,
    /// What the step that ended the run saw, said on the outcome line.
    why: Option<String>,
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
    /// When the badge was last sent — see the throttle in `on_tick`.
    badge_at: SystemTime,
    /// When an `AwaitUser` step was moved on.
    woken_at: Option<SystemTime>,
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
            entered_len: 0,
            seen_len: 0,
            sent_len: None,
            still_since: now,
            drew: false,
            watched: None,
            awaited: None,
            samples: std::collections::VecDeque::new(),
            finished: None,
            ready_saw: None,
            late: None,
            skipped: Vec::new(),
            why: None,
            cleaned: false,
            started: false,
            badge_at: SystemTime::UNIX_EPOCH,
            woken_at: None,
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

    fn finish(&mut self, host: &dyn PaneSessionHost, outcome: OpOutcome) {
        if self.finished.is_some() {
            return;
        }
        let outcome = match (outcome, self.late) {
            (OpOutcome::Done, Some((step, label))) => {
                OpOutcome::DrawnLate { step, label, skipped: std::mem::take(&mut self.skipped) }
            }
            (o, _) => o,
        };
        let why = self.why.take().map(|w| format!("; {w}")).unwrap_or_default();
        host.log(
            if outcome.is_done() { LogLevel::Info } else { LogLevel::Warn },
            &format!("{}.outcome", self.op.name),
            &format!("pane {} {outcome:?}{why}", host.shelld_session_id()),
        );
        self.finished = Some(outcome);
    }

    /// Release the hold.  Runs on every exit path, at most once.
    fn cleanup(&mut self, host: &dyn PaneSessionHost) {
        if self.cleaned {
            return;
        }
        self.cleaned = true;
        if self.op.hold_screen
            && let Err(e) = self.env.io().hold(host.shelld_session_id(), false) {
                host.log(
                    LogLevel::Warn,
                    &format!("{}.release_failed", self.op.name),
                    &format!("{e}"),
                );
            }
    }

    fn enter_step(&mut self, host: &dyn PaneSessionHost) {
        self.entered = self.env.now();
        self.still_since = self.entered;
        self.seen_len = self.env.output_len(host.shelld_session_id());
        self.entered_len = self.seen_len;
        self.drew = false;
        self.samples.clear();
        if let Some(s) = self.op.steps.get(self.at) {
            host.log(
                LogLevel::Info,
                &format!("{}.step", self.op.name),
                // The pane, always.  Two runs on two panes interleave
                // in the log, and without this the sequence reads as
                // one impossible run — three wakes for two parked
                // panes, with no way to tell which was which.
                &format!(
                    "pane {} {}/{} {}",
                    host.shelld_session_id(),
                    self.at + 1,
                    self.op.steps.len(),
                    s.label
                ),
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
        if self.skip_when_late(host, &step) {
            return true;
        }
        match step.kind {
            StepKind::Settle => elapsed >= step.timeout.unwrap_or_default(),
            StepKind::Terminate { target, signal, escalate, sent } => {
                let due = if sent { escalate.filter(|(after, _)| elapsed >= *after) } else { Some((Duration::ZERO, signal)) };
                if let Some(StepKind::Terminate { sent, .. }) = self.op.steps.get_mut(self.at).map(|s| &mut s.kind) {
                    *sent = true;
                }
                // re-verified on every send: the escalation can come seconds after the decision
                let gone = match due.map(|(_, sig)| self.env.signal(target, sig)) {
                    Some(Err(SignalError::Gone)) => {
                        let msg = format!("pid {} is already gone", target.pid);
                        host.log(LogLevel::Info, &format!("{}.already_gone", self.op.name), &msg);
                        true
                    }
                    Some(Err(e)) => {
                        let err = format!("pid {}: {e}", target.pid);
                        self.finish(host, OpOutcome::Failed { step: self.at, label: step.label, err });
                        return true;
                    }
                    _ => !self.env.pid_alive(target),
                };
                if gone {
                    // We took it down, so we own what it left switched
                    // on.  Mouse reporting is the one mode that outlives
                    // its program here: a signal produces no bytes, so
                    // the `CSI ? 1002 l` a clean exit would have sent
                    // never happens, and whatever the pane falls back to
                    // — usually a shell prompt — inherits a terminal
                    // that encodes each scroll as `CSI < 64;x;y M` and
                    // types it in (2026-09-01).
                    let _ = self.env.io().reset_mouse_reporting(sid);
                    return true;
                }
                false
            }
            StepKind::AwaitProcess { under, matching } => {
                self.awaited = self.env.find_in_front(under, matching);
                self.awaited.is_some()
            }
            StepKind::AwaitShell { shell } => match self.env.front(shell).shell_or_why() {
                Ok(()) => true,
                // A program in front may be the one just taken down, on
                // its way out (gemini's outer process outlives its
                // worker by a moment), so it is waited out rather than
                // failed at once; at the deadline, the run says who.
                Err(err) if step.timeout.is_some_and(|t| elapsed >= t) => {
                    host.log(LogLevel::Warn, &format!("{}.not_typed", self.op.name), &format!("pane {sid}: {err}"));
                    self.finish(host, OpOutcome::Failed { step: self.at, label: step.label, err });
                    true
                }
                Err(_) => false,
            },
            StepKind::WatchProcess { under, matching } => {
                match self.env.find_in_front(under, matching) {
                    Some(pid) => self.watched = Some(pid),
                    None => self.finish(
                        host,
                        OpOutcome::Failed {
                            step: self.at,
                            label: step.label,
                            err: "the program to watch is not running".into(),
                        },
                    ),
                }
                true
            }
            StepKind::StopIfProcess { under, matching } => {
                // anywhere in the pane, in front or not: stopping is the
                // side to err on, and a suspended one is still the pane's
                if self.env.find_descendant(under, matching).is_some() {
                    host.log(
                        LogLevel::Info,
                        &format!("{}.already_done", self.op.name),
                        &format!(
                            "pane {} already has one; nothing left to do",
                            host.shelld_session_id()
                        ),
                    );
                    self.finish(host, OpOutcome::Done);
                }
                true
            }
            StepKind::AwaitUser => false, // only `wake()` moves this on
            StepKind::AwaitJob(job) => match job.poll() {
                None => false,
                Some(Ok(())) => true,
                Some(Err(err)) => {
                    self.finish(host, OpOutcome::Failed { step: self.at, label: step.label, err });
                    true
                }
            },
            StepKind::Call(f) => {
                if let Err(err) = f() {
                    self.finish(host, OpOutcome::Failed { step: self.at, label: step.label, err });
                }
                true
            }
            StepKind::RestoreComposer(job) => {
                let Some(text) = job.text() else {
                    // Nothing was held, so there is nothing to give
                    // back and no reason to stop.
                    return true;
                };
                host.log(
                    LogLevel::Info,
                    &format!("{}.composer_restored", self.op.name),
                    &format!(
                        "pane {sid}: {} unsent chars put back; the rest is theirs",
                        text.chars().count()
                    ),
                );
                self.deliver(host, step.label, &text);
                self.finish(host, OpOutcome::Done);
                true
            }
            StepKind::CaptureComposer { job, ours } => {
                // A screen we cannot read means we do not know whether
                // anything was there, and the only safe reading of
                // "do not know" is "nothing" -- pasting a guess into
                // someone's input box is worse than losing a line they
                // can retype. It is logged either way so the choice is
                // visible afterwards.
                let unsent = self.env.pane_unsent(sid);
                // Ours is not theirs. Taken off the front rather than
                // matched whole, because the person may have started
                // typing after it -- that part is theirs and travels.
                let unsent = match (&unsent, &ours) {
                    (Some(t), Some(ours)) if t.trim_start().starts_with(ours.as_str()) => {
                        let rest = t.trim_start()[ours.len()..].trim();
                        host.log(
                            LogLevel::Info,
                            &format!("{}.composer_ours", self.op.name),
                            &format!(
                                "pane {sid}: {} chars of it were ours and are not carried",
                                ours.chars().count()
                            ),
                        );
                        (!rest.is_empty()).then(|| rest.to_string())
                    }
                    _ => unsent,
                };
                // Someone else's message is not the person's unsent text,
                // however much it looks like it from the grid.
                let unsent = match &unsent {
                    Some(t) if delivered_not_typed(t) => {
                        host.log(
                            LogLevel::Info,
                            &format!("{}.composer_delivered", self.op.name),
                            &format!(
                                "pane {sid}: the composer holds a message delivered to it, \
                                 not typed; not carried"
                            ),
                        );
                        None
                    }
                    _ => unsent,
                };
                match &unsent {
                    Some(t) => host.log(
                        LogLevel::Info,
                        &format!("{}.composer_held", self.op.name),
                        &format!("pane {sid}: holding {} unsent chars", t.chars().count()),
                    ),
                    None => host.log(
                        LogLevel::Info,
                        &format!("{}.composer_empty", self.op.name),
                        &format!("pane {sid}: nothing unsent to carry across"),
                    ),
                }
                job.finish(Ok(unsent));
                true
            }
            StepKind::PasteJob(job) => {
                let Some(text) = job.text() else {
                    host.log(
                        LogLevel::Info,
                        &format!("{}.nothing_to_paste", self.op.name),
                        &format!("pane {sid}: the job produced no text; done"),
                    );
                    self.finish(host, OpOutcome::Done);
                    return true;
                };
                self.deliver(host, step.label, &text);
                true
            }
            StepKind::Paste(text) => {
                self.deliver(host, step.label, &text);
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
                // Where to start reading for what the line produces.
                // Taken here rather than when a later step first runs,
                // because a program that starts quickly can take the
                // terminal in the gap between the two -- and a step
                // that only looks at bytes written after itself then
                // sees nothing and concludes the pane never drew.
                // Measured end to end: the stand-in painted at :31 and
                // `await_tui_ready` began at :32, waited thirty
                // seconds and gave up on a pane that was already
                // showing a full screen.
                self.sent_len = Some(self.env.output_len(sid));
                if let Err(err) = self.type_into(host, Typed::Bytes(&bytes)) {
                    self.finish(host, OpOutcome::Failed { step: self.at, label: step.label, err });
                }
                true
            }
            StepKind::AwaitTuiReady { from_len } => {
                let now = self.env.output_len(sid);
                let base = match from_len {
                    Some(v) => v,
                    None => {
                        // Everything the line has produced, not
                        // everything since this step woke up.
                        let start = self.sent_len.unwrap_or(now);
                        if let Some(StepKind::AwaitTuiReady { from_len }) =
                            self.op.steps.get_mut(self.at).map(|s| &mut s.kind)
                        {
                            *from_len = Some(start);
                        }
                        start
                    }
                };
                let fresh = now.saturating_sub(base) as usize;
                self.ready_saw = Some((fresh as u64, 0));
                if fresh == 0 {
                    return false;
                }
                let tail = self.env.output_tail(sid, fresh.min(256 * 1024));
                // What a full-screen program sends when it takes over.
                // Both, because a shell sets bracketed paste on its own
                // and only a full-screen program asks for the alternate
                // buffer.
                let took_over = contains(&tail, b"\x1b[?1049h") || contains(&tail, b"\x1b[?47h");
                let reads_pastes = contains(&tail, b"\x1b[?2004h");
                if took_over && reads_pastes {
                    host.log(
                        LogLevel::Info,
                        &format!("{}.ready", self.op.name),
                        &format!("sid={sid} the pane's program took the terminal ({fresh} B in)"),
                    );
                }
                took_over && reads_pastes
            }
            StepKind::StopIfWaiting { needles } => {
                let tail = self.env.output_tail(sid, 16 * 1024);
                let text = String::from_utf8_lossy(&tail);
                if let Some(hit) = needles.iter().find(|n| text.contains(**n)) {
                    host.log(
                        LogLevel::Info,
                        &format!("{}.waiting", self.op.name),
                        &format!("sid={sid} the pane is asking something ({hit:?}); leaving it alone"),
                    );
                    self.finish(host, OpOutcome::Done);
                    return true;
                }
                true
            }
            StepKind::Submit { ref line, pasted } => {
                let Some(st) = pasted else {
                    use marspot_term::grid_shm::FLAG_BRACKETED_PASTE;
                    let bracketed = self
                        .env
                        .pane_modes(sid)
                        .map(|(_, f)| f & FLAG_BRACKETED_PASTE != 0)
                        .unwrap_or(false);
                    let mut bytes = Vec::new();
                    if bracketed {
                        marspot_term::paste::bracketed_paste(line.as_bytes(), &mut bytes);
                    } else {
                        // Nothing to bracket with.  The text still goes
                        // in and the return still follows it a redraw
                        // later; all that is lost is the marker that
                        // told the program this was a paste.
                        bytes.extend_from_slice(line.as_bytes());
                    }
                    let base = self.env.output_len(sid);
                    host.log(
                        LogLevel::Info,
                        &format!("{}.paste", self.op.name),
                        &format!("sid={sid} bytes={} bracketed={bracketed}", bytes.len()),
                    );
                    if let Err(err) = self.type_into(host, Typed::Bytes(&bytes)) {
                        self.finish(host, OpOutcome::Failed { step: self.at, label: step.label, err });
                        return true;
                    }
                    if let Some(StepKind::Submit { pasted, .. }) =
                        self.op.steps.get_mut(self.at).map(|s| &mut s.kind)
                    {
                        let now = self.env.now();
                        *pasted = Some(SubmitState {
                            sent_at: now,
                            base_len: base,
                            seen_len: base,
                            changed_at: now,
                            showed_at: None,
                            returned_at: None,
                            returns: 0,
                        });
                    }
                    return false;
                };
                // Two things have to be true: the pane painted
                // something after the paste, and it has stopped.  The
                // first alone is not evidence — this interface
                // repaints by itself — and it was what let a return go
                // out 26 ms behind the paste and land as a newline.
                let now = self.env.now();

                // A return has gone out — did it do anything?
                //
                // Every gate before this one is a guess about when the
                // composer is ready, and three of them have been wrong.
                // This is not a guess: it reads the pane and looks for
                // the line still sitting on the prompt.  If it is
                // there, the return became a newline and another one
                // is sent.
                if let Some(at) = st.returned_at {
                    // A receipt is proof and arrives on its own
                    // schedule, so it is read before the delay that
                    // exists only to give the screen time to settle.
                    let reported = self.env.submit_receipt(sid, line);
                    if reported == Some(true) {
                        host.log(
                            LogLevel::Info,
                            &format!("{}.landed", self.op.name),
                            &format!(
                                "sid={sid} the pane reported it, after {} return(s)",
                                st.returns
                            ),
                        );
                        return true;
                    }
                    if now.duration_since(at).unwrap_or_default() < SUBMIT_VERIFY_AFTER {
                        return false;
                    }
                    // **A pane that can report is judged only on what
                    // it reports.** Falling back to the screen here
                    // would make the receipt an accelerator rather
                    // than evidence: the run could still end in
                    // `landed` because a composer looked empty, which
                    // is the thing receipts exist to stop believing.
                    // Cutting the channel then has to end in returns
                    // exhausted and a reported failure, and that is
                    // what the acceptance in the RFC asks for.
                    let still = match reported {
                        Some(_) => true,
                        None => self
                            .env
                            .pane_screen(sid)
                            .map(|screen| line_is_still_in_the_composer(&screen, line))
                            .unwrap_or(true),
                    };
                    if !still {
                        host.log(
                            LogLevel::Info,
                            &format!("{}.landed", self.op.name),
                            &format!("sid={sid} after {} return(s)", st.returns),
                        );
                        return true;
                    }
                    if st.returns >= SUBMIT_MAX_RETURNS {
                        host.log(
                            LogLevel::Warn,
                            &format!("{}.stuck", self.op.name),
                            &format!(
                                "sid={sid} the line is still in the composer after {} returns; \
                                 leaving it for the person",
                                st.returns
                            ),
                        );
                        return true;
                    }
                    host.log(
                        LogLevel::Info,
                        &format!("{}.again", self.op.name),
                        &format!("sid={sid} the return became a newline; sending another"),
                    );
                    if let Err(err) = self.type_into(host, Typed::Bytes(b"\r")) {
                        self.finish(host, OpOutcome::Failed { step: self.at, label: step.label, err });
                        return true;
                    }
                    if let Some(StepKind::Submit { pasted: Some(p), .. }) =
                        self.op.steps.get_mut(self.at).map(|s| &mut s.kind)
                    {
                        p.returned_at = Some(now);
                        p.returns += 1;
                    }
                    return false;
                }

                let len = self.env.output_len(sid);
                let waited = now.duration_since(st.sent_at).unwrap_or_default();
                // The grace is checked before anything else: a pane
                // that repaints on every tick would otherwise keep
                // resetting the quiet window and never reach it.
                let expired = waited >= SUBMIT_ECHO_GRACE;

                // Has the text itself come back?  A busy program gets
                // to its input queue when it gets to it, and until it
                // has drawn what was pasted there is nothing to be
                // quiet after.
                let mut showed_at = st.showed_at;
                if showed_at.is_none() && !expired {
                    let fresh = len.saturating_sub(st.base_len) as usize;
                    if fresh > 0 {
                        let tail = self.env.output_tail(sid, fresh.min(256 * 1024));
                        if contains(&tail, submit_needle(line)) {
                            showed_at = Some(now);
                        }
                    }
                }

                if !expired
                    && len != st.seen_len
                    && let Some(StepKind::Submit { pasted: Some(p), .. }) =
                        self.op.steps.get_mut(self.at).map(|s| &mut s.kind)
                {
                    p.seen_len = len;
                    p.changed_at = now;
                    p.showed_at = showed_at;
                    return false;
                }
                let quiet = now.duration_since(st.changed_at).unwrap_or_default();
                let settled = showed_at.is_some() && quiet >= SUBMIT_QUIET;
                if !expired && !settled {
                    return false;
                }
                host.log(
                    LogLevel::Info,
                    &format!("{}.submit", self.op.name),
                    &format!(
                        "sid={sid} showed={} painted={}B quiet={}ms after={}ms",
                        showed_at.is_some(),
                        len.saturating_sub(st.base_len),
                        quiet.as_millis(),
                        waited.as_millis()
                    ),
                );
                if let Err(err) = self.type_into(host, Typed::Bytes(b"\r")) {
                    self.finish(host, OpOutcome::Failed { step: self.at, label: step.label, err });
                    return true;
                }
                if let Some(StepKind::Submit { pasted: Some(p), .. }) =
                    self.op.steps.get_mut(self.at).map(|s| &mut s.kind)
                {
                    p.returned_at = Some(now);
                    p.returns += 1;
                    p.showed_at = showed_at;
                }
                false
            }
            StepKind::AwaitQuiet { still, min_bytes, or_composer, .. } => {
                self.await_quiet(host, step.label, still, min_bytes, or_composer)
            }
        }
    }

    /// Paste `text`, refusing anything over the cap.
    fn deliver(&mut self, host: &dyn PaneSessionHost, label: &'static str, text: &str) {
        let sid = host.shelld_session_id();
        if text.len() > MAX_PAYLOAD {
            self.finish(
                host,
                OpOutcome::Failed {
                    step: self.at,
                    label,
                    err: format!("{} B exceeds the {MAX_PAYLOAD} B cap", text.len()),
                },
            );
            return;
        }
        host.log(
            LogLevel::Info,
            &format!("{}.paste", self.op.name),
            &format!("sid={sid} bytes={}", text.len()),
        );
        if let Err(err) = self.type_into(host, Typed::Paste(text)) {
            self.finish(host, OpOutcome::Failed { step: self.at, label, err });
        }
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

    fn parked(&self) -> bool {
        self.is_awaiting_user()
    }

    fn wake_for_input(&mut self, host: &dyn PaneSessionHost) -> bool {
        self.wake(host, "input")
    }

    fn waking_for(&self) -> Option<Duration> {
        self.wake_left()
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
        if self.finished.is_some() {
            host.end();
            return;
        }
        if !self.started {
            // First tick: take the hold before anything else runs, and
            // log where we are starting.
            self.started = true;
            if self.op.hold_screen
                && let Err(e) = self.env.io().hold(host.shelld_session_id(), true) {
                    host.log(
                        LogLevel::Warn,
                        &format!("{}.hold_failed", self.op.name),
                        &format!("{e}"),
                    );
                }
            self.enter_step(host);
        }
        if let Some(b) = &self.op.badge {
            // A wire frame per pane per tick would be background creep
            // — and `on_tick` rides the redraw pump, so "per tick" is
            // 16 ms while anything is drawing.  A step that waits for
            // the user can hold that for hours.
            //
            // While a step is *working* a job's note can change, so the
            // badge is looked at often (an unchanged one sends nothing);
            // on the parked step it is re-asserted only often enough to
            // survive a core that restarted with no badges.
            let idle = matches!(
                self.op.steps.get(self.at).map(|s| &s.kind),
                Some(StepKind::AwaitUser)
            );
            // The run's own clock, not the wall's: mixing the two is
            // how the first cut of this throttle did nothing at all
            // (625 badge frames in a ten-second test — `badge_at` came
            // from `env.now()` and the comparison from
            // `SystemTime::now()`, so every tick looked overdue).
            let since = self
                .env
                .now()
                .duration_since(self.badge_at)
                .unwrap_or_default();
            let due = since >= if idle { PARKED_BADGE_EVERY } else { WORKING_BADGE_EVERY };
            if due {
                self.badge_at = self.env.now();
                let note = match self.op.steps.get(self.at).map(|s| &s.kind) {
                    Some(StepKind::AwaitJob(j)) => j.note().map(|n| format!(" {n}")),
                    _ => None,
                };
                let note = note.as_deref().unwrap_or("");
                let text = if idle { b.clone() } else { format!("{b}{note} {WORKING_MARK}") };
                host.set_badge(&text);
            }
        }
        if let Some(id) = self.watched
            && !self.env.pid_alive(id)
        {
            let pid = id.pid;
            let label = self.op.steps.get(self.at).map_or("end", |s| s.label);
            host.log(
                LogLevel::Warn,
                &format!("{}.program_exited", self.op.name),
                &format!("pid {pid} exited during {label}; ending the run"),
            );
            self.finish(
                host,
                OpOutcome::Failed { step: self.at, label, err: format!("pid {pid} exited") },
            );
            return;
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
        if self.quiet_ran_out(host, elapsed) {
            return;
        }
        if let Some(step) = self.op.steps.get(self.at)
            && let Some(limit) = step.timeout
                && elapsed >= limit {
                    if step.proceed_on_timeout {
                        let drawn = self
                            .env
                            .output_len(host.shelld_session_id())
                            .saturating_sub(self.entered_len);
                        // Nothing drawn at all is not "slow", it is
                        // "has not started". Going on means typing
                        // into a program that is not reading yet, and
                        // the return lands as a newline -- which is
                        // exactly what a machine at load 513 produced
                        // on 2026-10-01: 0 B after 3 s, the line
                        // pasted anyway, three returns, and a
                        // sentence left sitting in the composer.
                        //
                        // A program that has drawn *something* is
                        // merely slow and going on is the right call.
                        // One that has drawn nothing gets the longer
                        // wait, and then it is a failure rather than
                        // a thing to type at.
                        if drawn == 0 {
                            if elapsed < limit * LATE_EMPTY_GRACE {
                                return;
                            }
                            host.log(
                                LogLevel::Warn,
                                &format!("{}.never_drew", self.op.name),
                                &format!(
                                    "step={} waited {}ms and the pane drew nothing; \
                                     not typing into it",
                                    step.label,
                                    elapsed.as_millis()
                                ),
                            );
                            self.finish(
                                host,
                                OpOutcome::TimedOut { step: self.at, label: step.label },
                            );
                            return;
                        }
                        let saw = match self.ready_saw {
                            Some((bytes, _)) => format!(", {bytes} B since the step began"),
                            None => String::new(),
                        };
                        host.log(
                            LogLevel::Info,
                            &format!("{}.late", self.op.name),
                            &format!(
                                "step={} waited {}ms, drew {drawn}B{saw} — going on anyway",
                                step.label,
                                limit.as_millis()
                            ),
                        );
                        self.advance(host);
                    } else {
                        self.finish(
                            host,
                            OpOutcome::TimedOut { step: self.at, label: step.label },
                        );
                    }
                }
    }

    fn on_end(&mut self, host: &dyn PaneSessionHost, reason: EndReason) {
        if self.finished.is_none() {
            self.finish(host, OpOutcome::Cancelled(match reason {
                EndReason::UserEscape => "escape",
                EndReason::PluginRequested => "plugin",
                EndReason::PaneClosed => "pane closed",
            }));
        }
        self.cleanup(host);
        let outcome = self.finished.clone().unwrap_or(OpOutcome::Done);
        if let Some(f) = self.on_finish.as_mut() {
            f(host, &outcome);
        }
        if let Some(f) = &self.op.on_end {
            f(&outcome);
        }
    }
}

pub mod front;
mod job;
pub use job::Job;
mod quiet;
#[cfg(test)]
use quiet::composer_on_screen;
mod real_env;
mod typing;
mod wake;
pub use typing::{Origin, PtyIo, Typed};
pub use real_env::RealEnv;
#[cfg(test)]
mod never_ran_tests;

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
/// first CLI delivery with `missing permission: PermissionSet(16)` (the
/// permission model that message came from is gone; see the plugins
/// module docs) —
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
/// better failure than growing forever.
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
    receipts: Option<Arc<crate::receipts::Receipts>>,
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
            receipts: None,
            queued: std::collections::HashMap::new(),
            active: std::collections::HashMap::new(),
            sink: Arc::new(std::sync::Mutex::new(Vec::new())),
            next_id: 1,
        }
    }

    /// Where panes report their own submits.
    ///
    /// Takes an `Option` because binding the socket is best-effort:
    /// without it every op reads the screen, which is what they all
    /// did before.
    pub fn with_receipts(mut self, receipts: Option<Arc<crate::receipts::Receipts>>) -> Self {
        self.receipts = receipts;
        self
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
            if let Some(f) = &op.on_end {
                f(&OpOutcome::Failed { step: 0, label: "queue", err: "the pane's queue is full".into() });
            }
            return None;
        }
        let id = OpId(self.next_id);
        self.next_id += 1;
        q.push_back((id, op, step));
        Some(id)
    }

    /// Forget closed panes: drop their queued runs and free their slot.
    pub fn retain(&mut self, open: impl Fn(u64) -> bool) {
        // a run that never started is still told it is over
        for (_, q) in self.queued.iter().filter(|(sid, _)| !open(**sid)) {
            for (_, op, _) in q {
                if let Some(f) = &op.on_end {
                    f(&OpOutcome::Cancelled("pane closed"));
                }
            }
        }
        self.queued.retain(|sid, _| open(*sid));
        self.active.retain(|sid, _| open(*sid));
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
            let notify = op.on_end.clone();
            let sink = Arc::clone(&self.sink);
            let runner = OpRunner::new(
                op,
                Box::new(RealEnv::with_receipts(
                    Arc::clone(&self.io),
                    self.receipts.clone(),
                )),
            )
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
                    let outcome = OpOutcome::Failed { step: 0, label: "begin", err: e };
                    if let Some(f) = &notify {
                        f(&outcome);
                    }
                    self.sink.lock().unwrap().push(OpReport { id, sid, name, outcome });
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
mod tests;
#[cfg(test)]
mod job_tests;

#[cfg(test)]
mod submit_deadline_tests;

//! Input the shell types into the pane (`PaneType`): queued in arrival
//! order, one at a time, each answered with `PaneTyped`.
//!
//! The PTY is written through a queue (see `LocalSession::write`), so a
//! write that returned has only been queued.  Input counts as written
//! once its last byte has left the queue; the writer thread wakes the
//! loop then.  A return that follows the input waits for the input's
//! echo, or 50 ms, so that it lands after the redraw the input caused,
//! as the submit step of a scripted input always has.
//!
//! Whether a paste gets markers is the program's bracketed paste mode,
//! and a return typed before it may change that mode: zsh turns it off
//! as it takes the line, and a line that was `exec bash` leaves a shell
//! that never turns it on.  Until the program has answered that return,
//! the mode in hand is the one from before it, so a paste waits.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use marspot_term::shell_proto::{DoneCode, PaneType, PaneTyped, TypedResult};

use crate::SessionImpl;
use crate::local_session::Written;
use crate::pane_ctl::{local, local_mut, shell_in_front};
use crate::watch::Watches;

/// Requests held at once, the one being typed included.  The control
/// reader's channel has no bound, and input with a return takes up to
/// 50 ms: several callers can send faster than it is typed.
const QUEUE: usize = 8;
/// Input not written by then is not being read by the pane's program.
const NOT_READ_AFTER: Duration = Duration::from_secs(2);
const ECHO_WAIT: Duration = Duration::from_millis(50);

enum Stage {
    Writing,
    /// The input was written then, at that output position: output
    /// past it is the echo.
    Echo { written: Instant, pos: u64 },
}

/// A return typed then, at that output position.
#[derive(Clone, Copy)]
struct Returned {
    at: Instant,
    pos: u64,
}

/// A watch slot a request has reserved: (slot, watch id).
pub(crate) type Reserved = (usize, u32);

struct InFlight {
    req_id: u32,
    enter: bool,
    watch: Option<Reserved>,
    /// Bytes handed to the PTY, markers included.
    len: usize,
    queued_at: Instant,
    /// The output position when the input was queued.
    pos: u64,
    echo_off: bool,
    /// The input itself has a return the program will act on: one
    /// outside paste markers.
    has_return: bool,
    stage: Stage,
    /// Set by the PTY's writer thread once the input left its queue.
    written: Arc<AtomicBool>,
}

pub(crate) enum Step {
    /// The answer, and the watch slot reserved with it: armed when the
    /// input was written, freed otherwise.
    Typed(PaneTyped, Option<Reserved>),
    /// The embedded watch could not be registered: nothing was written.
    Refused(u32, (DoneCode, &'static str)),
}

pub(crate) struct Typing {
    queue: [Option<PaneType>; QUEUE],
    head: usize,
    len: usize,
    cur: Option<InFlight>,
    /// The latest return typed, until the next input starts.
    returned: Option<Returned>,
    /// Wakes the loop; the writer thread calls it when input is written.
    wake: Arc<dyn Fn() + Send + Sync>,
}

fn typed(req_id: u32, result: TypedResult, pos: u64, written: usize, echo_off: bool) -> PaneTyped {
    PaneTyped { req_id, sid: 0, result, echo_off, pos, written: written as u32 }
}

impl Typing {
    pub(crate) fn new(wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self { queue: Default::default(), head: 0, len: 0, cur: None, returned: None, wake }
    }

    pub(crate) fn idle(&self) -> bool {
        self.cur.is_none() && self.len == 0
    }

    /// Queue `t`; the refusal when the queue is full.
    pub(crate) fn queue(&mut self, t: PaneType) -> Option<PaneTyped> {
        if self.len + usize::from(self.cur.is_some()) >= QUEUE {
            return Some(typed(t.req_id, TypedResult::QueueFull, 0, 0, false));
        }
        self.queue[(self.head + self.len) % QUEUE] = Some(t);
        self.len += 1;
        None
    }

    /// Every request held, taken out for a restart: the one in flight
    /// whose input already left the PTY queue is answered as written,
    /// without its return (the caller must not type it again); the rest
    /// are returned unanswered.
    pub(crate) fn abort(&mut self, session: &SessionImpl) -> (Option<PaneTyped>, Vec<u32>) {
        let mut ids = Vec::new();
        let mut done = None;
        if let Some(c) = self.cur.take() {
            if c.written.load(Ordering::Acquire) {
                let pos = local(session).output_pos();
                done = Some(typed(c.req_id, TypedResult::Written, pos, c.len, c.echo_off));
            } else {
                ids.push(c.req_id);
            }
        }
        while self.len > 0 {
            if let Some(t) = self.queue[self.head].take() {
                ids.push(t.req_id);
            }
            self.head = (self.head + 1) % QUEUE;
            self.len -= 1;
        }
        (done, ids)
    }

    /// Move the request in flight along, or start the next; what was
    /// decided, if anything.
    pub(crate) fn step(&mut self, session: &mut SessionImpl, watches: &mut Watches) -> Option<Step> {
        if self.cur.is_none() {
            if self.len == 0 || self.paste_waits(session) {
                return None;
            }
            let t = self.queue[self.head].take()?;
            self.returned = None;
            self.head = (self.head + 1) % QUEUE;
            self.len -= 1;
            if let Some(step) = self.start(t, session, watches) {
                return Some(step);
            }
        }
        let step = self.advance(session)?;
        self.cur = None;
        Some(step)
    }

    /// The next request is a paste into a program in bracketed paste
    /// mode that has not answered the return typed before it: answered
    /// is the mode turned off, or output past the return once the echo
    /// wait is over.  A program that reads nothing is not waited for
    /// past the time input is given to be read.
    fn paste_waits(&self, session: &SessionImpl) -> bool {
        let (Some(r), Some(t)) = (self.returned, self.queue[self.head].as_ref()) else { return false };
        if !t.paste || !session.terminal().bracketed_paste_mode() {
            return false;
        }
        let since = r.at.elapsed();
        let answered = local(session).output_pos() > r.pos && since >= ECHO_WAIT;
        !answered && since < NOT_READ_AFTER
    }

    fn start(&mut self, t: PaneType, session: &mut SessionImpl, watches: &mut Watches) -> Option<Step> {
        let watch = match t.watch {
            Some(w) => match watches.reserve(w, session) {
                Ok(slot) => Some(slot),
                Err(code) => return Some(Step::Refused(t.req_id, code)),
            },
            None => None,
        };
        let pos = local(session).output_pos();
        if t.only_at_shell && !shell_in_front(session) {
            return Some(Step::Typed(typed(t.req_id, TypedResult::NotAtShell, pos, 0, false), watch));
        }
        let mut wrapped = Vec::new();
        let marked = t.paste && session.terminal().bracketed_paste_mode();
        let bytes = if marked {
            marspot_term::paste::bracketed_paste(&t.bytes, &mut wrapped);
            &wrapped
        } else {
            &t.bytes
        };
        let has_return = !marked && bytes.iter().any(|&b| b == b'\r' || b == b'\n');
        let echo_off = echo_off(local(session).raw_master());
        let flag = Arc::new(AtomicBool::new(false));
        let (set, wake) = (Arc::clone(&flag), Arc::clone(&self.wake));
        let on_written: Written = Arc::new(move || {
            set.store(true, Ordering::Release);
            wake();
        });
        if local_mut(session).write_then(bytes, Some(on_written)).is_err() {
            return Some(Step::Typed(typed(t.req_id, TypedResult::PtyQueueFull, pos, 0, echo_off), watch));
        }
        self.cur = Some(InFlight {
            req_id: t.req_id,
            enter: t.enter,
            watch,
            len: bytes.len(),
            queued_at: Instant::now(),
            pos,
            echo_off,
            has_return,
            stage: Stage::Writing,
            written: flag,
        });
        None
    }

    fn advance(&mut self, session: &mut SessionImpl) -> Option<Step> {
        let cur = self.cur.as_mut()?;
        let now = Instant::now();
        if let Stage::Writing = cur.stage {
            if !cur.written.load(Ordering::Acquire) {
                let late = now.duration_since(cur.queued_at) >= NOT_READ_AFTER;
                let written = cur.len - session.pty_write_backlog().min(cur.len);
                let r = typed(cur.req_id, TypedResult::NotReading, cur.pos, written, cur.echo_off);
                return late.then(|| Step::Typed(r, cur.watch.take()));
            }
            let pos = local(session).output_pos();
            if !cur.enter {
                if cur.has_return {
                    self.returned = Some(Returned { at: now, pos });
                }
                let r = typed(cur.req_id, TypedResult::Written, pos, cur.len, cur.echo_off);
                return Some(Step::Typed(r, cur.watch.take()));
            }
            cur.stage = Stage::Echo { written: now, pos };
        }
        let Stage::Echo { written, pos: at_written } = cur.stage else { return None };
        let pos = local(session).output_pos();
        if pos == at_written && now < written + ECHO_WAIT {
            return None;
        }
        if session.write(b"\r").is_err() {
            let r = typed(cur.req_id, TypedResult::NotReading, cur.pos, cur.len, cur.echo_off);
            return Some(Step::Typed(r, cur.watch.take()));
        }
        let r = typed(cur.req_id, TypedResult::Written, pos, cur.len + 1, cur.echo_off);
        let watch = cur.watch.take();
        self.returned = Some(Returned { at: now, pos });
        Some(Step::Typed(r, watch))
    }

    /// When the request in flight has to be looked at again: its
    /// deadline.  Its input being written wakes the loop sooner.
    /// A paste held for a return's answer is looked at again when the
    /// echo wait is over and when it stops waiting; output past the
    /// return wakes the loop anyway.
    pub(crate) fn due_in(&self, now: Instant) -> Option<Duration> {
        let Some(cur) = self.cur.as_ref() else {
            let r = self.returned.filter(|_| self.len > 0)?;
            let echo = r.at + ECHO_WAIT;
            let at = if now < echo { echo } else { r.at + NOT_READ_AFTER };
            return Some(at.saturating_duration_since(now));
        };
        Some(match cur.stage {
            Stage::Writing => (cur.queued_at + NOT_READ_AFTER).saturating_duration_since(now),
            Stage::Echo { written, .. } => (written + ECHO_WAIT).saturating_duration_since(now),
        })
    }
}

/// The terminal reads lines with echo off: a password prompt.
///
/// Echo off alone is not that.  A line editor (zle, readline) and every
/// full-screen program turn line mode and echo off together and draw
/// what is typed themselves, so a pane at a shell prompt would always
/// say echo off.
fn echo_off(master: std::os::fd::RawFd) -> bool {
    // SAFETY: termios is plain data, filled by tcgetattr on a live fd
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    let got = unsafe { libc::tcgetattr(master, &mut t) } == 0;
    got && t.c_lflag & libc::ICANON != 0 && t.c_lflag & libc::ECHO == 0
}

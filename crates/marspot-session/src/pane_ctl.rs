//! The session's half of pane control: input the shell types into the
//! pane (`typing`) and conditions it waits for (`watch`), with the
//! output position both are measured in.
//!
//! Idle costs nothing: with no input in flight and no watch registered,
//! a loop turn compares the output position with the last one it saw
//! and returns, and nothing is added to the loop's wake-up time.

use std::time::{Duration, Instant};

use marspot_term::shell_proto::{
    DoneCode, Frame, MsgType, PaneDone, PaneType, PaneTyped, PaneWatch, TypedResult, WatchReason, encode_pane_done,
    encode_pane_typed,
};

use std::sync::mpsc::Sender;

use crate::control_writer::ControlWriter;
use crate::{SessionEvent, SessionImpl};
use crate::track::Track;
use crate::typing::{Step, Typing};
use crate::watch::Watches;

/// Longest an ending image waits for its answers to leave.
const DRAIN: Duration = Duration::from_millis(200);
const RESTARTING: (DoneCode, &str) = (DoneCode::Other, "restarting");

/// A request L2 forwarded from the shell.
pub(crate) enum Request {
    Type(PaneType),
    Watch(PaneWatch),
}

/// What one image hands the next so that positions never go back: the
/// output position and the latest command end (its position, status).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Marks {
    pub(crate) pos: u64,
    pub(crate) command_end: Option<(u64, Option<u8>)>,
}

pub(crate) struct PaneCtl {
    sid: u64,
    typing: Typing,
    watches: Watches,
    track: Track,
}

impl PaneCtl {
    /// `wake` is the loop's: input leaving the PTY's queue wakes it.
    pub(crate) fn new(session: &SessionImpl, command_end: Option<(u64, Option<u8>)>, wake: Sender<SessionEvent>) -> Self {
        Self {
            sid: session.id(),
            typing: Typing::new(std::sync::Arc::new(move || {
                let _ = wake.send(SessionEvent::Wake);
            })),
            watches: Watches::new(session.id()),
            track: Track::new(session, command_end),
        }
    }

    /// Check waiting text watches on the grid after every batch.
    #[cfg(test)]
    pub(crate) fn check_every_batch(&mut self) {
        self.watches.check_every_batch();
    }

    pub(crate) fn marks(&self, session: &SessionImpl) -> Marks {
        Marks { pos: local(session).output_pos(), command_end: self.track.command_end() }
    }

    pub(crate) fn request(&mut self, r: Request, session: &mut SessionImpl, mut poke: Option<&mut ControlWriter>) {
        match r {
            Request::Watch(w) => {
                let code = self.watches.register(w.watch, session, &mut self.track);
                self.done(w.req_id, code, poke.as_deref_mut());
                self.watches.send_fired(poke);
            }
            Request::Type(t) => {
                if let Some(mut reply) = self.typing.queue(t) {
                    reply.pos = local(session).output_pos();
                    self.typed(reply, poke.as_deref_mut());
                }
                self.step(session, poke);
            }
        }
    }

    /// After the loop's turn of parsing: note what the batch did, move
    /// input along, look at the watches, and send what is owed.
    pub(crate) fn after_turn(&mut self, session: &mut SessionImpl, procs_changed: bool, mut poke: Option<&mut ControlWriter>) {
        let batch = self.track.after_batch(session);
        if self.typing.idle() && self.watches.is_empty() {
            return;
        }
        self.step(session, poke.as_deref_mut());
        self.watches.evaluate(session, &mut self.track, batch, procs_changed);
        self.watches.send_fired(poke);
    }

    /// When the loop must look again for input in flight or a watch
    /// that becomes true with time; `None` when nothing is owed.
    pub(crate) fn due_in(&self) -> Option<Duration> {
        let now = Instant::now();
        [self.typing.due_in(now), self.watches.due_in(&self.track, now)].into_iter().flatten().min()
    }

    /// The image is about to be replaced, its last output parsed: input
    /// not typed yet is answered, every waiting watch is told, and the
    /// answers leave before the image goes.  A report the writer
    /// refuses is offered again until it is taken or time is up.
    pub(crate) fn restarting(&mut self, session: &mut SessionImpl, mut poke: Option<&mut ControlWriter>) {
        let batch = self.track.after_batch(session);
        self.watches.evaluate(session, &mut self.track, batch, true);
        for req_id in self.typing.abort() {
            self.done(req_id, RESTARTING, poke.as_deref_mut());
        }
        self.watches.release_all();
        self.watches.end_all(WatchReason::Restarting, &self.track);
        let until = Instant::now() + DRAIN;
        while let Some(w) = poke.as_deref_mut() {
            self.watches.send_fired(Some(&mut *w));
            if (!self.watches.unsent() && w.backlog() == 0) || Instant::now() >= until {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A request that reached this image after its loop ended.
    pub(crate) fn refuse_restarting(&self, r: Request, poke: Option<&mut ControlWriter>) {
        let req_id = match r {
            Request::Type(t) => t.req_id,
            Request::Watch(w) => w.req_id,
        };
        self.done(req_id, RESTARTING, poke);
    }

    /// The pane's shell exited and its last output is parsed.
    pub(crate) fn session_ended(&mut self, session: &mut SessionImpl, mut poke: Option<&mut ControlWriter>) {
        let batch = self.track.after_batch(session);
        self.watches.evaluate(session, &mut self.track, batch, true);
        self.watches.end_all(WatchReason::SessionEnded, &self.track);
        self.watches.send_fired(poke.as_deref_mut());
        drain(poke);
    }

    fn step(&mut self, session: &mut SessionImpl, mut poke: Option<&mut ControlWriter>) {
        while let Some(step) = self.typing.step(session, &mut self.watches) {
            match step {
                Step::Typed(reply, watch) => {
                    self.track.mark(session);
                    self.typed(reply, poke.as_deref_mut());
                    match watch {
                        Some(w) if reply.result == TypedResult::Written => {
                            self.watches.activate(w, reply.pos, session, &mut self.track)
                        }
                        Some(w) => self.watches.release(w),
                        None => {}
                    }
                }
                Step::Refused(req_id, code) => self.done(req_id, code, poke.as_deref_mut()),
            }
        }
        self.watches.send_fired(poke);
    }

    fn typed(&self, mut reply: PaneTyped, poke: Option<&mut ControlWriter>) {
        reply.sid = self.sid;
        if let Some(w) = poke {
            let _ = w.send(Frame::new(MsgType::PaneTyped, encode_pane_typed(&reply)));
        }
    }

    fn done(&self, req_id: u32, code: (DoneCode, &'static str), poke: Option<&mut ControlWriter>) {
        let done = PaneDone { req_id, code: code.0, text: code.1.to_string() };
        if let (Some(w), Ok(p)) = (poke, encode_pane_done(&done)) {
            let _ = w.send(Frame::new(MsgType::PaneDone, p));
        }
    }
}

pub(crate) fn local(session: &SessionImpl) -> &crate::local_session::LocalSession {
    let SessionImpl::Local(s) = session;
    s
}

pub(crate) fn local_mut(session: &mut SessionImpl) -> &mut crate::local_session::LocalSession {
    let SessionImpl::Local(s) = session;
    s
}

/// Whether the pane's own shell has the terminal's foreground.
pub(crate) fn shell_in_front(session: &SessionImpl) -> bool {
    let s = local(session);
    // SAFETY: plain queries on the pty master and the shell's pid
    let fg = unsafe { libc::tcgetpgrp(s.raw_master()) };
    fg > 0 && fg == unsafe { libc::getpgid(s.child_pid()) }
}

/// Let what is queued for L2 leave before this image or process ends:
/// a writer thread dies with it, and its queue with it.
fn drain(poke: Option<&mut ControlWriter>) {
    let Some(w) = poke else { return };
    let until = Instant::now() + DRAIN;
    while w.backlog() > 0 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(test)]
#[path = "watch_perf_tests.rs"]
mod watch_perf_tests;

//! One connection's side of a request to the core: send through the
//! main loop and sleep on the answer until a deadline.  The waits built
//! on that are in `wait`.

use std::os::fd::RawFd;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use marspot::shell_proto::{DoneCode, MsgType, PaneType, PaneWatchFired, encode_pane_type};
use marspot_peer::control::{ErrorCode, Reject};

use super::caller::Peer;
use super::{Hub, MainCmd, Reply};

/// How long an answer to anything but `PaneSpawn` may take.
pub(super) const ANSWER_WITHIN: Duration = Duration::from_secs(3);
/// What a session answers a request that reached an image on its way
/// out (`PaneDone` code `Other` with this text): ask again shortly.
const RESTARTING_TEXT: &str = "restarting";
/// What the core answers a move whose session did not answer in time.
const UNANSWERED_TEXT: &str = "unanswered";
/// What the core answers a resize of a pane in a window.
const NOT_RESIZABLE_TEXT: &str = "not_resizable";

pub(super) enum Asked {
    /// Nothing came back in time: it may or may not have been done.
    NoAnswer,
    Refused(Reject),
}

pub(super) struct Conn<'h> {
    pub hub: &'h Hub,
    pub peer: Peer,
    pub(super) tx: Sender<Reply>,
    pub(super) rx: Receiver<Reply>,
    /// Reports for this connection's registered watches that came while
    /// it waited for something else.
    pub(super) fired: Vec<PaneWatchFired>,
    /// The caller's socket, watched for a hang-up during long waits.
    pub(super) sock: Option<RawFd>,
    /// Whether a session is still running; the registry's answer.
    pub(super) alive: fn(u64) -> bool,
    /// The longest a watch is registered again and again while its
    /// session is between images.
    pub(super) regrace: Duration,
}

/// A refusal for a `PaneDone` code.
pub(super) fn done_reject(code: DoneCode, text: &str) -> Reject {
    let (c, m) = match code {
        DoneCode::NotAttached => (ErrorCode::NotAttached, "the pane's window is closed or its session is not connected"),
        DoneCode::NoSuchSession => (ErrorCode::NoSuchPane, "the core has no such session"),
        DoneCode::WatchTableFull => (ErrorCode::WatchTableFull, "the pane has as many waits as it holds"),
        DoneCode::OldSession => (ErrorCode::OldSession, "the pane's session or the core is an older build; it can after the next update"),
        DoneCode::Other if text == UNANSWERED_TEXT => {
            return Reject::new(ErrorCode::Unknown, "the pane's session did not answer in time; the pane stays where it was, check with pane.list");
        }
        DoneCode::Other if text == NOT_RESIZABLE_TEXT => (ErrorCode::NotResizable, "a pane in a window takes its size from the window's layout"),
        DoneCode::Other if text == marspot::shell_proto::USER_TYPING_TEXT => return super::dispatch::view_typing_reject(),
        DoneCode::Other if text == marspot::shell_proto::IN_A_WINDOW_TEXT => return Reject::new(ErrorCode::BadRequest, "refused: in a window"),
        DoneCode::Other if text == RESTARTING_TEXT => {
            return Reject::new(ErrorCode::CoreRestarting, "the pane's session is moving to a new build; nothing was done").retry_after(200);
        }
        _ => (ErrorCode::Internal, "the core refused it"),
    };
    let message = if text.is_empty() { m.to_string() } else { format!("{m}: {text}") };
    Reject::new(c, message)
}

impl<'h> Conn<'h> {
    pub fn new(hub: &'h Hub, peer: Peer, sock: Option<RawFd>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self { hub, peer, tx, rx, fired: Vec::new(), sock, alive: super::dispatch::alive, regrace: Duration::from_secs(30) }
    }

    /// The core does what `bit` says, or why not.
    pub fn need_caps(&self, bit: u32) -> Result<(), Reject> {
        if self.hub.caps() & bit != 0 {
            return Ok(());
        }
        let hello = self.hub.hello_ms.load(std::sync::atomic::Ordering::Acquire);
        if hello == 0 || self.hub.now_ms().saturating_sub(hello) < 2_000 {
            return Err(Reject::new(ErrorCode::CoreRestarting, "the core is starting").retry_after(500));
        }
        Err(Reject::new(ErrorCode::OldSession, "the running core does not do this yet; it will after the next update"))
    }

    /// Send a frame to the core and wait for the answer to it.
    pub fn ask(
        &mut self,
        sid: u64,
        msg: MsgType,
        build: impl Fn(u32) -> std::io::Result<Vec<u8>>,
        guard_ops: bool,
        within: Duration,
    ) -> Result<Reply, Asked> {
        let req_id = self.hub.links.req_id();
        let payload = build(req_id).map_err(|e| Asked::Refused(Reject::new(ErrorCode::Internal, format!("{e}"))))?;
        self.hub.links.expect(req_id, self.tx.clone());
        let queued = self.hub.queue(MainCmd::Send { sid, msg, payload, guard_ops, reply: Some((req_id, self.tx.clone())) });
        let out = if queued { self.answer(req_id, Instant::now() + within) } else { Err(Asked::Refused(shutting_down())) };
        self.hub.links.unexpect(req_id);
        out
    }

    fn answer(&mut self, req_id: u32, deadline: Instant) -> Result<Reply, Asked> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(Reply::Fired(f)) => self.keep_fired(f),
                Ok(Reply::HungUp) => return Err(Asked::Refused(hung_up())),
                Ok(Reply::Refused(id, r)) if id == req_id => return Err(Asked::Refused(r)),
                Ok(r) if reply_id(&r) == Some(req_id) => return Ok(r),
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => return Err(Asked::NoAnswer),
                Err(RecvTimeoutError::Disconnected) => return Err(Asked::Refused(shutting_down())),
            }
        }
    }

    /// The pane's output position now: an empty input, answered with
    /// the position it was written at.  None when the core cannot say
    /// within `within`.
    pub fn probe_pos(&mut self, sid: u64, within: Duration) -> Option<u64> {
        if self.hub.caps() & marspot::shell_proto::CAP_TYPE_AND_WATCH == 0 {
            return None;
        }
        let build = |req_id| {
            encode_pane_type(&PaneType { req_id, sid, paste: false, enter: false, only_at_shell: false, bytes: Vec::new(), watch: None })
        };
        match self.ask(sid, MsgType::PaneType, build, false, within) {
            Ok(Reply::Typed(t)) if t.result == marspot::shell_proto::TypedResult::Written => Some(t.pos),
            _ => None,
        }
    }

    /// Keep a report for later, if its watch is still this connection's.
    pub(super) fn keep_fired(&mut self, f: PaneWatchFired) {
        if self.hub.links.has_watch(f.watch_id) {
            self.fired.push(f);
        }
    }

    /// Send a frame nobody waits for the answer to.
    pub fn cast(&self, sid: u64, msg: MsgType, payload: Vec<u8>) {
        self.hub.queue(MainCmd::Send { sid, msg, payload, guard_ops: false, reply: None });
    }
}

fn reply_id(r: &Reply) -> Option<u32> {
    match r {
        Reply::Typed(t) => Some(t.req_id),
        Reply::Done(d) => Some(d.req_id),
        Reply::Spawned(s) => Some(s.req_id),
        Reply::Fired(_) | Reply::Refused(..) | Reply::HungUp => None,
    }
}

pub(super) fn hung_up() -> Reject {
    Reject::new(ErrorCode::Internal, "the caller hung up")
}

fn shutting_down() -> Reject {
    Reject::new(ErrorCode::CoreRestarting, "the shell is not taking requests").retry_after(1_000)
}

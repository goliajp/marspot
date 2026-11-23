//! One connection's side of a request to the core: send through the
//! main loop, sleep on the answer until a deadline, and the waits built
//! on that.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use marspot::shell_proto::{
    DoneCode, MsgType, PaneType, PaneWatch, PaneWatchFired, WatchBody, WatchKind, WatchReason, encode_pane_type,
    encode_pane_watch,
};
use marspot_peer::control::{ErrorCode, Reject};

use super::caller::{CallerKey, Peer};
use super::{Hub, MainCmd, Reply};

/// How long an answer to anything but `PaneSpawn` may take.
pub(super) const ANSWER_WITHIN: Duration = Duration::from_secs(3);
/// How soon to register a watch again while its session is between
/// two images.
const REREGISTER_EVERY: Duration = Duration::from_millis(200);

pub(super) enum Asked {
    /// Nothing came back in time: it may or may not have been done.
    NoAnswer,
    Refused(Reject),
}

pub(super) struct Conn<'h> {
    pub hub: &'h Hub,
    pub peer: Peer,
    tx: Sender<Reply>,
    rx: Receiver<Reply>,
    /// Watch reports that came while waiting for something else.
    fired: Vec<PaneWatchFired>,
}

/// A refusal for a `PaneDone` code.
pub(super) fn done_reject(code: DoneCode, text: &str) -> Reject {
    let (c, m) = match code {
        DoneCode::NotAttached => (ErrorCode::NotAttached, "the pane's window is closed or its session is not connected"),
        DoneCode::NoSuchSession => (ErrorCode::NoSuchPane, "the core has no such session"),
        DoneCode::WatchTableFull => (ErrorCode::WatchTableFull, "the pane has as many waits as it holds"),
        DoneCode::OldSession => (ErrorCode::OldSession, "the pane's session or the core is an older build; it can after the next update"),
        _ => (ErrorCode::Internal, "the core refused it"),
    };
    let message = if text.is_empty() { m.to_string() } else { format!("{m}: {text}") };
    Reject::new(c, message)
}

impl<'h> Conn<'h> {
    pub fn new(hub: &'h Hub, peer: Peer) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self { hub, peer, tx, rx, fired: Vec::new() }
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
                Ok(Reply::Fired(f)) => self.fired.push(f),
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

    /// Send a frame nobody waits for the answer to.
    pub fn cast(&self, sid: u64, msg: MsgType, payload: Vec<u8>) {
        self.hub.queue(MainCmd::Send { sid, msg, payload, guard_ops: false, reply: None });
    }

    fn next_fired(&mut self, id: u32, deadline: Instant) -> Option<PaneWatchFired> {
        if let Some(i) = self.fired.iter().position(|f| f.watch_id == id) {
            return Some(self.fired.remove(i));
        }
        loop {
            match self.rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Reply::Fired(f)) if f.watch_id == id => return Some(f),
                Ok(_) => {}
                Err(_) => return None,
            }
        }
    }

    /// Register watch `body` on `sid`, from `since` or from now; the
    /// position it counts from (`None` when `since` was given).
    fn register(&mut self, sid: u64, body: &WatchBody, since: Option<u64>) -> Result<Option<u64>, Reject> {
        for attempt in 0..2 {
            let asked = match since {
                Some(s) => {
                    let mut b = body.clone();
                    b.since_pos = s;
                    self.ask(sid, MsgType::PaneWatch, |req_id| encode_pane_watch(&PaneWatch { req_id, sid, watch: b.clone() }), false, ANSWER_WITHIN)
                }
                // nothing written, so the watch counts from this moment
                None => self.ask(sid, MsgType::PaneType, |req_id| encode_pane_type(&empty_type(req_id, sid, body)), false, ANSWER_WITHIN),
            };
            match asked {
                Ok(Reply::Done(d)) if d.code == DoneCode::Ok => return Ok(None),
                Ok(Reply::Done(d)) => return Err(done_reject(d.code, &d.text)),
                Ok(Reply::Typed(t)) if t.result == marspot::shell_proto::TypedResult::Written => return Ok(Some(t.pos)),
                Ok(Reply::Typed(_)) => return Err(Reject::new(ErrorCode::PaneBusy, "the pane has 8 inputs queued")),
                Ok(_) => return Err(Reject::new(ErrorCode::Internal, "an answer of the wrong kind")),
                Err(Asked::Refused(r)) => return Err(r),
                Err(Asked::NoAnswer) if attempt == 0 => continue,
                Err(Asked::NoAnswer) => {}
            }
        }
        Err(Reject::new(ErrorCode::Unknown, "the core did not answer the wait's registration"))
    }

    /// Wait on `sid` for `kind` until `deadline`: the report, or None at
    /// the deadline.  Registered again (same id, same start) while the
    /// session restarts; cancelled at the end either way.
    pub fn wait(
        &mut self,
        sid: u64,
        caller: CallerKey,
        body: WatchBody,
        since: Option<u64>,
        deadline: Instant,
    ) -> Result<Option<PaneWatchFired>, Reject> {
        self.need_caps(marspot::shell_proto::CAP_TYPE_AND_WATCH)?;
        let body = self.add_watch(sid, caller, body)?;
        let out = self.register(sid, &body, since).and_then(|base| self.wait_registered(sid, &body, base.or(since).unwrap_or(0), deadline));
        self.end_watch(sid, body.watch_id);
        out
    }

    /// Take a watch id for `body` within the caller's and the table's share.
    pub fn add_watch(&mut self, sid: u64, caller: CallerKey, body: WatchBody) -> Result<WatchBody, Reject> {
        let cfg = self.hub.config();
        let id = self.hub.links.add_watch(sid, caller, body.clone(), self.tx.clone(), (cfg.watches_per_caller, cfg.watches_total))?;
        Ok(WatchBody { watch_id: id, ..body })
    }

    /// Forget watch `id` here and cancel it in the session.
    pub fn end_watch(&mut self, sid: u64, id: u32) {
        self.hub.links.drop_watch(id);
        let cancel = WatchBody { watch_id: id, kind: WatchKind::Cancel, since_pos: 0, quiet_ms: 0, text: String::new() };
        let req_id = self.hub.links.req_id();
        if let Ok(p) = encode_pane_watch(&PaneWatch { req_id, sid, watch: cancel }) {
            self.cast(sid, MsgType::PaneWatch, p);
        }
        self.fired.retain(|f| f.watch_id != id);
    }

    /// Sleep on a registered watch counting from `base`.
    pub fn wait_registered(&mut self, sid: u64, body: &WatchBody, base: u64, deadline: Instant) -> Result<Option<PaneWatchFired>, Reject> {
        self.hub.links.set_since(body.watch_id, base);
        let body = &WatchBody { since_pos: base, ..body.clone() };
        loop {
            let Some(f) = self.next_fired(body.watch_id, deadline) else { return Ok(None) };
            if f.reason != WatchReason::Restarting {
                return Ok(Some(f));
            }
            // the same id and start, until the new image takes it or time is up
            loop {
                match self.register(sid, body, Some(base)) {
                    Ok(_) => break,
                    Err(r) if matches!(r.code, ErrorCode::OldSession | ErrorCode::NotAttached | ErrorCode::Unknown) => {
                        if Instant::now() + REREGISTER_EVERY >= deadline {
                            return Ok(None);
                        }
                        std::thread::sleep(REREGISTER_EVERY);
                    }
                    Err(r) => return Err(r),
                }
            }
        }
    }
}

fn empty_type(req_id: u32, sid: u64, body: &WatchBody) -> PaneType {
    PaneType { req_id, sid, paste: false, enter: false, only_at_shell: false, bytes: Vec::new(), watch: Some(body.clone()) }
}

fn reply_id(r: &Reply) -> Option<u32> {
    match r {
        Reply::Typed(t) => Some(t.req_id),
        Reply::Done(d) => Some(d.req_id),
        Reply::Spawned(s) => Some(s.req_id),
        Reply::Fired(_) | Reply::Refused(..) => None,
    }
}

fn shutting_down() -> Reject {
    Reject::new(ErrorCode::CoreRestarting, "the shell is not taking requests").retry_after(1_000)
}

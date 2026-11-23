//! Waits: register a watch in the session, sleep until its report or
//! the deadline, and carry it across the session moving to a new image.
//!
//! While asleep the connection's socket is watched for the caller
//! hanging up (`hangup`), so a caller that went away does not hold its
//! connection, its share of waits and a slot in the session.

use std::time::{Duration, Instant};

use marspot::shell_proto::{
    CAP_TYPE_AND_WATCH, DoneCode, MsgType, PaneType, PaneWatch, PaneWatchFired, TypedResult, WatchBody, WatchKind,
    WatchReason, encode_pane_type, encode_pane_watch,
};
use marspot_peer::control::{ErrorCode, Reject};

use super::caller::CallerKey;
use super::conn::{ANSWER_WITHIN, Asked, Conn, done_reject, hung_up};
use super::{Reply, hangup};

/// How soon to register a watch again while its session is between
/// two images.
const REREGISTER_EVERY: Duration = Duration::from_millis(200);

/// Answers that mean the session has not taken the watch yet but may.
fn between_images(code: ErrorCode) -> bool {
    matches!(code, ErrorCode::CoreRestarting | ErrorCode::OldSession | ErrorCode::NotAttached | ErrorCode::Unknown)
}

/// A report the session did not send: what the connection concluded.
fn concluded(sid: u64, id: u32, reason: WatchReason, pos: u64) -> PaneWatchFired {
    PaneWatchFired { sid, watch_id: id, reason, pos, status: None, line: None }
}

impl Conn<'_> {
    fn next_fired(&mut self, id: u32, deadline: Instant) -> Result<Option<PaneWatchFired>, Reject> {
        if let Some(i) = self.fired.iter().position(|f| f.watch_id == id) {
            return Ok(Some(self.fired.remove(i)));
        }
        loop {
            match self.rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Reply::Fired(f)) if f.watch_id == id => return Ok(Some(f)),
                Ok(Reply::Fired(f)) => self.keep_fired(f),
                Ok(Reply::HungUp) => return Err(hung_up()),
                Ok(_) => {}
                Err(_) => return Ok(None),
            }
        }
    }

    /// Sleep until `until`, or until the caller hangs up.
    fn pause(&mut self, until: Instant) -> Result<(), Reject> {
        loop {
            match self.rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok(Reply::HungUp) => return Err(hung_up()),
                Ok(Reply::Fired(f)) => self.keep_fired(f),
                Ok(_) => {}
                Err(_) => return Ok(()),
            }
        }
    }

    /// Register watch `body` on `sid`, from `since` or from now; the
    /// position it counts from (`None` when `since` was given).
    fn register(&mut self, sid: u64, body: &WatchBody, since: Option<u64>) -> Result<Option<u64>, Reject> {
        if let Some(s) = since {
            self.hub.links.set_since(body.watch_id, s);
        }
        for attempt in 0..2 {
            let asked = match since {
                Some(s) => {
                    let b = WatchBody { since_pos: s, ..body.clone() };
                    self.ask(sid, MsgType::PaneWatch, |req_id| encode_pane_watch(&PaneWatch { req_id, sid, watch: b.clone() }), false, ANSWER_WITHIN)
                }
                // nothing written, so the watch counts from this moment
                None => self.ask(sid, MsgType::PaneType, |req_id| encode_pane_type(&empty_type(req_id, sid, body)), false, ANSWER_WITHIN),
            };
            match asked {
                Ok(Reply::Done(d)) if d.code == DoneCode::Ok => return Ok(None),
                Ok(Reply::Done(d)) => return Err(done_reject(d.code, &d.text)),
                Ok(Reply::Typed(t)) if t.result == TypedResult::Written => return Ok(Some(t.pos)),
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
        self.need_caps(CAP_TYPE_AND_WATCH)?;
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
        let _armed = self.sock.and_then(|fd| hangup::watch(fd, self.tx.clone()));
        let body = &WatchBody { since_pos: base, ..body.clone() };
        loop {
            let Some(f) = self.next_fired(body.watch_id, deadline)? else { return Ok(None) };
            if f.reason != WatchReason::Restarting {
                return Ok(Some(f));
            }
            if let Some(end) = self.reregister(sid, body, base, deadline)? {
                return Ok(end);
            }
        }
    }

    /// Register a watch again (same id, same start) after its session
    /// said it is moving to a new image: `None` once the new image took
    /// it; else what to answer.  Gives up when the session ends, at the
    /// deadline, or after [`Conn::regrace`] (answered as `restarting`).
    fn reregister(&mut self, sid: u64, body: &WatchBody, base: u64, deadline: Instant) -> Result<Option<Option<PaneWatchFired>>, Reject> {
        let give_up = Instant::now() + self.regrace;
        loop {
            if !(self.alive)(sid) {
                return Ok(Some(Some(concluded(sid, body.watch_id, WatchReason::SessionEnded, base))));
            }
            match self.register(sid, body, Some(base)) {
                Ok(_) => return Ok(None),
                Err(r) if between_images(r.code) => {}
                Err(r) => return Err(r),
            }
            let next = Instant::now() + REREGISTER_EVERY;
            if next >= deadline {
                return Ok(Some(None));
            }
            if next >= give_up {
                return Ok(Some(Some(concluded(sid, body.watch_id, WatchReason::Restarting, base))));
            }
            self.pause(next)?;
        }
    }
}

fn empty_type(req_id: u32, sid: u64, body: &WatchBody) -> PaneType {
    PaneType { req_id, sid, paste: false, enter: false, only_at_shell: false, bytes: Vec::new(), watch: Some(body.clone()) }
}

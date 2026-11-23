//! `helper.*`: served by whoever registered for them (the helpers
//! plugin), so this module knows nothing of agents.  The service gets a
//! [`Port`]: the pane operations a request may use, counted against the
//! caller that asked, or against marspot itself for work no connection
//! asked for ([`with_port`]).

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use marspot::pidtree::ProcRow;
use marspot::shell_proto::{CAP_SHADOWS, MsgType, PaneRetire, PaneType, RetireMode, WatchBody, WatchKind, WatchReason, encode_pane_retire};
use marspot_peer::control::{ErrorCode, HelperStartArgs, HelperStop, Op, OpenArgs, Reject};
use marspot_peer::json::Value;

use super::super::observe;
use super::caller::{self, CallerKey, Peer};
use super::conn::Conn;
use super::dispatch::{Ctx, audit, target};
use super::{hub, names, ops_input, ops_pane};

pub(crate) trait HelperService: Send + Sync {
    fn start(&self, port: &mut Port, a: &HelperStartArgs) -> Result<Value, Reject>;
    /// Answered on the connection's thread from what was published.
    fn list(&self, caller: Option<u64>, all: bool) -> Value;
    fn stop(&self, port: &mut Port, which: &HelperStop) -> Result<Value, Reject>;
    /// Stop `sid` as the user asked in the window; None when it is not
    /// a helper.
    fn stop_sid(&self, _port: &mut Port, _sid: u64) -> Option<Result<(), Reject>> {
        None
    }
    /// Input reached pane `sid` through the socket.
    fn input_seen(&self, sid: u64);
}

static SERVICE: OnceLock<Box<dyn HelperService>> = OnceLock::new();

pub(crate) fn register(s: Box<dyn HelperService>) {
    let _ = SERVICE.set(s);
}

/// [`HelperService::stop_sid`] as `ctx` asks it.
pub(super) fn stop_sid(conn: &mut Conn, ctx: &Ctx, sid: u64) -> Option<Result<(), Reject>> {
    SERVICE.get()?.stop_sid(&mut Port { conn, ctx }, sid)
}

pub(super) fn serve(conn: &mut Conn, ctx: &Ctx, op: &Op) -> Result<Value, Reject> {
    let Some(s) = SERVICE.get() else {
        return Err(Reject::new(ErrorCode::NotSupported, format!("{} is not served by this build", op.name())));
    };
    let mut port = Port { conn, ctx };
    match op {
        Op::HelperStart(a) => s.start(&mut port, a),
        Op::HelperList { all } => Ok(s.list(ctx.caller.sid(), *all)),
        Op::HelperStop(w) => s.stop(&mut port, w),
        _ => Err(Reject::new(ErrorCode::Internal, "not a helper request")),
    }
}

pub(super) fn input_seen(sid: u64) {
    if let Some(s) = SERVICE.get() {
        s.input_seen(sid);
    }
}

/// What became of a wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Waited {
    Met { pos: u64 },
    Ended,
    Timeout,
}

pub(crate) struct Port<'a, 'h> {
    conn: &'a mut Conn<'h>,
    ctx: &'a Ctx,
}

/// Run `f` with a port no connection opened, counted as asked by pane
/// `caller` (or from outside the panes).  None before the socket runs.
pub(crate) fn with_port<R>(caller: Option<u64>, f: impl FnOnce(&mut Port) -> R) -> Option<R> {
    let h = hub()?;
    let mut conn = Conn::new(h, Peer::default(), None);
    let ctx = Ctx {
        caller: caller.map_or(CallerKey::External, CallerKey::Pane),
        caller_name: "marspot".into(),
        obs: observe::shared().fresh(observe::INTERVAL),
        cfg: h.config(),
        view: |_| observe::shared().fresh(observe::INTERVAL),
    };
    Some(f(&mut Port { conn: &mut conn, ctx: &ctx }))
}

impl Port<'_, '_> {
    /// The pane the caller is in.
    pub fn caller(&self) -> Option<u64> {
        self.ctx.caller.sid()
    }

    /// The processes from the one that connected up to its pane's
    /// shell; None when the caller was named by its codex thread, whose
    /// requests come from a process outside the panes.
    pub fn caller_processes(&self) -> Option<Vec<ProcRow>> {
        let (pid, sid) = (self.conn.peer.pid?, self.conn.peer.pane?);
        let obs = self.ctx.view_now();
        let shell = obs.session(sid)?.shell?;
        (self.caller() == Some(sid)).then(|| caller::chain(pid, |p| caller::row_of(p, &obs), shell.pid))
    }

    pub fn target(&self, t: &str) -> Result<u64, Reject> {
        target(self.ctx, t)
    }

    pub fn cwd_of(&self, sid: u64) -> Option<String> {
        names::cwd_of(sid, &self.ctx.view_now())
    }

    pub fn full_name(&self, sid: u64) -> String {
        names::full_name(sid, &self.ctx.view_now())
    }

    /// A shadow pane under `parent`, in `cwd`, within every limit a
    /// `pane.open` from the caller is held to: its id and the full name
    /// `pane.list` shows for it.
    pub fn open_shadow(&mut self, parent: u64, cwd: &str) -> Result<(u64, String), Reject> {
        let a = OpenArgs { cwd: Some(cwd.to_string()), shadow: true, parent: Some(parent.to_string()), cols: 120, rows: 40, command: None };
        let v = ops_pane::open(self.conn, self.ctx, &a)?;
        let sid = v.get("id").and_then(Value::i64).map(|i| i as u64).ok_or_else(|| Reject::new(ErrorCode::Internal, "the new pane has no id"))?;
        let name = v.str_at("name").map_or_else(|| sid.to_string(), str::to_string);
        Ok((sid, name))
    }

    /// Type `line` at the shell of a pane that just opened, once its
    /// prompt is drawn (at most `within`); the position it went in at.
    pub fn type_at_prompt(&mut self, sid: u64, line: &str, within: Duration) -> Result<u64, Reject> {
        let body = WatchBody { watch_id: 0, kind: WatchKind::Prompt, since_pos: 0, quiet_ms: 0, text: String::new() };
        // a prompt never seen still gets the line, refused unless the shell is in front
        let _ = self.conn.wait_as_helper(sid, body, Some(0), Instant::now() + within);
        let t = PaneType { req_id: 0, sid, paste: true, enter: true, only_at_shell: true, bytes: line.as_bytes().to_vec(), watch: None };
        ops_input::type_into(self.conn, sid, t).map(|t| t.pos)
    }

    /// Paste `text`, then Enter when `enter`; the position after it.
    pub fn send_text(&mut self, sid: u64, text: &str, enter: bool) -> Result<u64, Reject> {
        let t = PaneType { req_id: 0, sid, paste: true, enter, only_at_shell: false, bytes: text.as_bytes().to_vec(), watch: None };
        ops_input::type_into(self.conn, sid, t).map(|t| t.pos)
    }

    pub fn send_keys(&mut self, sid: u64, bytes: &[u8]) -> Result<u64, Reject> {
        let t = PaneType { req_id: 0, sid, paste: false, enter: false, only_at_shell: false, bytes: bytes.to_vec(), watch: None };
        ops_input::type_into(self.conn, sid, t).map(|t| t.pos)
    }

    /// Wait until `sid` printed past `since` (from now when None) and
    /// was then quiet `quiet_ms`, or `deadline`.
    pub fn wait_quiet(&mut self, sid: u64, quiet_ms: u32, since: Option<u64>, deadline: Instant) -> Result<Waited, Reject> {
        let body = WatchBody { watch_id: 0, kind: WatchKind::Quiet, since_pos: 0, quiet_ms, text: String::new() };
        Ok(match self.conn.wait_as_helper(sid, body, since, deadline)? {
            None => Waited::Timeout,
            Some(f) if f.reason == WatchReason::Met => Waited::Met { pos: f.pos },
            Some(f) if f.reason == WatchReason::Restarting => Waited::Timeout,
            Some(_) => Waited::Ended,
        })
    }

    /// Close shadow pane `sid` for good: its directory goes too.
    pub fn close(&mut self, sid: u64) -> Result<(), Reject> {
        self.conn.need_caps(CAP_SHADOWS)?;
        let build = |req_id| Ok(encode_pane_retire(&PaneRetire { req_id, sid, mode: RetireMode::Delete }));
        let out = ops_pane::ask_done(self.conn, sid, MsgType::PaneRetire, build, ops_pane::Again::Gone);
        if out.is_ok() {
            self.conn.hub.recent.lock().unwrap_or_else(|p| p.into_inner()).closed(sid, Instant::now());
        }
        audit(self.conn, self.ctx, "pane.close", sid, None, false, out.as_ref().map_or_else(|r| r.code.as_str(), |_| "closed"));
        out
    }

    /// Write down what a helper request did to pane `sid`.
    pub fn audit(&self, op: &'static str, sid: u64, detail: &str, result: &str) {
        audit(self.conn, self.ctx, op, sid, Some(detail.as_bytes().to_vec()), false, result);
    }
}

//! One request, start to answer: read it, see who asks, apply the
//! switches and the rates, run the op, write down what was refused.

use std::sync::Arc;

use marspot_peer::control::{ErrorCode, Op, Reject, Response, request};
use marspot_peer::json::Value;

use super::super::observe::{self, Observation};
use super::audit::Rec;
use super::caller::{self, CallerKey};
use super::config::Config;
use super::conn::Conn;
use super::limits::Kind;
use super::{names, ops_input, ops_pane, ops_read};

/// What every op is handed besides its arguments.
pub(super) struct Ctx {
    pub caller: CallerKey,
    /// The caller's full pane name, or `external`.
    pub caller_name: String,
    pub obs: Arc<Observation>,
    pub cfg: Arc<Config>,
    /// Where a view taken now comes from.
    pub view: fn(&Ctx) -> Arc<Observation>,
}

impl Ctx {
    /// A view at most the observer's interval old, for a request that
    /// waited after `obs` was taken.
    pub fn view_now(&self) -> Arc<Observation> {
        (self.view)(self)
    }
}

/// How recently a key in the focused pane counts as the user typing.
const TYPING_MS: u64 = 1_000;

pub(super) fn handle(conn: &mut Conn, line: &str) -> Response {
    let req = match request::parse(line) {
        Ok(r) => r,
        Err(r) => return refuse(conn, "external", "bad_request", "?", "", r),
    };
    // one walk of the process table per interval, however many requests
    let obs = observe::shared().fresh(observe::INTERVAL);
    let caller = caller::identify(&conn.peer, req.caller_thread.as_deref(), caller::codex_threads);
    let caller_name = match caller {
        CallerKey::Pane(sid) => names::full_name(sid, &obs),
        CallerKey::External => "external".into(),
    };
    // walks are shared per interval, so a later view costs at most one more
    let ctx = Ctx { caller, caller_name, obs, cfg: conn.hub.config(), view: |_| observe::shared().fresh(observe::INTERVAL) };
    let op_name: &'static str = static_name(&req.op);
    match run(conn, &ctx, &req.op) {
        Ok(v) => Response::Ok(v),
        Err(r) => {
            let pane = target_of(&req.op).unwrap_or("");
            refuse(conn, &ctx.caller_name, r.code.as_str(), op_name, pane, r)
        }
    }
}

fn refuse(conn: &Conn, caller: &str, code: &'static str, op: &'static str, pane: &str, r: Reject) -> Response {
    conn.hub.audit.refused(caller.to_string(), code, op, pane.to_string());
    Response::Err(r)
}

pub(super) fn run(conn: &mut Conn, ctx: &Ctx, op: &Op) -> Result<Value, Reject> {
    if !ctx.cfg.enabled {
        return Err(Reject::new(ErrorCode::Disabled, "pane control is switched off (control_enabled in settings.toml)"));
    }
    // the claimed thread's pane and the one the process descends from:
    // naming another pane's thread must not get a denied pane through
    let panes = [ctx.caller.sid(), conn.peer.pane];
    for sid in panes.iter().enumerate().filter_map(|(i, p)| p.filter(|s| !panes[..i].contains(&Some(*s)))) {
        if let Some(cwd) = names::cwd_of(sid, &ctx.obs)
            && conn.hub.deny.lock().unwrap_or_else(|p| p.into_inner()).denies(&cwd)
        {
            return Err(Reject::new(ErrorCode::Disabled, format!("panes in {cwd} may not control panes (control-deny.tsv)")));
        }
    }
    let kind = match op {
        Op::Send(_) | Op::Run(_) => Kind::Input,
        Op::Read(_) | Op::List | Op::Audit { .. } | Op::HelperList { .. } => Kind::Read,
        Op::Open(_) => Kind::Spawn,
        _ => Kind::Other,
    };
    let now = conn.hub.now_ms();
    conn.hub.limits.lock().unwrap_or_else(|p| p.into_inner()).admit(&ctx.cfg, ctx.caller, kind, now)?;
    match op {
        Op::List => Ok(ops_read::list(conn, ctx)),
        Op::Read(a) => ops_read::read(conn, ctx, a),
        Op::Audit { limit } => Ok(ops_read::audit(conn, *limit)),
        Op::Send(a) => ops_input::send(conn, ctx, a),
        Op::Run(a) => ops_input::run(conn, ctx, a),
        Op::Open(a) => ops_pane::open(conn, ctx, a),
        Op::Close { target } => ops_pane::close(conn, ctx, target),
        Op::Resize { target, cols, rows } => ops_pane::resize(conn, ctx, target, *cols, *rows),
        Op::Place { target, place } => ops_pane::place(conn, ctx, target, *place),
        Op::HelperStart(_) | Op::HelperList { .. } | Op::HelperStop(_) => super::helpers_port::serve(conn, ctx, op),
    }
}

fn static_name(op: &Op) -> &'static str {
    match op {
        Op::List => "pane.list",
        Op::Open(_) => "pane.open",
        Op::Send(_) => "pane.send",
        Op::Run(_) => "pane.run",
        Op::Read(_) => "pane.read",
        Op::Resize { .. } => "pane.resize",
        Op::Place { .. } => "pane.place",
        Op::Close { .. } => "pane.close",
        Op::Audit { .. } => "pane.audit",
        Op::HelperStart(_) => "helper.start",
        Op::HelperList { .. } => "helper.list",
        Op::HelperStop(_) => "helper.stop",
    }
}

fn target_of(op: &Op) -> Option<&str> {
    match op {
        Op::Send(a) => Some(&a.target),
        Op::Run(a) => Some(&a.target),
        Op::Read(a) => Some(&a.target),
        Op::Resize { target, .. } | Op::Place { target, .. } | Op::Close { target } => Some(target),
        _ => None,
    }
}

/// The session `target` names, alive now.  `self` is the caller's pane.
/// A session id the observation is too old to list still counts when
/// the registry has it and its process runs: a pane opened a moment ago.
pub(super) fn target(ctx: &Ctx, t: &str) -> Result<u64, Reject> {
    let sid = if t.trim() == "self" {
        ctx.caller.sid().ok_or_else(|| Reject::new(ErrorCode::NoSuchPane, "`self`: the caller is not in a pane"))?
    } else {
        match names::resolve_coded(t, &ctx.obs) {
            Ok(sid) => sid,
            Err(r) => match names::parse_target(t) {
                Ok(names::Target::Id(sid)) if r.code == ErrorCode::NoSuchPane && alive(sid) => sid,
                _ => return Err(r),
            },
        }
    };
    if !alive(sid) {
        return Err(Reject::new(ErrorCode::NoSuchPane, format!("pane {sid} has ended")));
    }
    Ok(sid)
}

pub(super) fn alive(sid: u64) -> bool {
    marspot_term::session_registry::read_session_entry(sid).is_ok_and(|e| e.pid > 1 && marspot::pidtree::pid_is_alive(e.pid))
}

/// The user typed into `sid` in the last second while it had the focus.
pub(super) fn user_typing(conn: &Conn, sid: u64) -> bool {
    use std::sync::atomic::Ordering;
    let h = conn.hub;
    let last = h.last_key_ms.load(Ordering::Relaxed);
    h.focus.load(Ordering::Relaxed) == sid && last > 0 && h.now_ms().saturating_sub(last) < TYPING_MS
}

pub(super) fn typing_reject() -> Reject {
    Reject::new(ErrorCode::UserTyping, "the user is typing in that pane; pass force to send anyway")
}

/// Write down a request that changed something.
pub(super) fn audit(conn: &Conn, ctx: &Ctx, op: &'static str, sid: u64, content: Option<Vec<u8>>, echo_off: bool, result: &str) {
    let reached = matches!(result, "written" | "ended" | "timeout") || result.starts_with("exit ");
    if matches!(op, "pane.send" | "pane.run") && reached {
        super::helpers_port::input_seen(sid);
    }
    conn.hub.audit.record(Rec {
        at_ms: super::audit_log::now_ms(),
        caller: ctx.caller_name.clone(),
        pid: conn.peer.pid,
        exe: conn.peer.exe.clone(),
        op,
        pane: names::full_name(sid, &ctx.obs),
        sid,
        content,
        echo_off,
        result: result.to_string(),
    });
}

//! `proc.stop`: a session ends processes of its own pane.  The judging
//! and the signals are the zombie watch's (`plugins::zombie::stop`);
//! here are the caller, the answer and one audit line per process named.

use std::sync::Arc;
use std::time::Instant;

use marspot::pidtree::ProcId;
use marspot_peer::control::{ErrorCode, ProcStopArgs, Reject};
use marspot_peer::json::{Value, obj};

use super::super::observe::{self, Observation};
use super::audit_log::{Audit, now_ms, utc_text};
use super::conn::Conn;
use super::dispatch::Ctx;
use crate::plugins::zombie::attrib::Known;
use crate::plugins::zombie::stop::{self, Outcome, Verdict, force};

pub(super) fn stop(conn: &mut Conn, ctx: &Ctx, a: &ProcStopArgs) -> Result<Value, Reject> {
    // within 100 ms, so a process the caller just started is in it
    let obs = observe::shared().fresh(observe::NOW);
    serve(conn, ctx, &obs, &Known::current(), a)
}

/// `stop` against a given observation and set of bindings.
pub(super) fn serve(conn: &Conn, ctx: &Ctx, obs: &Arc<Observation>, known: &Known, a: &ProcStopArgs) -> Result<Value, Reject> {
    let Some(sid) = ctx.caller.sid() else {
        return Err(Reject::new(ErrorCode::NoSuchPane, "proc.stop: the caller is not in a pane, and only a pane's own processes can be ended"));
    };
    let done = stop::run(obs, known, sid, conn.peer.pid, &a.ids, a.tree);
    let who = Who { caller: ctx.caller_name.clone(), pid: conn.peer.pid, exe: conn.peer.exe.clone(), sid };
    for v in &done.verdicts {
        let signal = match (v.outcome, a.tree) {
            (Outcome::Stopped, true) => "STOP,TERM,CONT",
            (Outcome::Stopped, false) => "TERM",
            _ => "-",
        };
        conn.hub.audit.line(line(&who, v, signal, v.outcome.as_str(), done.told.contains(&v.asked)));
    }
    let to_kill: Vec<Verdict> = done.verdicts.iter().filter(|v| v.outcome == Outcome::Stopped).cloned().collect();
    let wanted = a.force && !to_kill.is_empty();
    let mut forced = false;
    if wanted {
        let ids: Vec<ProcId> = to_kill.iter().flat_map(|v| v.signalled.iter().copied()).collect();
        let audit = conn.hub.audit.clone();
        let told = done.told.clone();
        forced = force::after(Instant::now() + force::GRACE, ids, Box::new(move |killed| after_kill(&audit, &who, &to_kill, &told, killed.as_ref())));
    }
    let results = done.verdicts.iter().map(result).collect();
    let mut kv = vec![("results".to_string(), Value::Arr(results)), ("force".into(), forced.into())];
    if wanted && !forced {
        kv.push(("note".into(), "too many kills are waiting: SIGKILL was not scheduled, ask again with force later".into()));
    }
    Ok(Value::Obj(kv))
}

/// Who asked, as each audit line names them.
#[derive(Clone)]
struct Who {
    caller: String,
    pid: Option<i32>,
    exe: String,
    sid: u64,
}

fn result(v: &Verdict) -> Value {
    let mut kv = vec![("id".to_string(), Value::from(v.asked.as_str())), ("result".into(), v.outcome.as_str().into())];
    if let Some(r) = &v.row {
        kv.push(("comm".into(), r.comm.as_str().into()));
    }
    if !v.message.is_empty() {
        kv.push(("message".into(), v.message.as_str().into()));
    }
    if !v.signalled.is_empty() {
        let ids = v.signalled.iter().map(|id| Value::from(format!("{}@{}", id.pid, id.start_us))).collect();
        kv.push(("signalled".into(), Value::Arr(ids)));
    }
    Value::Obj(kv)
}

/// One audit line: who asked, the process, what was sent, what came of it.
fn line(who: &Who, v: &Verdict, signal: &str, result: &str, suspect: bool) -> String {
    let (pid, start_us, comm) = match &v.row {
        Some(r) => (Value::from(r.pid as i64), Value::from(r.start_us.to_string()), Value::from(r.comm.as_str())),
        None => (Value::Null, Value::Null, Value::Null),
    };
    obj([
        ("at", utc_text(now_ms()).into()),
        ("caller", who.caller.as_str().into()),
        ("pid", who.pid.map_or(Value::Null, |p| (p as i64).into())),
        ("exe", who.exe.as_str().into()),
        ("op", "proc.stop".into()),
        ("sid", (who.sid as i64).into()),
        ("id", v.asked.as_str().into()),
        ("target_pid", pid),
        ("start_us", start_us),
        ("comm", comm),
        ("argv", v.argv.as_str().into()),
        ("tree", (v.signalled.len() as i64).into()),
        ("signal", signal.into()),
        ("result", result.into()),
        ("suspect", suspect.into()),
    ])
    .to_json()
}

/// The line for each named process once its SIGKILL came due: `killed`
/// when it or anything under it still ran, else `ended`; `lost` when
/// the shell replaced itself first and the kill was never sent.
fn after_kill(audit: &Audit, who: &Who, named: &[Verdict], told: &std::collections::HashSet<String>, killed: Option<&force::Killed>) {
    for v in named {
        let result = match killed {
            None => "lost",
            Some(k) if v.signalled.iter().any(|id| k.iter().any(|(kid, r)| kid == id && r.is_ok())) => "killed",
            Some(_) => "ended",
        };
        audit.line(line(who, v, "KILL", result, told.contains(&v.asked)));
    }
    if killed.is_none() {
        // the exec that drops the kill follows at once
        audit.flush();
    }
}

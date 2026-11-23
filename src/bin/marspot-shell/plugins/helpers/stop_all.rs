//! `helper.stop`: one helper on the request's own connection, or all of
//! a caller's at once, each on a thread of its own, answered within the
//! client's wait with what was stopped, what failed and why, and what
//! is still closing.

use std::sync::Arc;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use marspot_peer::control::{ErrorCode, Reject};
use marspot_peer::json::{Value, obj};

use super::Shared;
use super::table::Event;
use crate::pane_control::helpers_port::with_port;

/// Well inside the 30 s a client waits for an answer.
const ANSWER_WITHIN: Duration = Duration::from_secs(20);

/// Close helper `sid` with `close`; a failure with its session gone
/// counts as closed.  A helper still alive, or one that cannot be told
/// because the app was put away meanwhile, stays one.
pub(super) fn stop_one(s: &Shared, sid: u64, close: impl FnOnce() -> Result<(), Reject>) -> Result<(), Reject> {
    let epoch = s.epoch();
    if let Err(r) = close()
        && s.may_be_alive(sid, epoch)
    {
        return Err(r);
    }
    s.record(sid, Event::Stopped);
    s.sweep_dirs();
    Ok(())
}

/// Stop every helper in `named` (session id, full name) as asked by
/// pane `caller`.
pub(super) fn stop_all(s: &Arc<Shared>, caller: u64, named: Vec<(u64, String)>) -> Value {
    let (tx, rx) = channel();
    let mut waiting: Vec<(u64, String)> = Vec::new();
    for (sid, name) in named {
        let (s2, tx2) = (Arc::clone(s), tx.clone());
        let spawned = std::thread::Builder::new().name("helpers-stop".into()).spawn(move || {
            let out = stop_one(&s2, sid, || with_port(Some(caller), |p| p.close(sid)).unwrap_or_else(|| Err(Reject::new(ErrorCode::Internal, "the control socket is not running"))));
            let _ = tx2.send((sid, out));
        });
        match spawned {
            Ok(_) => waiting.push((sid, name)),
            Err(e) => {
                let _ = tx.send((sid, Err(Reject::new(ErrorCode::Internal, e.to_string()))));
                waiting.push((sid, name));
            }
        }
    }
    drop(tx);
    let until = Instant::now() + ANSWER_WITHIN;
    let (mut stopped, mut failed) = (Vec::new(), Vec::new());
    while !waiting.is_empty() {
        let Ok((sid, out)) = rx.recv_timeout(until.saturating_duration_since(Instant::now())) else { break };
        let Some(i) = waiting.iter().position(|(w, _)| *w == sid) else { continue };
        let (_, name) = waiting.swap_remove(i);
        match out {
            Ok(()) => stopped.push(Value::from(name)),
            Err(r) => failed.push(obj([("name", name.into()), ("code", r.code.as_str().into()), ("message", r.message.into())])),
        }
    }
    let closing: Vec<Value> = waiting.into_iter().map(|(_, n)| Value::from(n)).collect();
    obj([("stopped", Value::Arr(stopped)), ("failed", Value::Arr(failed)), ("closing", Value::Arr(closing))])
}

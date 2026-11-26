//! A request back to its JSON line, for the clients.

use super::{Input, Op, Request, VERSION, Wait};
use crate::control::b64;
use crate::json::{Value, obj};

impl Wait {
    pub(super) fn to_value(&self) -> Value {
        match self {
            Wait::Text(t) => obj([("text", t.as_str().into())]),
            Wait::Quiet(ms) => obj([("quiet_ms", (*ms as i64).into())]),
            Wait::CommandEnd => obj([("command_end", true.into())]),
            Wait::Prompt => obj([("prompt", true.into())]),
            Wait::Exit => obj([("exit", true.into())]),
            Wait::Change => obj([("change", true.into())]),
        }
    }
}

fn put(kv: &mut Vec<(String, Value)>, k: &str, v: impl Into<Value>) {
    kv.push((k.to_string(), v.into()));
}

fn put_opt(kv: &mut Vec<(String, Value)>, k: &str, v: &Option<String>) {
    if let Some(v) = v {
        put(kv, k, v.as_str());
    }
}

impl Request {
    /// The request as one line, without the newline.
    pub fn to_json(&self) -> String {
        let mut a: Vec<(String, Value)> = Vec::new();
        match &self.op {
            Op::List => {}
            Op::HelperStart(h) => a = super::helper::start_json(h),
            Op::HelperList { all } => put(&mut a, "all", *all),
            Op::HelperStop(h) => a = super::helper::stop_json(h),
            Op::ProcStop(p) => a = super::proc::stop_json(p),
            Op::ResetClaim(c) => a = super::reset::claim_json(c),
            Op::ResetStatus => {}
            Op::Open(o) => {
                put_opt(&mut a, "cwd", &o.cwd);
                put(&mut a, "shadow", o.shadow);
                if o.new_window {
                    put(&mut a, "new_window", true);
                }
                put_opt(&mut a, "parent", &o.parent);
                put(&mut a, "cols", o.cols as i64);
                put(&mut a, "rows", o.rows as i64);
                put_opt(&mut a, "command", &o.command);
            }
            Op::Send(s) => {
                put(&mut a, "target", s.target.as_str());
                match &s.input {
                    Input::Text { text, enter } => {
                        put(&mut a, "text", text.as_str());
                        put(&mut a, "enter", *enter);
                    }
                    Input::Keys(k) => put(&mut a, "keys", Value::Arr(k.iter().map(|n| n.as_str().into()).collect())),
                    Input::Bytes(b) => put(&mut a, "bytes_b64", b64::encode(b)),
                }
                put(&mut a, "force", s.force);
            }
            Op::Run(r) => {
                put(&mut a, "target", r.target.as_str());
                put(&mut a, "command", r.command.as_str());
                put(&mut a, "timeout_s", r.timeout_s as i64);
                put(&mut a, "force", r.force);
            }
            Op::Read(r) => {
                put(&mut a, "target", r.target.as_str());
                put(&mut a, "history_lines", r.history_lines as i64);
                if let Some(w) = &r.wait {
                    put(&mut a, "wait", w.to_value());
                }
                if let Some(s) = r.since {
                    put(&mut a, "since", s as i64);
                }
                put(&mut a, "timeout_s", r.timeout_s as i64);
            }
            Op::Resize { target, cols, rows } => {
                put(&mut a, "target", target.as_str());
                put(&mut a, "cols", *cols as i64);
                put(&mut a, "rows", *rows as i64);
            }
            Op::Place { target, place, force } => {
                put(&mut a, "target", target.as_str());
                put(&mut a, "where", place.as_str());
                put(&mut a, "force", *force);
            }
            Op::Close { target } => put(&mut a, "target", target.as_str()),
            Op::Audit { limit } => put(&mut a, "limit", *limit as i64),
        }
        let mut top = vec![("v".to_string(), Value::from(VERSION)), ("op".to_string(), self.op.name().into())];
        if let Some(t) = &self.caller_thread {
            top.push(("caller".into(), obj([("thread", t.as_str().into())])));
        }
        top.push(("args".into(), Value::Obj(a)));
        Value::Obj(top).to_json()
    }
}

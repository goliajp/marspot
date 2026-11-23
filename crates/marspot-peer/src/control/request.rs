//! Requests: what each op takes, read from and written to a JSON line.

use super::{ErrorCode, Reject, keys};
use crate::json::{self, Value};

/// The newest request version this build knows; a later one is read
/// for the parts this build understands.
pub const VERSION: i64 = 1;

pub const DEFAULT_TIMEOUT_S: u32 = 60;
pub const MAX_TIMEOUT_S: u32 = 3600;
pub const MAX_HISTORY_LINES: u32 = 2000;
pub const MAX_WAIT_TEXT: usize = 256;

#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub op: Op,
    /// The caller's Codex thread, when its client knows it.
    pub caller_thread: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    List,
    Open(OpenArgs),
    Send(SendArgs),
    Run(RunArgs),
    Read(ReadArgs),
    Resize { target: String, cols: u16, rows: u16 },
    Place { target: String, place: Where },
    Close { target: String },
    Audit { limit: u32 },
    /// `helper.*`: served by a handler this build may not have.
    Helper(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct OpenArgs {
    pub cwd: Option<String>,
    pub shadow: bool,
    pub parent: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub command: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    Text { text: String, enter: bool },
    Keys(Vec<String>),
    Bytes(Vec<u8>),
}

impl Input {
    /// The bytes this writes, before any paste brackets.
    pub fn len(&self) -> usize {
        match self {
            Input::Text { text, .. } => text.len(),
            Input::Keys(k) => k.iter().map(|n| keys::bytes(n).map_or(0, <[u8]>::len)).sum(),
            Input::Bytes(b) => b.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SendArgs {
    pub target: String,
    pub input: Input,
    pub force: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunArgs {
    pub target: String,
    pub command: String,
    pub timeout_s: u32,
    pub force: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Wait {
    Text(String),
    Quiet(u32),
    CommandEnd,
    Prompt,
    Exit,
    Change,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReadArgs {
    pub target: String,
    pub history_lines: u32,
    pub wait: Option<Wait>,
    pub since: Option<u64>,
    pub timeout_s: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Where {
    Window,
    Shadow,
    Peek,
    Unpeek,
}

impl Where {
    pub fn as_str(self) -> &'static str {
        match self {
            Where::Window => "window",
            Where::Shadow => "shadow",
            Where::Peek => "peek",
            Where::Unpeek => "unpeek",
        }
    }
}

impl Op {
    pub fn name(&self) -> &str {
        match self {
            Op::List => "pane.list",
            Op::Open(_) => "pane.open",
            Op::Send(_) => "pane.send",
            Op::Run(_) => "pane.run",
            Op::Read(_) => "pane.read",
            Op::Resize { .. } => "pane.resize",
            Op::Place { .. } => "pane.place",
            Op::Close { .. } => "pane.close",
            Op::Audit { .. } => "pane.audit",
            Op::Helper(n) => n,
        }
    }

    /// Asking again after a lost answer cannot do anything twice.
    pub fn is_read_only(&self) -> bool {
        matches!(self, Op::List | Op::Read(_) | Op::Audit { .. }) || self.name() == "helper.list"
    }
}

fn bad(msg: impl Into<String>) -> Reject {
    Reject::new(ErrorCode::BadRequest, msg)
}

struct Args<'a>(&'a Value);

impl Args<'_> {
    fn str(&self, k: &str) -> Result<Option<String>, Reject> {
        match self.0.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Str(s)) => Ok(Some(s.clone())),
            Some(_) => Err(bad(format!("`{k}` must be a string"))),
        }
    }
    fn target(&self) -> Result<String, Reject> {
        match self.0.get("target") {
            Some(Value::Str(s)) if !s.trim().is_empty() => Ok(s.clone()),
            Some(Value::Num(n)) if n.fract() == 0.0 && *n > 0.0 => Ok(format!("{}", *n as u64)),
            _ => Err(bad("`target` is required: a session id, a pane name, w(window,x,y) or self")),
        }
    }
    fn uint(&self, k: &str) -> Result<Option<u64>, Reject> {
        match self.0.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Num(n)) if n.fract() == 0.0 && *n >= 0.0 => Ok(Some(*n as u64)),
            Some(_) => Err(bad(format!("`{k}` must be a whole number"))),
        }
    }
    fn bool(&self, k: &str, default: bool) -> Result<bool, Reject> {
        match self.0.get(k) {
            None | Some(Value::Null) => Ok(default),
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(bad(format!("`{k}` must be true or false"))),
        }
    }
    fn timeout(&self) -> Result<u32, Reject> {
        Ok(self.uint("timeout_s")?.map_or(DEFAULT_TIMEOUT_S, |t| t.clamp(1, MAX_TIMEOUT_S as u64) as u32))
    }
    fn size(&self, k: &str, default: u16) -> Result<u16, Reject> {
        match self.uint(k)? {
            None => Ok(default),
            Some(n @ 2..=1000) => Ok(n as u16),
            Some(_) => Err(bad(format!("`{k}` must be between 2 and 1000"))),
        }
    }
}

/// Read one request line.
pub fn parse(line: &str) -> Result<Request, Reject> {
    let v = json::parse(line).map_err(|e| bad(format!("not JSON: {e}")))?;
    if !matches!(v, Value::Obj(_)) {
        return Err(bad("a request is a JSON object"));
    }
    let op = v.str_at("op").ok_or_else(|| bad("`op` is required"))?;
    let caller_thread = v.at(&["caller", "thread"]).and_then(Value::str).map(String::from);
    let empty = Value::Obj(Vec::new());
    let a = Args(v.get("args").filter(|a| matches!(a, Value::Obj(_))).unwrap_or(&empty));
    let op = match op {
        "pane.list" => Op::List,
        "pane.open" => Op::Open(OpenArgs {
            cwd: a.str("cwd")?,
            shadow: a.bool("shadow", true)?,
            parent: a.str("parent")?,
            cols: a.size("cols", 120)?,
            rows: a.size("rows", 40)?,
            command: a.str("command")?,
        }),
        "pane.send" => Op::Send(SendArgs { target: a.target()?, input: input(&a)?, force: a.bool("force", false)? }),
        "pane.run" => Op::Run(RunArgs {
            target: a.target()?,
            command: run_command(a.str("command")?)?,
            timeout_s: a.timeout()?,
            force: a.bool("force", false)?,
        }),
        "pane.read" => Op::Read(ReadArgs {
            target: a.target()?,
            history_lines: a.uint("history_lines")?.map_or(0, |n| n.min(MAX_HISTORY_LINES as u64) as u32),
            wait: wait(a.0.get("wait"))?,
            since: a.uint("since")?,
            timeout_s: a.timeout()?,
        }),
        "pane.resize" => Op::Resize {
            target: a.target()?,
            cols: a.uint("cols")?.ok_or_else(|| bad("`cols` is required"))?.min(1000) as u16,
            rows: a.uint("rows")?.ok_or_else(|| bad("`rows` is required"))?.min(1000) as u16,
        },
        "pane.place" => Op::Place {
            target: a.target()?,
            place: match a.str("where")?.as_deref() {
                Some("window") => Where::Window,
                Some("shadow") => Where::Shadow,
                Some("peek") => Where::Peek,
                Some("unpeek") => Where::Unpeek,
                _ => return Err(bad("`where` is one of window, shadow, peek, unpeek")),
            },
        },
        "pane.close" => Op::Close { target: a.target()? },
        "pane.audit" => Op::Audit { limit: a.uint("limit")?.map_or(50, |n| n.clamp(1, 1000) as u32) },
        h if h.starts_with("helper.") => Op::Helper(h.to_string()),
        other => return Err(bad(format!("unknown op `{other}`"))),
    };
    if let Op::Resize { cols, rows, .. } = &op
        && (*cols < 2 || *rows < 2)
    {
        return Err(bad("`cols` and `rows` must be at least 2"));
    }
    Ok(Request { op, caller_thread })
}

#[path = "request_args.rs"]
mod args;
use args::{input, run_command, wait};

#[path = "request_json.rs"]
mod encode;

#[cfg(test)]
#[path = "request_tests.rs"]
mod pane_control_tests;

//! JSON-RPC over the Codex app-server control socket.
//!
//! Methods used here are experimental in Codex 0.163 and are only
//! served to a client that declares `experimentalApi` at initialize.
//! The server also sends notifications (and requests, such as approval
//! prompts meant for the TUI) to every subscriber of a thread; this
//! client keeps them for the caller and never answers a request, so
//! the TUI stays the one that answers.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::path::Path;
use std::time::Duration;

use crate::json::{self, Value, obj};
use crate::ws::{Msg, Ws};

/// Notifications kept while a call waits for its answer.  Past this the
/// oldest are dropped and counted: a stalled reader must not grow
/// without bound.
const MAX_HELD: usize = 1024;

#[derive(Debug)]
pub enum RpcError {
    Io(io::Error),
    /// The server answered with an error object.
    Remote { code: i64, message: String },
    Closed,
    Protocol(String),
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RpcError::Io(e) => write!(f, "{e}"),
            RpcError::Remote { code, message } => write!(f, "{message} ({code})"),
            RpcError::Closed => f.write_str("connection closed"),
            RpcError::Protocol(m) => write!(f, "protocol: {m}"),
        }
    }
}

impl std::error::Error for RpcError {}

impl From<io::Error> for RpcError {
    fn from(e: io::Error) -> Self {
        RpcError::Io(e)
    }
}

pub struct Rpc {
    ws: Ws,
    next_id: i64,
    held: VecDeque<Value>,
    pub dropped: u64,
    /// `userAgent` from the initialize answer, e.g. `codex-tui/0.163.0 …`.
    pub user_agent: String,
}

impl Rpc {
    pub fn connect(path: &Path, client: &str, version: &str, timeout: Duration) -> Result<Rpc, RpcError> {
        let ws = Ws::connect(path, timeout)?;
        let mut rpc = Rpc { ws, next_id: 0, held: VecDeque::new(), dropped: 0, user_agent: String::new() };
        let info = obj([("name", Value::from(client)), ("version", Value::from(version))]);
        let caps = obj([("experimentalApi", Value::from(true))]);
        let r = rpc.call("initialize", obj([("clientInfo", info), ("capabilities", caps)]))?;
        rpc.user_agent = r.str_at("userAgent").unwrap_or_default().to_string();
        rpc.ws.send_text(&obj([("jsonrpc", Value::from("2.0")), ("method", Value::from("initialized"))]).to_json())?;
        Ok(rpc)
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.next_id += 1;
        let id = self.next_id;
        let req = obj([
            ("jsonrpc", Value::from("2.0")),
            ("id", Value::from(id)),
            ("method", Value::from(method)),
            ("params", params),
        ]);
        self.ws.send_text(&req.to_json())?;
        loop {
            let v = self.read()?;
            let is_answer = v.get("method").is_none() && v.get("id").and_then(Value::i64) == Some(id);
            if !is_answer {
                self.hold(v);
                continue;
            }
            if let Some(e) = v.get("error") {
                let code = e.get("code").and_then(Value::i64).unwrap_or(0);
                let message = e.str_at("message").unwrap_or_default().to_string();
                return Err(RpcError::Remote { code, message });
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// The next notification or server request: a held one first, else
    /// one read now.  `Ok(None)` when the read timeout passed with
    /// nothing to read.
    pub fn next_event(&mut self) -> Result<Option<Value>, RpcError> {
        if let Some(v) = self.held.pop_front() {
            return Ok(Some(v));
        }
        match self.read() {
            Ok(v) => Ok(Some(v)),
            Err(RpcError::Io(e)) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn set_read_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        self.ws.set_read_timeout(d)
    }

    fn read(&mut self) -> Result<Value, RpcError> {
        match self.ws.recv()? {
            Msg::Text(t) => json::parse(&t).map_err(RpcError::Protocol),
            Msg::Closed => Err(RpcError::Closed),
        }
    }

    fn hold(&mut self, v: Value) {
        if self.held.len() == MAX_HELD {
            self.held.pop_front();
            self.dropped += 1;
        }
        self.held.push_back(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against a real Codex daemon; needs one running for `CODEX_HOME`
    /// (default `~/.codex`).  Read-only calls.
    /// `cargo test -p marspot-peer -- --ignored live_daemon`
    #[test]
    #[ignore]
    fn live_daemon_initializes_and_lists_threads() {
        let home = std::env::var("CODEX_HOME").unwrap_or_else(|_| format!("{}/.codex", std::env::var("HOME").unwrap()));
        let sock = Path::new(&home).join("app-server-control/app-server-control.sock");
        let mut rpc = Rpc::connect(&sock, "marspot-peer-test", "0", Duration::from_secs(10)).unwrap();
        assert!(rpc.user_agent.starts_with("codex"), "{}", rpc.user_agent);
        let r = rpc.call("thread/loaded/list", obj([])).unwrap();
        assert!(matches!(r.get("data"), Some(Value::Arr(_))), "{}", r.to_json());
        let err = rpc.call("thread/read", obj([("threadId", Value::from("not-a-thread"))])).unwrap_err();
        assert!(matches!(err, RpcError::Remote { .. }), "{err}");
    }
}

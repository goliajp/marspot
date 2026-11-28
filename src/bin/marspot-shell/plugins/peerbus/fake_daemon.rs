//! A stand-in for a codex home's app-server daemon: a real socket at
//! `<home>/app-server-control/app-server-control.sock` speaking the
//! WebSocket JSON-RPC the worker speaks, answering from a table of
//! threads, and writing down every method it was asked.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};

use marspot_peer::json::{self, obj, Value};
use marspot_peer::sha1::{base64, sha1};

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

#[derive(Default)]
pub(in crate::plugins) struct State {
    /// `thread/read`'s `thread` for each loaded thread, in order.
    pub threads: Vec<(String, Value)>,
    /// Listed as loaded, but `thread/read` fails for them.
    pub unreadable: Vec<String>,
    /// Ids per page of `thread/loaded/list`; all on one when None.
    pub page: Option<usize>,
    /// Every request's method, in the order they came.
    pub asked: Vec<String>,
    /// Methods answered with an error.
    pub refused: Vec<&'static str>,
    /// How long each answer takes.
    pub delay: std::time::Duration,
}

pub(in crate::plugins) struct Daemon {
    pub state: Arc<Mutex<State>>,
}

impl Daemon {
    pub fn start(home: &Path) -> Daemon {
        let dir = home.join("app-server-control");
        std::fs::create_dir_all(&dir).unwrap();
        let listener = marspot_term::uds_path::bind(dir.join("app-server-control.sock")).unwrap();
        let state = Arc::new(Mutex::new(State::default()));
        let s = Arc::clone(&state);
        std::thread::spawn(move || {
            for c in listener.incoming().flatten() {
                let s = Arc::clone(&s);
                std::thread::spawn(move || serve(c, &s));
            }
        });
        Daemon { state }
    }

    pub fn asked(&self, method: &str) -> usize {
        self.state.lock().unwrap().asked.iter().filter(|m| *m == method).count()
    }
}

/// A thread as `thread/read` describes it.
pub(super) fn thread(cwd: &str, parent: Option<&str>, status: &str) -> Value {
    obj([
        ("cwd", Value::from(cwd)),
        ("ephemeral", Value::from(false)),
        ("parentThreadId", parent.map_or(Value::Null, Value::from)),
        ("threadSource", Value::from("user")),
        ("status", obj([("type", Value::from(status))])),
    ])
}

fn serve(mut c: UnixStream, state: &Mutex<State>) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if c.read_exact(&mut byte).is_err() {
            return;
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let Some(key) = head.lines().find_map(|l| l.strip_prefix("Sec-WebSocket-Key: ")) else { return };
    let accept = base64(&sha1(format!("{}{GUID}", key.trim()).as_bytes()));
    let _ = write!(c, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n");
    while let Some(text) = read_text(&mut c) {
        let Ok(req) = json::parse(&text) else { return };
        let (Some(id), Some(method)) = (req.get("id").cloned(), req.str_at("method").map(str::to_string)) else {
            continue; // a notification
        };
        let delay = state.lock().unwrap().delay;
        std::thread::sleep(delay);
        let answer = answer(state, &method, req.get("params"));
        let msg = match answer {
            Ok(result) => obj([("jsonrpc", Value::from("2.0")), ("id", id), ("result", result)]),
            Err(m) => obj([("jsonrpc", Value::from("2.0")), ("id", id), ("error", obj([("code", Value::from(-32600i64)), ("message", Value::from(m))]))]),
        };
        if write_text(&mut c, &msg.to_json()).is_err() {
            return;
        }
    }
}

fn answer(state: &Mutex<State>, method: &str, params: Option<&Value>) -> Result<Value, String> {
    let mut s = state.lock().unwrap();
    s.asked.push(method.to_string());
    if s.refused.contains(&method) {
        return Err(format!("{method}: refused here"));
    }
    let id = params.and_then(|p| p.str_at("threadId")).unwrap_or_default().to_string();
    match method {
        "initialize" => Ok(obj([("userAgent", Value::from("codex-tui/0.172.0 (fake)"))])),
        "thread/loaded/list" => {
            let ids: Vec<&str> = s.threads.iter().map(|(i, _)| i.as_str()).chain(s.unreadable.iter().map(String::as_str)).collect();
            let from: usize = params.and_then(|p| p.str_at("cursor")).and_then(|c| c.parse().ok()).unwrap_or(0);
            let to = s.page.map_or(ids.len(), |n| (from + n).min(ids.len()));
            let next = if to < ids.len() { Value::from(to.to_string().as_str()) } else { Value::Null };
            Ok(obj([("data", Value::Arr(ids[from..to].iter().map(|i| Value::from(*i)).collect())), ("nextCursor", next)]))
        }
        "thread/read" => s.threads.iter().find(|(i, _)| *i == id).map(|(_, t)| obj([("thread", t.clone())])).ok_or("not loaded".into()),
        "thread/resume" => Ok(obj([("approvalPolicy", Value::from("never")), ("sandbox", obj([("type", Value::from("dangerFullAccess"))]))])),
        "thread/unsubscribe" | "thread/queue/add" | "turn/steer" | "thread/goal/set" | "turn/interrupt" => Ok(obj([])),
        "thread/goal/get" => Ok(obj([("goal", Value::Null)])),
        "thread/turns/list" => Ok(obj([("data", Value::Arr(Vec::new()))])),
        _ => Err(format!("{method}: not served here")),
    }
}

/// One client text message; client frames are masked and short here.
fn read_text(c: &mut UnixStream) -> Option<String> {
    let mut h = [0u8; 2];
    c.read_exact(&mut h).ok()?;
    if h[0] & 0x0F == 0x8 {
        return None;
    }
    let len = match h[1] & 0x7F {
        126 => {
            let mut b = [0u8; 2];
            c.read_exact(&mut b).ok()?;
            u16::from_be_bytes(b) as usize
        }
        127 => {
            let mut b = [0u8; 8];
            c.read_exact(&mut b).ok()?;
            u64::from_be_bytes(b) as usize
        }
        n => n as usize,
    };
    let mut mask = [0u8; 4];
    c.read_exact(&mut mask).ok()?;
    let mut p = vec![0u8; len];
    c.read_exact(&mut p).ok()?;
    p.iter_mut().enumerate().for_each(|(i, x)| *x ^= mask[i % 4]);
    String::from_utf8(p).ok()
}

fn write_text(c: &mut UnixStream, text: &str) -> std::io::Result<()> {
    let b = text.as_bytes();
    let mut out = vec![0x81];
    if b.len() < 126 {
        out.push(b.len() as u8);
    } else if b.len() <= 0xFFFF {
        out.push(126);
        out.extend_from_slice(&(b.len() as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(b.len() as u64).to_be_bytes());
    }
    out.extend_from_slice(b);
    c.write_all(&out)
}

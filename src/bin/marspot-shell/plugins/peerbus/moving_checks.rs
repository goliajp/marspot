//! What the old home's daemon and queue say about a session that is to
//! move, and the one door into that daemon that a move closes.

use std::collections::HashSet;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use marspot_peer::json::{obj, Value};
use marspot_peer::rpc::RpcError;
use marspot_term::lx_warn;

use super::moving::Held;
use super::worker::Worker;

/// Pages of `thread/loaded/list` read before giving up on an answer.
const MAX_PAGES: usize = 64;
/// How long `sqlite3` may take altogether; its `.timeout` bounds only
/// the wait for a lock.
const SQLITE_FOR: Duration = Duration::from_secs(5);

/// One loaded thread, as `thread/read` gives it.
pub(super) struct Loaded {
    pub id: String,
    pub parent: Option<String>,
    pub status: Option<String>,
}

impl Worker {
    /// The daemon's word on `thread` and the threads it started, and
    /// the home's queue; None when it may move.
    pub(super) fn why_held(&mut self, home: &Path, thread: &str) -> Option<Held> {
        let loaded = match self.loaded_states(home) {
            Ok(l) => l,
            Err(e) => {
                lx_warn!("peerbus.move.unknown", &e.to_string(), thread = thread.to_string());
                return Some(Held::Unknown);
            }
        };
        match loaded.iter().find(|t| t.id == thread).map(|t| t.status.as_deref()) {
            Some(Some("idle")) => {}
            Some(Some("active")) => return Some(Held::Busy),
            _ => return Some(Held::Unknown),
        }
        if running_descendant(&loaded, thread) {
            return Some(Held::ChildRunning);
        }
        match queued(home, thread) {
            Ok(0) => None,
            Ok(_) => Some(Held::Queued),
            Err(e) => {
                lx_warn!("peerbus.move.queue_unread", &e, thread = thread.to_string());
                Some(Held::Unknown)
            }
        }
    }

    /// Every thread the daemon has loaded, across all pages.
    pub(super) fn loaded_ids(&mut self, home: &Path) -> Result<Vec<String>, RpcError> {
        let mut ids = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(c) => obj([("cursor", Value::from(c.as_str()))]),
                None => obj([]),
            };
            let r = self.call(home, "thread/loaded/list", params)?;
            ids.extend(r.get("data").map_or(&[][..], Value::arr).iter().filter_map(Value::str).map(str::to_string));
            cursor = r.str_at("nextCursor").map(str::to_string);
            if cursor.is_none() {
                return Ok(ids);
            }
        }
        Err(RpcError::Protocol(format!("thread/loaded/list: more than {MAX_PAGES} pages")))
    }

    /// Each loaded thread with its parent and status.  One that cannot
    /// be read fails the whole answer: it may be a child at work.
    fn loaded_states(&mut self, home: &Path) -> Result<Vec<Loaded>, RpcError> {
        let ids = self.loaded_ids(home)?;
        let mut out = Vec::new();
        for id in ids {
            let read = self.call(home, "thread/read", obj([("threadId", Value::from(id.as_str())), ("includeTurns", Value::from(false))]))?;
            let t = read.get("thread");
            let parent = t.and_then(|t| t.str_at("parentThreadId")).map(str::to_string);
            let status = t.and_then(|t| t.get("status")).and_then(|s| s.str_at("type")).map(str::to_string);
            out.push(Loaded { id, parent, status });
        }
        Ok(out)
    }

    /// Put `text` into the thread: steering the running turn when asked
    /// to and able, queued as the next input otherwise.  Never into a
    /// session that is moving: its daemon is the one being left.
    pub(super) fn hand_over(&mut self, home: &Path, thread: &str, text: String, steer_turn: Option<String>, id: String) -> (Result<Value, RpcError>, &'static str) {
        if self.find(home, thread).is_some_and(|sid| self.moving(sid)) {
            return (Err(RpcError::Protocol("the session is moving to another account".into())), "moving");
        }
        let input = Value::Arr(vec![obj([("type", Value::from("text")), ("text", Value::from(text))])]);
        if let Some(turn) = steer_turn {
            let p = obj([("threadId", Value::from(thread)), ("expectedTurnId", Value::from(turn)), ("input", input.clone())]);
            let r = self.call(home, "turn/steer", p);
            if r.is_ok() {
                return (r, "turn/steer");
            }
        }
        let p = obj([("threadId", Value::from(thread)), ("clientUserMessageId", Value::from(id)), ("input", input)]);
        (self.call(home, "thread/queue/add", p), "thread/queue/add")
    }
}

/// A thread started by `root`, or by one of those, still at work.  A
/// thread with no status is counted as working: not knowing is no
/// reason to move.
pub(super) fn running_descendant(loaded: &[Loaded], root: &str) -> bool {
    let mut family: HashSet<&str> = HashSet::from([root]);
    // each pass adds a generation; there are no more than threads
    for _ in 0..loaded.len() {
        let before = family.len();
        for t in loaded {
            if t.parent.as_deref().is_some_and(|p| family.contains(p)) {
                family.insert(&t.id);
            }
        }
        if family.len() == before {
            break;
        }
    }
    loaded
        .iter()
        .filter(|t| t.id != root && family.contains(t.id.as_str()))
        .any(|t| !matches!(t.status.as_deref(), Some("idle" | "notLoaded" | "systemError")))
}

/// Messages waiting in `home`'s queue for `thread`.
///
/// Read from the daemon's own store with the system's `sqlite3`, opened
/// read-only: the app-server has a `thread/queue/list`, but what it
/// answers has not been established, and a misread "empty" would move
/// a session away from mail that then runs in the home it left.
pub(super) fn queued(home: &Path, thread: &str) -> Result<u64, String> {
    let db = home.join("queue_1.sqlite");
    if !db.exists() {
        return Ok(0);
    }
    // it goes into the query as a literal
    if thread.is_empty() || !thread.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Err(format!("not a thread id: {thread:?}"));
    }
    let query = format!("select count(*) from queued_items where thread_id = '{thread}';");
    let out = sqlite3(&db, &query, SQLITE_FOR)?;
    if !out.status.success() {
        return Err(format!("sqlite3: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().map_err(|e| format!("sqlite3 answered {:?}: {e}", out.stdout))
}

/// `sqlite3 -readonly <db> <query>`, killed when it has not finished
/// within `limit`.
pub(super) fn sqlite3(db: &Path, query: &str, limit: Duration) -> Result<std::process::Output, String> {
    sqlite3_with("/usr/bin/sqlite3", &["-readonly", "-cmd", ".timeout 2000"], db, query, limit)
}

pub(super) fn sqlite3_with(bin: &str, opts: &[&str], db: &Path, query: &str, limit: Duration) -> Result<std::process::Output, String> {
    let child = Command::new(bin)
        .args(opts)
        .arg(db)
        .arg(query)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("sqlite3: {e}"))?;
    let pid = child.id() as i32;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(limit) {
        Ok(r) => r.map_err(|e| format!("sqlite3: {e}")),
        Err(_) => {
            // the waiting thread has not reaped it, so the pid is still ours
            unsafe { libc::kill(pid, libc::SIGKILL) };
            Err(format!("sqlite3 did not finish within {limit:?}"))
        }
    }
}

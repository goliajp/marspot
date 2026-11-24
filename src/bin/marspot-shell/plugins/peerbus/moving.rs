//! A codex pane moving to another account: whether its session may go
//! now, and the mark that keeps messages out of the old home meanwhile.
//!
//! The session lives in the old home's daemon, which keeps it loaded
//! after the TUI is gone.  A message queued there in that gap runs a
//! turn nobody sees, in a home the pane has left; so from the moment a
//! move is decided until the old binding is gone, nothing is handed to
//! that daemon and senders are told so.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::SyncSender;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use marspot::pidtree::{self, ProcId};
use marspot_peer::json::{obj, Value};
use marspot_peer::rpc::RpcError;
use marspot_term::{lx_info, lx_warn};

use super::worker::{Event, Worker};

/// What the codex plugin asks of the worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ask {
    /// May the pane's session move?  Marked moving when it may.
    Begin { sid: u64, codex: ProcId },
    /// The move did not happen; the old process may still be there.
    End { sid: u64, codex: ProcId },
}

/// Why a session may not move now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Held {
    NotBound,
    Moving,
    Busy,
    ChildRunning,
    Queued,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Go { thread: String, home: PathBuf },
    Held(Held),
}

/// Verdicts not yet taken; one per ask, and asks come from clicks.
const MAX_VERDICTS: usize = 64;
/// How long after a move the next binding of its pane is still the
/// moved session's, owed the sentence that it may go on.
const GO_ON_WITHIN: Duration = Duration::from_secs(120);

static ASKS: Mutex<Option<SyncSender<Event>>> = Mutex::new(None);
static VERDICTS: Mutex<Vec<(u64, ProcId, Verdict)>> = Mutex::new(Vec::new());

pub(crate) fn connect(tx: Option<SyncSender<Event>>) {
    *ASKS.lock().unwrap() = tx;
}

/// Hand the worker `a`; false when there is no worker to take it.
pub(crate) fn ask(a: Ask) -> bool {
    ASKS.lock().unwrap().as_ref().is_some_and(|tx| tx.try_send(Event::Move(a)).is_ok())
}

/// A stand-in for the worker, for tests on the asking side: takes
/// asks and answers none.
#[cfg(test)]
pub(crate) struct Listener(std::sync::mpsc::Receiver<Event>);

#[cfg(test)]
impl Listener {
    pub(crate) fn connect() -> Listener {
        let (tx, rx) = std::sync::mpsc::sync_channel(8);
        connect(Some(tx));
        verdicts();
        Listener(rx)
    }

    /// The next ask, if one came.
    pub(crate) fn next(&self) -> Option<Ask> {
        match self.0.try_recv() {
            Ok(Event::Move(a)) => Some(a),
            _ => None,
        }
    }
}

/// Every verdict reached since the last call.
pub(crate) fn verdicts() -> Vec<(u64, ProcId, Verdict)> {
    std::mem::take(&mut *VERDICTS.lock().unwrap())
}

pub(crate) fn post(sid: u64, codex: ProcId, v: Verdict) {
    let mut q = VERDICTS.lock().unwrap();
    if q.len() == MAX_VERDICTS {
        q.remove(0);
    }
    q.push((sid, codex, v));
}

/// The worker's side: which panes are moving, and which are owed the
/// go-on sentence once their new process binds.
#[derive(Default)]
pub(crate) struct Moves {
    marked: HashMap<u64, ProcId>,
    go_on: HashMap<u64, (ProcId, Instant)>,
}

/// One loaded thread, as `thread/read` gives it.
pub(super) struct Loaded {
    pub id: String,
    pub parent: Option<String>,
    pub status: Option<String>,
}

impl Worker {
    pub(super) fn on_move(&mut self, a: Ask) {
        match a {
            Ask::Begin { sid, codex } => {
                let v = self.begin_move(sid, codex);
                lx_info!("peerbus.move", &format!("{v:?}"), sid = sid);
                post(sid, codex, v);
            }
            Ask::End { sid, codex } => {
                self.moves.go_on.remove(&sid);
                // a process already gone leaves its binding to the next
                // refresh, and the mark has to outlast that binding
                if self.moves.marked.get(&sid) == Some(&codex) && pidtree::proc_id(codex.pid) == Some(codex) {
                    self.moves.marked.remove(&sid);
                    lx_info!("peerbus.move.ended", "the session stays where it was", sid = sid);
                }
            }
        }
    }

    fn begin_move(&mut self, sid: u64, codex: ProcId) -> Verdict {
        let Some(b) = self.bindings.get(&sid).filter(|b| b.codex == codex) else {
            return Verdict::Held(Held::NotBound);
        };
        if self.moves.marked.contains_key(&sid) {
            return Verdict::Held(Held::Moving);
        }
        let (home, thread, turn) = (b.home.clone(), b.thread.clone(), b.active_turn.is_some());
        // marked before the checks: nothing is queued between them and the move
        self.moves.marked.insert(sid, codex);
        let held = if turn { Some(Held::Busy) } else { self.why_held(&home, &thread) };
        if let Some(h) = held {
            self.moves.marked.remove(&sid);
            return Verdict::Held(h);
        }
        self.moves.go_on.insert(sid, (codex, Instant::now()));
        Verdict::Go { thread, home }
    }

    /// The daemon's word on `thread` and the threads it started, and
    /// the home's queue; None when it may move.
    fn why_held(&mut self, home: &Path, thread: &str) -> Option<Held> {
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

    fn loaded_states(&mut self, home: &Path) -> Result<Vec<Loaded>, RpcError> {
        let r = self.call(home, "thread/loaded/list", obj([]))?;
        let ids: Vec<String> = r.get("data").map_or(&[][..], Value::arr).iter().filter_map(Value::str).map(str::to_string).collect();
        let mut out = Vec::new();
        for id in ids {
            let read = self.call(home, "thread/read", obj([("threadId", Value::from(id.as_str())), ("includeTurns", Value::from(false))]));
            let t = match read {
                Ok(v) => v,
                Err(RpcError::Remote { .. }) => continue, // unloaded in between
                Err(e) => return Err(e),
            };
            let t = t.get("thread");
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

    /// Is `sid`'s session moving?  Nothing is handed to its daemon then.
    pub(super) fn moving(&self, sid: u64) -> bool {
        self.moves.marked.contains_key(&sid)
    }

    /// The binding the mark protected is gone.
    pub(super) fn moved_out(&mut self, sid: u64) {
        self.moves.marked.remove(&sid);
    }

    /// A new process in a pane that moved has bound: tell the session
    /// it may go on, the sentence the move took the place of.
    pub(super) fn moved_in(&mut self, sid: u64, codex: ProcId) {
        self.moves.go_on.retain(|_, (_, at)| at.elapsed() < GO_ON_WITHIN);
        if !self.moves.go_on.get(&sid).is_some_and(|(old, _)| *old != codex) {
            return;
        }
        self.moves.go_on.remove(&sid);
        let Some(b) = self.bindings.get(&sid) else { return };
        let (home, thread) = (b.home.clone(), b.thread.clone());
        let word = crate::plugins::handoff::resume_word();
        let (r, via) = self.hand_over(&home, &thread, word, None, marspot_peer::rand::uuid_v4());
        match r {
            Ok(_) => lx_info!("peerbus.move.go_on", "the moved session was told to go on", sid = sid, via = via),
            Err(e) => lx_warn!("peerbus.move.go_on_failed", &e.to_string(), sid = sid),
        }
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
    let out = Command::new("/usr/bin/sqlite3")
        .args(["-readonly", "-cmd", ".timeout 2000"])
        .arg(&db)
        .arg(format!("select count(*) from queued_items where thread_id = '{thread}';"))
        .output()
        .map_err(|e| format!("sqlite3: {e}"))?;
    if !out.status.success() {
        return Err(format!("sqlite3: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().map_err(|e| format!("sqlite3 answered {:?}: {e}", out.stdout))
}

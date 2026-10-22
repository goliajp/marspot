//! The peerbus worker: one thread that owns every inbox, every
//! registry record marspot writes, and one app-server connection per
//! `CODEX_HOME`.
//!
//! Event-driven: it wakes when the set of Codex panes changes, when a
//! message arrives, or when a daemon reports something.  Two timers:
//! a retry after a daemon could not be reached, and the names check
//! (`NAMES_EVERY`), since a pane is renamed by a `cd` or a move that
//! nothing here is told about.
//! Calls to a daemon block this thread for up to `CALL_TIMEOUT`; events
//! that arrive meanwhile are set aside and handled after.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use marspot::pidtree;
use marspot_peer::envelope::Mode;
use marspot_peer::frame::{IdleState, Priority, Receipt, UserFrame};
use marspot_peer::json::{obj, Value};
use marspot_peer::registry::{self, Record, Status};
use marspot_peer::rpc::{RpcError, RpcSender};
use marspot_term::{lx_info, lx_warn};

use super::bind::Unbound;
use super::deliver::{self, Rate};
use super::inbox::Inbox;

pub(super) const CHANNEL: usize = 256;
pub(super) const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// Events set aside during calls.  Bounded like everything else; past
/// this the oldest are dropped and logged.
pub(super) const MAX_STASH: usize = 4096;
pub(super) const RETRY_MIN: Duration = Duration::from_secs(2);
/// How often names are checked against the panes.  A file listing and
/// one small read per agent; a rename shows up this late at worst.
const NAMES_EVERY: Duration = Duration::from_secs(10);

pub(super) const RETRY_MAX: Duration = Duration::from_secs(60);
/// Streaming notifications this client has no use for.  Without the
/// opt-out every token a subscribed thread prints would cross into L1.
pub(super) const OPT_OUT: &[&str] = &[
    "item/agentMessage/delta",
    "item/plan/delta",
    "command/exec/outputDelta",
    "process/outputDelta",
    "item/commandExecution/outputDelta",
    "item/fileChange/outputDelta",
    "item/reasoning/summaryTextDelta",
    "item/reasoning/textDelta",
    "thread/realtime/item/transcript/delta",
    "thread/realtime/transcript/delta",
    "thread/realtime/outputAudio/delta",
    "thread/tokenUsage/updated",
    "account/rateLimits/updated",
];

/// A pane with a Codex process, as the main thread saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Want {
    pub sid: u64,
    pub codex: pidtree::ProcId,
    pub argv: Vec<String>,
    /// From the pane's window title -- see `bind::thread_in_title`.
    pub title_thread: Option<String>,
}

pub(crate) enum Event {
    Want(Vec<Want>),
    Inbound { key: u64, frame: UserFrame, peer_pid: Option<i32> },
    /// `notify_when_idle`: tell `from` once, when the session next goes idle.
    Subscribe { key: u64, from: String, msg_id: String },
    /// `peer_idle_notice` for a subscription this session placed.
    Notice { key: u64, from: String, state: String, finished_at: Option<i64> },
    Rpc { home: PathBuf, msg: Value },
    RpcClosed { home: PathBuf },
    Stop,
}

pub(super) struct Conn {
    pub tx: RpcSender,
    /// Codex version, from the daemon's user agent.
    pub version: String,
}

pub(super) struct Binding {
    /// Unique per inbox, so a frame from a closed inbox finds nothing.
    pub(super) key: u64,
    pub(super) codex: pidtree::ProcId,
    pub(super) home: PathBuf,
    pub(super) thread: String,
    pub(super) class: Mode,
    /// The TUI's argv: its overrides decide the class until the thread
    /// has run a turn and can be asked directly.
    pub(super) argv: Vec<String>,
    /// `thread/resume` succeeded: turn events arrive for this thread.
    pub(super) subscribed: bool,
    pub(super) active_turn: Option<String>,
    pub(super) inbox: Inbox,
    pub(super) record: Record,
    pub(super) rate: Rate,
    /// Idle subscriptions: (reply address, subscription msg_id).
    pub(super) subs: Vec<(String, String)>,
    pub(super) last_delivered: Option<Instant>,
}

pub(crate) struct Worker {
    pub(super) tx: SyncSender<Event>,
    pub(super) rx: Receiver<Event>,
    pub(super) stash: VecDeque<Event>,
    pub(super) owner: String,
    pub(super) sock_dir: Option<PathBuf>,
    pub(super) conns: HashMap<PathBuf, Conn>,
    pub(super) bindings: HashMap<u64, Binding>,
    pub(super) wants: Vec<Want>,
    /// `(home, thread) → cwd`, None for a thread that is not a user's
    /// (see `bind::user_thread_cwd`).  Neither ever changes.
    pub(super) cwds: HashMap<(PathBuf, String), Option<String>>,
    /// Last reason logged per unbound pane, so it is logged once.
    pub(super) unbound: HashMap<u64, Unbound>,
    pub(super) next_key: u64,
    pub(super) retry_at: Option<Instant>,
    pub(super) backoff: Duration,
    pub(super) names_at: Instant,
    /// Panes running two agents, logged once each.
    pub(super) crowded: HashSet<u64>,
}

impl Worker {
    pub fn new(tx: SyncSender<Event>, rx: Receiver<Event>, owner: String) -> Worker {
        Worker {
            tx,
            rx,
            stash: VecDeque::new(),
            owner,
            sock_dir: None,
            conns: HashMap::new(),
            bindings: HashMap::new(),
            wants: Vec::new(),
            cwds: HashMap::new(),
            unbound: HashMap::new(),
            next_key: 1,
            retry_at: None,
            backoff: RETRY_MIN,
            names_at: Instant::now(),
            crowded: HashSet::new(),
        }
    }

    pub fn run(mut self) {
        self.clear_leftovers();
        loop {
            let ev = match self.stash.pop_front() {
                Some(e) => e,
                None => {
                    let due = self.retry_at.map_or(self.names_at, |t| t.min(self.names_at));
                    match self.rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
                        Ok(e) => e,
                        Err(RecvTimeoutError::Timeout) => {
                            let now = Instant::now();
                            if self.retry_at.is_some_and(|t| t <= now) {
                                self.retry_at = None;
                                self.refresh();
                            }
                            if self.names_at <= now {
                                self.names_at = now + NAMES_EVERY;
                                self.rename_pass();
                            }
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            };
            match ev {
                Event::Want(w) => {
                    if w != self.wants {
                        self.wants = w;
                        self.refresh();
                        self.rename_pass();
                    }
                }
                Event::Inbound { key, frame, peer_pid } => self.deliver(key, frame, peer_pid),
                Event::Subscribe { key, from, msg_id } => self.subscribe(key, from, msg_id),
                Event::Notice { key, from, state, finished_at } => self.notice(key, &from, &state, finished_at),
                Event::Rpc { home, msg } => self.on_note(&home, &msg),
                Event::RpcClosed { home } => {
                    if self.conns.remove(&home).is_some() {
                        lx_warn!("peerbus.daemon.closed", "app-server connection closed", home = home.display().to_string());
                        self.bindings.values_mut().filter(|b| b.home == home).for_each(|b| b.subscribed = false);
                        self.schedule_retry();
                    }
                }
                Event::Stop => break,
            }
        }
        self.teardown();
    }

    // ── daemon notifications ──────────────────────────────────────

    fn on_note(&mut self, home: &Path, msg: &Value) {
        let Some(method) = msg.str_at("method") else { return }; // stale answers
        if msg.get("id").is_some() {
            return; // a request meant for the TUI; the TUI answers it
        }
        let params = msg.get("params");
        let thread = params.and_then(|p| p.str_at("threadId")).map(str::to_string);
        match method {
            "thread/started" => self.refresh(),
            "thread/status/changed" => {
                let Some(thread) = thread else { return };
                let status = params.and_then(|p| p.get("status"));
                if status.and_then(|s| s.str_at("type")) == Some("notLoaded") {
                    self.refresh();
                    return;
                }
                let Some(new) = status.and_then(status_of) else { return };
                let Some(sid) = self.find(home, &thread) else { return };
                // a thread that has now run a turn can be subscribed
                if new == Status::Idle && !self.bindings[&sid].subscribed {
                    let b = &self.bindings[&sid];
                    let (cwd, argv) = (b.record.cwd.clone(), b.argv.clone());
                    let (class, subscribed) = self.class_for(home, &thread, &cwd, &argv);
                    let b = self.bindings.get_mut(&sid).expect("found above");
                    b.subscribed = subscribed;
                    b.class = class;
                    b.record.class = class;
                }
                let b = self.bindings.get_mut(&sid).expect("found above");
                let went_idle = new == Status::Idle && b.record.status != Status::Idle;
                if b.record.status != new || b.record.class != b.class {
                    b.record.status = new;
                    write_record(&registry::default_dirs(), &b.record);
                }
                if went_idle {
                    self.fire(sid, IdleState::Idle);
                }
            }
            "turn/started" | "turn/completed" => {
                let Some(sid) = thread.and_then(|t| self.find(home, &t)) else { return };
                let turn = params.and_then(|p| p.at(&["turn", "id"])).and_then(Value::str).map(str::to_string);
                let b = self.bindings.get_mut(&sid).expect("found above");
                b.active_turn = if method == "turn/started" { turn } else { None };
            }
            _ => {}
        }
    }

    fn find(&self, home: &Path, thread: &str) -> Option<u64> {
        self.bindings.iter().find(|(_, b)| b.home == home && b.thread == thread).map(|(sid, _)| *sid)
    }

    // ── delivery ──────────────────────────────────────────────────

    fn deliver(&mut self, key: u64, frame: UserFrame, peer_pid: Option<i32>) {
        let reply_to = deliver::reply_address(&frame);
        let Some((sid, b)) = self.bindings.iter_mut().find(|(_, b)| b.key == key) else {
            return; // the inbox closed after the frame was read
        };
        let sid = *sid;
        let address = format!("uds:{}", b.inbox.path.display());
        let admitted = deliver::admit(&frame, b.class, &mut b.rate, Instant::now());
        let a = match admitted {
            Ok(a) => a,
            Err(why) => {
                lx_info!("peerbus.refused", &why.reason(), sid = sid, peer_pid = peer_pid.unwrap_or(0));
                if let Some(to) = &reply_to {
                    deliver::send_receipt(to, &address, why.receipt(), frame.msg_id.as_deref(), &why.reason());
                }
                return;
            }
        };
        let (home, thread) = (b.home.clone(), b.thread.clone());
        let steer_turn = b.active_turn.clone().filter(|_| frame.priority == Priority::Now && b.subscribed);
        let id = frame.msg_id.clone().unwrap_or_else(marspot_peer::rand::uuid_v4);
        let (result, via) = self.hand_over(&home, &thread, a.text, steer_turn, id);
        match &result {
            Ok(_) => {
                lx_info!("peerbus.deliver", "queued for the codex session", sid = sid, via = via, peer_pid = peer_pid.unwrap_or(0));
                if let Some(b) = self.bindings.get_mut(&sid) {
                    b.last_delivered = Some(Instant::now());
                }
            }
            Err(e) => {
                let reason = format!("codex did not accept it: {e}");
                lx_warn!("peerbus.deliver_failed", &reason, sid = sid, via = via);
                if let Some(to) = &reply_to {
                    deliver::send_receipt(to, &address, Receipt::Refused, frame.msg_id.as_deref(), &reason);
                }
            }
        }
    }

    /// Put `text` into the thread: steering the running turn when asked
    /// to and able, queued as the next input otherwise.
    pub(super) fn hand_over(&mut self, home: &Path, thread: &str, text: String, steer_turn: Option<String>, id: String) -> (Result<Value, RpcError>, &'static str) {
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

fn status_of(s: &Value) -> Option<Status> {
    match s.str_at("type")? {
        "idle" => Some(Status::Idle),
        "active" => {
            let waiting = s.get("activeFlags").map_or(&[][..], Value::arr).iter().filter_map(Value::str).any(|f| f.starts_with("waiting"));
            Some(if waiting { Status::Waiting } else { Status::Busy })
        }
        _ => None,
    }
}

pub(super) fn write_record(dirs: &[PathBuf], r: &Record) {
    for d in dirs {
        if let Err(e) = registry::write(d, r, now_ms()) {
            lx_warn!("peerbus.registry.write_failed", &e.to_string(), dir = d.display().to_string());
        }
    }
}

/// `<pid>-<8 hex>.sock`: only files of this shape are ours to delete.
pub(super) fn is_inbox_name(path: &str, pid: i32) -> bool {
    let Some(name) = Path::new(path).file_name().and_then(|n| n.to_str()) else { return false };
    let Some(rest) = name.strip_prefix(&format!("{pid}-")).and_then(|r| r.strip_suffix(".sock")) else { return false };
    rest.len() == 8 && rest.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(super) fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_peer::json::parse;

    #[test]
    fn statuses_map_to_registry_words() {
        assert_eq!(status_of(&parse(r#"{"type":"idle"}"#).unwrap()), Some(Status::Idle));
        assert_eq!(status_of(&parse(r#"{"type":"active","activeFlags":[]}"#).unwrap()), Some(Status::Busy));
        assert_eq!(status_of(&parse(r#"{"type":"active","activeFlags":["waitingOnApproval"]}"#).unwrap()), Some(Status::Waiting));
        assert_eq!(status_of(&parse(r#"{"type":"systemError"}"#).unwrap()), None);
    }

    #[test]
    fn only_inbox_shaped_names_are_deleted() {
        assert!(is_inbox_name("/tmp/cc-socks/42-0a1b2c3d.sock", 42));
        assert!(!is_inbox_name("/tmp/cc-socks/42.sock", 42));
        assert!(!is_inbox_name("/tmp/cc-socks/43-0a1b2c3d.sock", 42));
        assert!(!is_inbox_name("/tmp/cc-socks/42-0a1b2c3dx.sock", 42));
    }
}

//! Which session a kimi process has open, and what its record says.
//!
//! kimi holds `<home>/locks/sessions/<session id>.lock` for as long as
//! it has a session open; the file names its pid.  That is the binding:
//! no guess about which process wrote the newest file.  A session
//! exists only once its first prompt was submitted, so a kimi that has
//! just started has none.
//!
//! `<home>/session_index.jsonl` says where each session's directory
//! is, and `agents/main/wire.jsonl` in it is the record: one JSON
//! object per line, read here from its tail only.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use marspot_peer::envelope::Mode;
use marspot_peer::json::{self, Value};
use marspot_peer::registry::Status;

use crate::plugins::pidtree::ProcId;

/// A session id as kimi makes them (`session_<uuid>`), which is also
/// what may be a file name here.
pub(crate) fn is_session_id(s: &str) -> bool {
    marspot_peer::spool::is_session_name(s)
}

/// A lock file is a couple of hundred bytes.
const LOCK_BYTES: u64 = 4096;

/// The session `pid` holds the lock of under `home`.
pub(super) fn session_of_pid(home: &Path, pid: i32) -> Option<String> {
    let rd = std::fs::read_dir(home.join("locks/sessions")).ok()?;
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".lock")).filter(|id| is_session_id(id)) else {
            continue;
        };
        let mut text = String::new();
        let read = std::fs::File::open(e.path()).and_then(|f| f.take(LOCK_BYTES).read_to_string(&mut text));
        if read.is_err() {
            continue;
        }
        let holder = json::parse(&text).ok().and_then(|v| v.get("pid").and_then(Value::i64));
        if holder == Some(i64::from(pid)) {
            return Some(id.to_string());
        }
    }
    None
}

/// How much of the index is read: a line is a few hundred bytes and
/// new sessions are appended, so a session in use is near the end.
const INDEX_BYTES: u64 = 1024 * 1024;

fn tail(path: &Path, bytes: u64) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let from = len.saturating_sub(bytes);
    f.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = Vec::new();
    f.take(bytes).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    // a read that starts mid-file starts mid-line
    let whole = if from > 0 { text.split_once('\n').map_or("", |(_, rest)| rest) } else { &text };
    Some(whole.to_string())
}

/// A name kimi gives a workspace or a session directory.
fn plain(name: &str) -> bool {
    !name.is_empty() && name.len() <= 255 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The directory of session `id` under `home`.
///
/// The index records an absolute path; only its last two components
/// are taken from it and joined under this home, so what a file
/// another program writes can name is a directory inside
/// `<home>/sessions` and nothing else.
pub(crate) fn session_dir(home: &Path, id: &str) -> Option<PathBuf> {
    let index = tail(&home.join("session_index.jsonl"), INDEX_BYTES)?;
    let needle = format!("\"{id}\"");
    let line = index.lines().rev().find(|l| l.contains(&needle))?;
    let v = json::parse(line).ok()?;
    if v.str_at("sessionId") != Some(id) {
        return None;
    }
    let recorded = Path::new(v.str_at("sessionDir")?);
    let session = recorded.file_name()?.to_str()?;
    let workspace = recorded.parent()?.file_name()?.to_str()?;
    (session == id && plain(workspace)).then(|| home.join("sessions").join(workspace).join(session))
}

/// The main agent's record of session `id` under `home`, if it exists.
pub(crate) fn wire_of(home: &Path, id: &str) -> Option<PathBuf> {
    let path = session_dir(home, id)?.join("agents/main/wire.jsonl");
    path.is_file().then_some(path)
}

/// Why the last turn ended, when it did not end well.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Failure {
    /// `turn.ended.error.code`.
    pub code: String,
    /// `turn.ended.turnId`, so one failure is acted on once.
    pub turn: i64,
}

/// What the tail of a record says.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Facts {
    pub model: Option<String>,
    pub status: Status,
    pub class: Mode,
    /// Set while the last turn is one that failed.
    pub failure: Option<Failure>,
}

/// How much of a record's end is read.  Every model request is one
/// short record; a turn whose tool output exceeds this leaves the
/// model and the state as they were last read.
const TAIL_BYTES: u64 = 256 * 1024;

fn typed(line: &str, ty: &str) -> Option<Value> {
    // the type leads every record this reads; a tool's output further
    // into some other line cannot pass for one
    let rest = line.strip_prefix(r#"{"type":""#)?;
    (rest.strip_prefix(ty)?.starts_with('"')).then(|| json::parse(line).ok()).flatten()
}

/// The model as the badge shows it: kimi's alias without the provider
/// it is filed under, or the model's own name for one that was defined
/// by environment variables and has no alias of its own.
fn model_name(request: &Value) -> Option<String> {
    let alias = request.str_at("modelAlias").filter(|a| !a.is_empty() && !a.starts_with("__"));
    let name = alias.or_else(|| request.str_at("model").filter(|m| !m.is_empty()))?;
    let name = name.strip_prefix("kimi-code/").unwrap_or(name);
    (name.len() <= 64 && name.bytes().all(|b| b.is_ascii_graphic())).then(|| name.to_string())
}

/// Read `text`, the end of a record, into facts.  None when nothing in
/// it says what the session is doing, in which case nothing is assumed.
pub(super) fn facts_in(text: &str) -> Option<Facts> {
    let mut model = None;
    let mut status = None;
    let mut class = Mode::Prompting;
    let mut failure = None;
    let mut asked: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(v) = typed(line, "llm.request") {
            model = model_name(&v).or(model);
        } else if typed(line, "turn.prompt").is_some() || line.starts_with(r#"{"type":"context.append_loop_event""#) {
            if status != Some(Status::Busy) {
                asked.clear();
                failure = None;
            }
            status = Some(Status::Busy);
        } else if let Some(v) = typed(line, "turn.ended") {
            status = Some(Status::Idle);
            asked.clear();
            failure = (v.str_at("reason") == Some("failed"))
                .then(|| {
                    let code = v.at(&["error", "code"]).and_then(Value::str)?;
                    Some(Failure { code: code.to_string(), turn: v.get("turnId").and_then(Value::i64)? })
                })
                .flatten();
        } else if let Some(v) = typed(line, "interaction.request") {
            asked.extend(v.str_at("id").map(str::to_string));
        } else if let Some(v) = typed(line, "interaction.resolved") {
            asked.retain(|id| Some(id.as_str()) != v.str_at("id"));
        } else if let Some(v) = typed(line, "permission.set_mode") {
            // `auto` still stops for some approvals; only `yolo` never asks
            class = if v.str_at("mode") == Some("yolo") { Mode::Bypass } else { Mode::Prompting };
        }
    }
    let status = match status? {
        Status::Busy if !asked.is_empty() => Status::Waiting,
        s => s,
    };
    Some(Facts { model, status, class, failure })
}

struct Bound {
    id: String,
    wire: PathBuf,
    read_at: Option<SystemTime>,
    facts: Option<Facts>,
}

/// The session of each kimi process, and what its record last said.
///
/// Finding the session lists a directory, so it happens on a slow
/// cadence; between searches a bound process costs one `stat`, and the
/// tail is read again only when the file was written.  Keyed by
/// process and cut back to the live ones every tick.
#[derive(Default)]
pub(super) struct SessionIndex {
    bound: HashMap<ProcId, Bound>,
    searched_at: HashMap<ProcId, Instant>,
}

impl SessionIndex {
    /// How often a process's lock is looked for again.  Not only while
    /// it has none: `/new` opens another session under the same
    /// process.
    const SEARCH_EVERY: Duration = Duration::from_secs(6);

    pub(super) fn retain(&mut self, live: impl Fn(ProcId) -> bool) {
        self.bound.retain(|id, _| live(*id));
        self.searched_at.retain(|id, _| live(*id));
    }

    /// The session `proc` has open under `home` and its facts; None
    /// while it has none.
    pub(super) fn look(&mut self, proc: ProcId, home: &Path) -> Option<(String, Option<Facts>)> {
        if self.searched_at.get(&proc).is_none_or(|t| t.elapsed() >= Self::SEARCH_EVERY) {
            self.searched_at.insert(proc, Instant::now());
            let found = session_of_pid(home, proc.pid).and_then(|id| Some((wire_of(home, &id)?, id)));
            match found {
                Some((wire, id)) => {
                    if self.bound.get(&proc).is_none_or(|b| b.id != id) {
                        self.bound.insert(proc, Bound { id, wire, read_at: None, facts: None });
                    }
                }
                None => {
                    self.bound.remove(&proc);
                }
            }
        }
        let bound = self.bound.get_mut(&proc)?;
        let Ok(written) = std::fs::metadata(&bound.wire).and_then(|m| m.modified()) else {
            self.bound.remove(&proc);
            return None;
        };
        if bound.read_at != Some(written) {
            bound.read_at = Some(written);
            // a tail nothing can be read from leaves the last reading
            if let Some(facts) = tail(&bound.wire, TAIL_BYTES).as_deref().and_then(facts_in) {
                bound.facts = Some(facts);
            }
        }
        Some((bound.id.clone(), bound.facts.clone()))
    }
}

/// `default_model` in `<home>/config.toml`, for a kimi with no session
/// yet.  One top-level key read by line; the file is the user's and is
/// not parsed further.
pub(super) fn default_model(home: &Path) -> Option<String> {
    let text = std::fs::read_to_string(home.join("config.toml")).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            return None; // top-level keys come before the first table
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        if key.trim() != "default_model" {
            continue;
        }
        let name = value.trim().trim_matches('"');
        let name = name.strip_prefix("kimi-code/").unwrap_or(name);
        return (!name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_graphic() && b != b'"'))
            .then(|| name.to_string());
    }
    None
}

#[cfg(test)]
pub(crate) mod fixture {
    use std::path::{Path, PathBuf};

    /// Records a 2.5.3 build wrote against a local model server, with
    /// the lines over 2 KB (tool and profile snapshots) left out.
    pub(crate) const CANCELLED_THEN_COMPLETED: &str = include_str!("fixtures/wire-cancelled-then-completed.jsonl");
    pub(crate) const STOP_HOOK: &str = include_str!("fixtures/wire-stop-hook.jsonl");
    pub(crate) const TOOL_CALL: &str = include_str!("fixtures/wire-tool-call.jsonl");

    pub(crate) const ID: &str = "session_84eb6e0a-2c78-40a4-b453-dc017d464a35";

    /// A home holding session `id` with `wire` as its record, locked
    /// by `pid`.  Returns the record's path.
    pub(crate) fn session(home: &Path, id: &str, wire: &str, pid: Option<i32>) -> PathBuf {
        let dir = home.join("sessions/wd_a_0123456789ab").join(id);
        std::fs::create_dir_all(dir.join("agents/main")).unwrap();
        std::fs::write(dir.join("agents/main/wire.jsonl"), wire).unwrap();
        let line = format!("{{\"sessionId\":\"{id}\",\"sessionDir\":\"/elsewhere/sessions/wd_a_0123456789ab/{id}\",\"workDir\":\"/w/a\"}}\n");
        let mut index = std::fs::read_to_string(home.join("session_index.jsonl")).unwrap_or_default();
        index.push_str(&line);
        std::fs::write(home.join("session_index.jsonl"), index).unwrap();
        if let Some(pid) = pid {
            std::fs::create_dir_all(home.join("locks/sessions")).unwrap();
            let lock = format!(
                "{{\"version\":1,\"token\":\"{pid}:0d9f6c1e-5a55-4c3e-9a41-7f3d0c8f5b21\",\"pid\":{pid},\"hostname\":\"host.local\",\"startedAt\":1794351557245,\"clientIdentity\":\"kimi_code_cli\"}}"
            );
            std::fs::write(home.join("locks/sessions").join(format!("{id}.lock")), lock).unwrap();
        }
        dir.join("agents/main/wire.jsonl")
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;
    use crate::plugins::handoff::history::tmp;

    fn upto(wire: &str, last: &str) -> String {
        let end = wire.find(last).expect("the record is in the fixture");
        let line_end = wire[end..].find('\n').map_or(wire.len(), |n| end + n + 1);
        wire[..line_end].to_string()
    }

    #[test]
    fn a_turn_in_flight_is_busy_and_an_ended_one_is_idle() {
        let all = facts_in(CANCELLED_THEN_COMPLETED).expect("facts");
        assert_eq!(all.status, Status::Idle);
        assert_eq!(all.model.as_deref(), Some("mock-model"), "an environment-defined model has no alias worth showing");
        assert_eq!(all.failure, None);
        assert_eq!(all.class, Mode::Bypass, "the last record switched the session to yolo");

        let first_prompt = upto(CANCELLED_THEN_COMPLETED, r#""type":"turn.prompt""#);
        let f = facts_in(&first_prompt).expect("facts");
        assert_eq!((f.status, f.class, f.model), (Status::Busy, Mode::Prompting, None));

        let cancelled = upto(CANCELLED_THEN_COMPLETED, r#""reason":"cancelled""#);
        assert_eq!(facts_in(&cancelled).unwrap().status, Status::Idle, "an interrupted turn is over");

        let second = upto(CANCELLED_THEN_COMPLETED, r#""turnStep":"1.1""#);
        assert_eq!(facts_in(&second).unwrap().status, Status::Busy);
    }

    #[test]
    fn a_turn_the_stop_hook_continued_is_one_turn() {
        let continued = upto(STOP_HOOK, r#""name":"stop_hook""#);
        assert_eq!(facts_in(&continued).unwrap().status, Status::Busy, "the first reply ended a step, not the turn");
        assert_eq!(facts_in(STOP_HOOK).unwrap().status, Status::Idle);
    }

    #[test]
    fn a_tail_that_starts_inside_a_turn_is_busy() {
        let from = TOOL_CALL.find(r#"{"type":"context.append_loop_event","agentId":"main","event":{"type":"tool.call""#).unwrap();
        let end = TOOL_CALL.find(r#"{"type":"turn.ended""#).unwrap();
        assert_eq!(facts_in(&TOOL_CALL[from..end]).unwrap().status, Status::Busy);
        assert_eq!(facts_in(TOOL_CALL).unwrap().status, Status::Idle);
        assert_eq!(facts_in(r#"{"type":"metadata","protocol_version":"1.5","created_at":1}"#), None);
        assert_eq!(facts_in("not a record\n{\"type\":\"turn.prompt\n"), None, "a record of some other shape says nothing");
    }

    #[test]
    fn an_unanswered_request_is_waiting_for_the_user() {
        let busy = upto(TOOL_CALL, r#""type":"tool.call""#);
        let ask = r#"{"type":"interaction.request","agentId":"main","id":"int_1","kind":"approval","toolCallId":"call_probe1","request":{},"time":1}"#;
        let asked = format!("{busy}{ask}\n");
        assert_eq!(facts_in(&asked).unwrap().status, Status::Waiting);
        let answered = format!("{asked}{}\n", r#"{"type":"interaction.resolved","agentId":"main","id":"int_1","response":{},"time":2}"#);
        assert_eq!(facts_in(&answered).unwrap().status, Status::Busy);
    }

    #[test]
    fn a_failed_turn_names_its_error() {
        let failed = r#"{"type":"turn.ended","agentId":"main","turnId":3,"reason":"failed","error":{"code":"provider.quota_exhausted","message":"x","retryable":false},"durationMs":10,"time":1}"#;
        let f = facts_in(&format!("{TOOL_CALL}{failed}\n")).unwrap();
        assert_eq!(f.status, Status::Idle);
        assert_eq!(f.failure, Some(Failure { code: "provider.quota_exhausted".into(), turn: 3 }));
        let next = format!("{TOOL_CALL}{failed}\n{}\n", r#"{"type":"turn.prompt","agentId":"main","input":[],"origin":{"kind":"user"},"turnId":4,"time":2}"#);
        assert_eq!(facts_in(&next).unwrap().failure, None, "the next turn is not the one that failed");
    }

    #[test]
    fn the_session_is_the_one_whose_lock_names_the_process() {
        let home = tmp("kimi-lock");
        let wire = session(&home, ID, CANCELLED_THEN_COMPLETED, Some(4242));
        session(&home, "session_other", STOP_HOOK, Some(77));
        assert_eq!(session_of_pid(&home, 4242).as_deref(), Some(ID));
        assert_eq!(session_of_pid(&home, 4243), None, "a process with no session open");
        assert_eq!(wire_of(&home, ID), Some(wire));
        assert_eq!(wire_of(&home, "session_missing"), None);
        std::fs::write(home.join("locks/sessions/..%2f.lock"), r#"{"pid":9}"#).unwrap();
        std::fs::write(home.join("locks/sessions/session_bad.lock"), "not json").unwrap();
        assert_eq!(session_of_pid(&home, 9), None, "a name that is not a session id");
        assert_eq!(session_of_pid(&tmp("kimi-no-home"), 4242), None, "a home kimi never ran in");
    }

    #[test]
    fn an_index_cannot_point_outside_the_home() {
        let home = tmp("kimi-index");
        std::fs::write(
            home.join("session_index.jsonl"),
            "{\"sessionId\":\"session_a\",\"sessionDir\":\"/x/../session_a\",\"workDir\":\"/w\"}\n\
             {\"sessionId\":\"session_b\",\"sessionDir\":\"/h/sessions/wd_b_1/session_c\",\"workDir\":\"/w\"}\n\
             {\"sessionId\":\"session_d\",\"sessionDir\":\"/h/sessions/wd_d_1/session_d\",\"workDir\":\"/w\"}\n",
        )
        .unwrap();
        assert_eq!(session_dir(&home, "session_a"), None);
        assert_eq!(session_dir(&home, "session_b"), None, "the directory is named after the session");
        assert_eq!(session_dir(&home, "session_d"), Some(home.join("sessions/wd_d_1/session_d")));
    }

    #[test]
    fn the_index_follows_the_lock_and_keeps_only_live_processes() {
        let home = tmp("kimi-look");
        let proc = ProcId { pid: 4242, start_us: 1 };
        let mut idx = SessionIndex::default();
        assert_eq!(idx.look(proc, &home), None, "no session before the first prompt");
        session(&home, ID, CANCELLED_THEN_COMPLETED, Some(4242));
        let mut idx = SessionIndex::default();
        let (id, facts) = idx.look(proc, &home).expect("bound");
        assert_eq!(id, ID);
        assert_eq!(facts.unwrap().status, Status::Idle);
        idx.retain(|_| false);
        assert!(idx.bound.is_empty() && idx.searched_at.is_empty());
    }

    #[test]
    fn the_default_model_is_the_top_level_key() {
        let home = tmp("kimi-default-model");
        assert_eq!(default_model(&home), None);
        std::fs::write(home.join("config.toml"), "default_model = \"kimi-code/k3\"\n\n[[hooks]]\nevent = \"Stop\"\n").unwrap();
        assert_eq!(default_model(&home).as_deref(), Some("k3"));
        std::fs::write(home.join("config.toml"), "[models.x]\ndefault_model = \"inner\"\n").unwrap();
        assert_eq!(default_model(&home), None, "a key of the same name inside a table is another key");
    }
}

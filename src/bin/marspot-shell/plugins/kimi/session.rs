//! Which session a kimi process has open, and what its record says.
//!
//! Two things say which session is a process's.  While a turn runs,
//! kimi holds `<home>/locks/sessions/<session id>.lock`, and the file
//! names its pid: that is certain, but it is there only for the length
//! of the turn.  Between turns the session is the one in the
//! process's directory that was written since the process started,
//! which `<home>/session_index.jsonl` answers: it lists every session
//! with the directory it was opened in.  That second answer is taken
//! only for a kimi that shares its home and directory with no other.
//! A session exists only once its first prompt was submitted, so a
//! kimi that has just started has none.
//!
//! `agents/main/wire.jsonl` in the session's directory is the record:
//! one JSON object per line, read here from its tail only.

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

/// The directory an index line gives its session, with the session's
/// id.
///
/// The index records an absolute path; only its last two components
/// are taken from it and joined under this home, so what a file
/// another program writes can name is a directory inside
/// `<home>/sessions` and nothing else.
fn indexed(home: &Path, line: &Value) -> Option<(String, PathBuf)> {
    let id = line.str_at("sessionId").filter(|id| is_session_id(id))?;
    let recorded = Path::new(line.str_at("sessionDir")?);
    let session = recorded.file_name()?.to_str()?;
    let workspace = recorded.parent()?.file_name()?.to_str()?;
    (session == id && plain(workspace)).then(|| (id.to_string(), home.join("sessions").join(workspace).join(session)))
}

/// The directory of session `id` under `home`.
pub(crate) fn session_dir(home: &Path, id: &str) -> Option<PathBuf> {
    let index = tail(&home.join("session_index.jsonl"), INDEX_BYTES)?;
    let needle = format!("\"{id}\"");
    let line = index.lines().rev().find(|l| l.contains(&needle))?;
    indexed(home, &json::parse(line).ok()?).filter(|(found, _)| found == id).map(|(_, dir)| dir)
}

/// How far a record's last write may trail the start of the process
/// that wrote it: the two clocks are not read at the same instant.
const START_SLACK: Duration = Duration::from_secs(2);

/// The session opened in `cwd` whose record was written last, among
/// those written since `started`.
fn newest_in(home: &Path, cwd: &str, started: SystemTime) -> Option<String> {
    let index = tail(&home.join("session_index.jsonl"), INDEX_BYTES)?;
    let real = std::fs::canonicalize(cwd).ok();
    let here = |dir: &str| dir == cwd || real.as_deref().is_some_and(|r| Path::new(dir) == r);
    index
        .lines()
        .filter_map(|l| json::parse(l).ok())
        .filter(|v| v.str_at("workDir").is_some_and(here))
        .filter_map(|v| indexed(home, &v))
        .filter_map(|(id, dir)| {
            let written = std::fs::metadata(dir.join("agents/main/wire.jsonl")).and_then(|m| m.modified()).ok()?;
            (written + START_SLACK >= started).then_some((written, id))
        })
        .max_by_key(|(written, _)| *written)
        .map(|(_, id)| id)
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
    /// `turn.ended.error.name`: the code is shared by errors of
    /// different kinds, and the name is what tells them apart.
    pub name: Option<String>,
    /// `turn.ended.time`, as unix seconds.
    pub at: i64,
    /// `turn.ended.turnId`.
    pub turn: i64,
}

/// What the tail of a record says.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Facts {
    /// The model the next request goes to, by the name kimi's
    /// configuration has for it (`kimi-code/k3`).
    pub alias: Option<String>,
    /// The provider's own name for it, when a request made under this
    /// alias has said it.
    pub model: Option<String>,
    /// `off`, `on`, or an effort the model declares.
    pub thinking: Option<String>,
    pub status: Status,
    pub class: Mode,
    /// Set while the last turn is one that failed.
    pub failure: Option<Failure>,
    /// The number of the last turn that ended.  It moves with every
    /// turn, however short, which a state sampled now and then does not.
    pub ended: Option<i64>,
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

/// The alias kimi gives a model defined by `KIMI_MODEL_*` variables,
/// which names nothing a person would recognise.
pub(super) const ENV_ALIAS: &str = "__kimi_env_model__";

/// The model and the thinking setting, folded over the records that
/// carry them.
///
/// `profile.bind` opens a session with both, `config.update` is written
/// when the user changes either (`/model`, `/effort`) and carries only
/// what changed, and every request says what it was sent with.  kimi
/// rebuilds its own state from the first two in order, so the last
/// record to name a field is what the next request uses; a request's
/// own word is taken too, because in a long session the tail may hold
/// nothing else.
#[derive(Default)]
struct Setting {
    alias: Option<String>,
    /// The provider's name for the model, and the alias it was said under.
    model: Option<(String, String)>,
    thinking: Option<String>,
}

impl Setting {
    fn take(&mut self, v: &Value, request: bool) {
        let named = |key: &str| v.str_at(key).filter(|s| super::config::plain(s) || *s == ENV_ALIAS).map(str::to_string);
        if let Some(alias) = named("modelAlias") {
            if request && let Some(model) = named("model") {
                self.model = Some((model, alias.clone()));
            }
            self.alias = Some(alias);
        }
        // `thinkingLevel` is what older records call it
        if let Some(effort) = named("thinkingEffort").or_else(|| named("thinkingLevel")) {
            self.thinking = Some(effort);
        }
    }

    fn model(&self) -> Option<String> {
        let (model, under) = self.model.as_ref()?;
        (Some(under) == self.alias.as_ref()).then(|| model.clone())
    }
}

/// Read `text`, the end of a record, into facts.  None when nothing in
/// it says what the session is doing, in which case nothing is assumed.
pub(super) fn facts_in(text: &str) -> Option<Facts> {
    let mut setting = Setting::default();
    let mut status = None;
    let mut class = Mode::Prompting;
    let mut failure = None;
    let mut ended = None;
    let mut asked: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(v) = typed(line, "llm.request") {
            // a compaction may run on another model than the turn's
            if v.str_at("kind") == Some("loop") {
                setting.take(&v, true);
            }
        } else if let Some(v) = typed(line, "profile.bind").or_else(|| typed(line, "config.update")) {
            setting.take(&v, false);
        } else if typed(line, "turn.prompt").is_some() || line.starts_with(r#"{"type":"context.append_loop_event""#) {
            if status != Some(Status::Busy) {
                asked.clear();
                failure = None;
            }
            status = Some(Status::Busy);
        } else if let Some(v) = typed(line, "turn.ended") {
            status = Some(Status::Idle);
            ended = v.get("turnId").and_then(Value::i64).or(ended);
            asked.clear();
            failure = (v.str_at("reason") == Some("failed"))
                .then(|| {
                    let code = v.at(&["error", "code"]).and_then(Value::str)?;
                    let name = v.at(&["error", "name"]).and_then(Value::str).map(str::to_string);
                    let at = v.get("time").and_then(Value::i64)? / 1000;
                    Some(Failure { code: code.to_string(), name, at, turn: v.get("turnId").and_then(Value::i64)? })
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
    Some(Facts { model: setting.model(), alias: setting.alias, thinking: setting.thinking, status, class, failure, ended })
}

/// A kimi process asking for its session.
pub(super) struct Asker<'a> {
    pub proc: ProcId,
    pub started: SystemTime,
    pub home: &'a Path,
    pub cwd: Option<&'a str>,
    /// No other kimi shares this home and directory.  When one does,
    /// the newest session there could be either one's.
    pub lone: bool,
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
    /// How often a process's session is looked for again.  Not only
    /// while it has none: `/new` opens another session under the same
    /// process.  Short, because a lock is there only while a turn runs.
    const SEARCH_EVERY: Duration = Duration::from_secs(2);

    pub(super) fn retain(&mut self, live: impl Fn(ProcId) -> bool) {
        self.bound.retain(|id, _| live(*id));
        self.searched_at.retain(|id, _| live(*id));
    }

    /// The session `who` has open and its facts; None while it has
    /// none, or while it cannot be told which is its own.
    pub(super) fn look(&mut self, who: &Asker) -> Option<(String, Option<Facts>)> {
        let proc = who.proc;
        if self.searched_at.get(&proc).is_none_or(|t| t.elapsed() >= Self::SEARCH_EVERY) {
            self.searched_at.insert(proc, Instant::now());
            // the lock is certain; without one a lone kimi's session is
            // the newest in its directory, and one that is not alone
            // keeps what a lock last said
            let id = session_of_pid(who.home, proc.pid)
                .or_else(|| who.cwd.filter(|_| who.lone).and_then(|cwd| newest_in(who.home, cwd, who.started)));
            if let Some((wire, id)) = id.and_then(|id| Some((wire_of(who.home, &id)?, id)))
                && self.bound.get(&proc).is_none_or(|b| b.id != id)
            {
                self.bound.insert(proc, Bound { id, wire, read_at: None, facts: None });
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
        assert_eq!((all.alias.as_deref(), all.model.as_deref()), (Some(ENV_ALIAS), Some("mock-model")));
        assert_eq!(all.thinking.as_deref(), Some("on"), "what the requests were sent with");
        assert_eq!(all.failure, None);
        assert_eq!(all.ended, Some(1), "the second turn is the last that ended");
        assert_eq!(all.class, Mode::Bypass, "the last record switched the session to yolo");

        let first_prompt = upto(CANCELLED_THEN_COMPLETED, r#""type":"turn.prompt""#);
        let f = facts_in(&first_prompt).expect("facts");
        assert_eq!((f.status, f.class, f.model, f.thinking), (Status::Busy, Mode::Prompting, None, None));

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

    /// The record a 2.5.3 build wrote when the model server answered
    /// 429 with `exceeded_current_quota_error`.  The code is the one
    /// every provider error carries; the name is what says quota.
    #[test]
    fn a_failed_turn_names_its_error() {
        let failed = r#"{"type":"turn.ended","agentId":"main","turnId":1,"reason":"failed","error":{"code":"provider.api_error","message":"You have exceeded your current quota.","name":"APIProviderQuotaExhaustedError","details":{"statusCode":429,"requestId":null,"traceId":null,"diagnostics":{}},"retryable":false},"durationMs":35,"time":1794354224601}"#;
        let f = facts_in(&format!("{TOOL_CALL}{failed}\n")).unwrap();
        assert_eq!(f.status, Status::Idle);
        let quota = Failure { code: "provider.api_error".into(), name: Some("APIProviderQuotaExhaustedError".into()), at: 1_794_354_224, turn: 1 };
        assert_eq!(f.failure, Some(quota));
        let next = format!("{TOOL_CALL}{failed}\n{}\n", r#"{"type":"turn.prompt","agentId":"main","input":[],"origin":{"kind":"user"},"turnId":2,"time":2}"#);
        assert_eq!(facts_in(&next).unwrap().failure, None, "the next turn is not the one that failed");
    }

    #[test]
    fn a_change_of_model_or_effort_shows_before_the_next_request() {
        let bind = r#"{"type":"profile.bind","agentId":"main","modelAlias":"kimi-code/k3","profileName":"agent","thinkingEffort":"high","systemPrompt":"…","disallowedTools":[],"time":1}"#;
        let request = r#"{"type":"llm.request","agentId":"main","kind":"loop","provider":"kimi","model":"k3","modelAlias":"kimi-code/k3","thinkingEffort":"high","thinkingKeep":"all","toolSelect":false,"messageCount":2,"turnStep":"0.1","time":2}"#;
        let prompt = r#"{"type":"turn.prompt","agentId":"main","input":[],"origin":{"kind":"user"},"turnId":0,"time":2}"#;
        let ended = r#"{"type":"turn.ended","agentId":"main","turnId":0,"reason":"completed","durationMs":1,"time":3}"#;
        let got = |lines: &[&str]| {
            let f = facts_in(&lines.join("\n")).expect("facts");
            (f.alias, f.model, f.thinking)
        };
        let s = |a: &str, m: Option<&str>, t: &str| (Some(a.to_string()), m.map(str::to_string), Some(t.to_string()));
        assert_eq!(got(&[bind, prompt]), s("kimi-code/k3", None, "high"), "bound, nothing sent yet");
        assert_eq!(got(&[bind, prompt, request, ended]), s("kimi-code/k3", Some("k3"), "high"));

        let effort = r#"{"type":"config.update","agentId":"main","thinkingEffort":"low","time":4}"#;
        assert_eq!(got(&[bind, prompt, request, ended, effort]), s("kimi-code/k3", Some("k3"), "low"), "only the effort changed");

        let model = r#"{"type":"config.update","agentId":"main","modelAlias":"kimi-code/kimi-for-coding","thinkingEffort":"max","time":5}"#;
        assert_eq!(
            got(&[bind, prompt, request, ended, effort, model]),
            s("kimi-code/kimi-for-coding", None, "max"),
            "the provider's name for the old model is not the new one's"
        );
        // a resumed session whose tail no longer reaches its binding
        assert_eq!(got(&[prompt, request, ended]), s("kimi-code/k3", Some("k3"), "high"));
        let compaction = request.replace(r#""kind":"loop""#, r#""kind":"compaction""#).replace("kimi-code/k3", "kimi-code/small");
        assert_eq!(got(&[prompt, request, &compaction, ended]), s("kimi-code/k3", Some("k3"), "high"));
        let legacy = r#"{"type":"config.update","agentId":"main","thinkingLevel":"off","time":6}"#;
        assert_eq!(got(&[prompt, request, ended, legacy]).2.as_deref(), Some("off"));
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

    fn asker<'a>(home: &'a Path, cwd: Option<&'a str>, lone: bool) -> Asker<'a> {
        Asker { proc: ProcId { pid: 4242, start_us: 1 }, started: SystemTime::now() - Duration::from_secs(1), home, cwd, lone }
    }

    #[test]
    fn the_index_follows_the_lock_and_keeps_only_live_processes() {
        let home = tmp("kimi-look");
        let mut idx = SessionIndex::default();
        assert_eq!(idx.look(&asker(&home, Some("/w/a"), true)), None, "no session before the first prompt");
        session(&home, ID, CANCELLED_THEN_COMPLETED, Some(4242));
        let mut idx = SessionIndex::default();
        let (id, facts) = idx.look(&asker(&home, None, false)).expect("bound by the lock alone");
        assert_eq!(id, ID);
        assert_eq!(facts.unwrap().status, Status::Idle);
        // the turn ends and the lock goes; the session stays the process's
        std::fs::remove_file(home.join("locks/sessions").join(format!("{ID}.lock"))).unwrap();
        idx.searched_at.clear();
        assert_eq!(idx.look(&asker(&home, None, false)).map(|(id, _)| id).as_deref(), Some(ID));
        idx.retain(|_| false);
        assert!(idx.bound.is_empty() && idx.searched_at.is_empty());
    }

    /// Between turns there is no lock.  The session is then the one in
    /// the process's directory written since the process started.
    #[test]
    fn an_idle_kimi_is_bound_to_the_session_written_in_its_directory_since_it_started() {
        let home = tmp("kimi-newest");
        let old = session(&home, "session_old", STOP_HOOK, None);
        let before = SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options().write(true).open(&old).unwrap().set_modified(before).unwrap();
        let mut idx = SessionIndex::default();
        assert_eq!(idx.look(&asker(&home, Some("/w/a"), true)), None, "a session older than the process is not its own");
        session(&home, ID, CANCELLED_THEN_COMPLETED, None);
        let mut idx = SessionIndex::default();
        assert_eq!(idx.look(&asker(&home, Some("/w/a"), true)).map(|(id, _)| id).as_deref(), Some(ID));
        let mut idx = SessionIndex::default();
        assert_eq!(idx.look(&asker(&home, Some("/w/elsewhere"), true)), None, "a session of another directory");
        let mut idx = SessionIndex::default();
        assert_eq!(idx.look(&asker(&home, Some("/w/a"), false)), None, "two kimis in one directory: neither is guessed at");
        let mut idx = SessionIndex::default();
        assert_eq!(idx.look(&asker(&home, None, true)), None, "a process whose directory cannot be read");
    }
}

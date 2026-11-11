//! The Claude Code session registry: `<config dir>/sessions/<pid>.json`,
//! one record per live session, read by every session to list its
//! peers and find their inbox sockets.
//!
//! marspot writes records here for Codex, Gemini and Kimi sessions, keyed by
//! the pid of each session's relay, so Claude sessions list them and
//! message them like any peer.  Those records carry `entrypoint = MARKER`; that marker is
//! the only thing that makes a record ours, and only ours are ever
//! rewritten or removed.
//!
//! A reader counts a record as live while its pid exists and the pid's
//! start time still equals `procStart` (see `proc`).  Claude Code
//! deletes records whose process is gone, ours included.
//!
//! A Claude session's own record is never written, with one exception:
//! `restore_foreign` undoes the renames an earlier marspot made there.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::envelope::Mode;
use crate::json::{self, Value, obj};
use crate::proc;

pub const MARKER: &str = "marspot-codex";
/// Our own field: the session's permission class, which the sender
/// CLI reads back to fill `from-mode`.  Claude Code ignores it.
const CLASS_KEY: &str = "marspotPermissionClass";
/// Our own fields: which agent the record stands for and its process
/// (the record's `pid` is its relay), and the relay's send socket.
const AGENT_KEY: &str = "marspotAgent";
const AGENT_PID_KEY: &str = "marspotAgentPid";
const RELAY_KEY: &str = "marspotRelay";
/// Our own field: the file a Gemini or Kimi session's mail waits in (`spool`).
const SPOOL_KEY: &str = "marspotSpool";
/// Our own field: which marspot wrote the record (its state root).  A
/// restarting marspot clears its own leftovers and nobody else's — a
/// sandbox instance and the installed app share this directory.
const OWNER_KEY: &str = "marspotOwner";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Idle,
    Busy,
    Waiting,
    /// Nothing reports it, and the record then carries no status at
    /// all: a guess would read to a sender as the session's own word.
    Unknown,
}

impl Status {
    fn as_str(self) -> Option<&'static str> {
        match self {
            Status::Idle => Some("idle"),
            Status::Busy => Some("busy"),
            Status::Waiting => Some("waiting"),
            Status::Unknown => None,
        }
    }
}

/// Which agent a record of ours stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agent {
    Codex,
    Gemini,
    Kimi,
}

impl Agent {
    pub fn as_str(self) -> &'static str {
        match self {
            Agent::Codex => "codex",
            Agent::Gemini => "gemini",
            Agent::Kimi => "kimi",
        }
    }

    /// The agent's name as a listing prints it.
    pub fn label(self) -> &'static str {
        match self {
            Agent::Codex => "Codex",
            Agent::Gemini => "Gemini",
            Agent::Kimi => "Kimi",
        }
    }

    /// What a session of this agent supports beyond plain messages.
    /// Nothing tells marspot when a Gemini session goes idle, so it
    /// does not offer the notice: a sender is told that nothing was
    /// subscribed instead of waiting for a notice that cannot come.
    /// A Kimi session's record says when each turn ends.
    pub fn features(self) -> &'static [&'static str] {
        match self {
            Agent::Codex | Agent::Kimi => FEATURES,
            Agent::Gemini => &[],
        }
    }
}

/// What this side of the protocol supports beyond plain messages.
pub const FEATURES: &[&str] = &["notify_idle"];

/// A record marspot writes for one Codex or Gemini session.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    /// The session's relay process (`relay`), which serves its inbox.
    pub pid: i32,
    pub agent: Agent,
    /// The agent's own process; for Gemini the one nearest the shell.
    pub agent_pid: i32,
    /// The relay's send socket.
    pub relay: String,
    /// The Codex thread id, or the Gemini session id.
    pub session_id: String,
    pub cwd: String,
    pub started_at_ms: i64,
    /// `proc::lstart` of `pid`.
    pub proc_start: String,
    /// The agent's version, empty when it is not known.
    pub version: String,
    pub name: String,
    pub name_since_ms: i64,
    /// Absolute path of the inbox socket.
    pub socket: String,
    pub status: Status,
    pub class: Mode,
    pub owner: String,
    /// Where a Gemini session's mail waits for its next turn.
    pub spool: Option<String>,
}

impl Record {
    pub fn to_json(&self, now_ms: i64) -> String {
        let mut v = obj([
            ("pid", Value::from(i64::from(self.pid))),
            ("sessionId", Value::from(self.session_id.as_str())),
            ("cwd", Value::from(self.cwd.as_str())),
            ("startedAt", Value::from(self.started_at_ms)),
            ("procStart", Value::from(self.proc_start.as_str())),
            ("version", Value::from(self.version.as_str())),
            ("peerProtocol", Value::from(1)),
            ("peerFeatures", Value::Arr(self.agent.features().iter().map(|f| Value::from(*f)).collect())),
            ("kind", Value::from("interactive")),
            ("entrypoint", Value::from(MARKER)),
            ("pidDomain", Value::from("darwin")),
            ("messagingSocketPath", Value::from(self.socket.as_str())),
            ("name", Value::from(self.name.as_str())),
            ("nameSource", Value::from("peer")),
            ("nameSince", Value::from(self.name_since_ms)),
            // a record always has the key; these sessions have no earlier names
            ("formerNames", Value::Arr(Vec::new())),
            ("updatedAt", Value::from(now_ms)),
            (CLASS_KEY, Value::from(self.class.as_str())),
            (AGENT_KEY, Value::from(self.agent.as_str())),
            (AGENT_PID_KEY, Value::from(i64::from(self.agent_pid))),
            (RELAY_KEY, Value::from(self.relay.as_str())),
            (OWNER_KEY, Value::from(self.owner.as_str())),
        ]);
        if let Some(status) = self.status.as_str() {
            v.set("status", Value::from(status));
            v.set("statusUpdatedAt", Value::from(now_ms));
        }
        if let Some(spool) = &self.spool {
            v.set(SPOOL_KEY, Value::from(spool.as_str()));
        }
        v.to_json()
    }
}

/// What a reader needs from any record, ours or a Claude session's.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub pid: i32,
    pub session_id: Option<String>,
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub socket: Option<String>,
    pub status: Option<String>,
    pub proc_start: Option<String>,
    pub started_at_ms: Option<i64>,
    pub features: Vec<String>,
    pub ours: bool,
    pub class: Option<Mode>,
    /// The relay's send socket, for a record of ours.
    pub relay: Option<String>,
    pub owner: Option<String>,
    /// For a record of ours: the agent, its process, its mail file.
    pub agent: Option<Agent>,
    pub agent_pid: Option<i32>,
    pub spool: Option<String>,
}

impl Entry {
    /// The agent the session runs, as a recipient string names it.
    pub fn kind(&self) -> &'static str {
        self.agent.map_or("claude", Agent::as_str)
    }

    pub fn supports(&self, feature: &str) -> bool {
        self.features.iter().any(|f| f == feature)
    }

    /// Live by the same rule Claude Code applies.
    pub fn is_live(&self) -> bool {
        proc::alive(self.pid) && self.proc_start.as_ref().is_none_or(|s| proc::lstart(self.pid).as_ref() == Some(s))
    }
}

pub fn record_path(dir: &Path, pid: i32) -> PathBuf {
    dir.join(format!("{pid}.json"))
}

/// Write atomically: readers list the directory at any moment and a
/// torn record would read as garbage.  The temporary name starts with
/// a dot, so it never matches the `<digits>.json` readers look for.
pub fn write(dir: &Path, rec: &Record, now_ms: i64) -> io::Result<()> {
    let tmp = dir.join(format!(".marspot-{}.json.tmp", rec.pid));
    fs::write(&tmp, rec.to_json(now_ms))?;
    fs::rename(&tmp, record_path(dir, rec.pid))
}

/// Undo a rename an earlier marspot made in a Claude session's own
/// record (`nameSource` `peer`, the old name first among the former
/// names): marspot keeps pane names in a table of its own now, and a
/// session should be called what it calls itself.  Returns the pids put
/// back, with the names they got.
pub fn restore_foreign(dir: &Path, now_ms: i64) -> Vec<(i32, String)> {
    let mut out = Vec::new();
    for e in read_all(dir).into_iter().filter(|e| !e.ours) {
        let path = record_path(dir, e.pid);
        let Some(mut v) = fs::read_to_string(&path).ok().and_then(|t| json::parse(&t).ok()) else { continue };
        if v.str_at("nameSource") != Some("peer") {
            continue;
        }
        let former = v.get("formerNames").map_or(&[][..], Value::arr).to_vec();
        let Some(old) = former.first().and_then(|f| f.str_at("name")).map(str::to_string) else { continue };
        // `<dir>-<2 hex>` is the name Claude Code derives; anything
        // else was given, by `/rename` or at launch
        let derived = old.rsplit_once('-').is_some_and(|(_, t)| t.len() == 2 && t.bytes().all(|b| b.is_ascii_hexdigit()));
        v.set("name", Value::from(old.as_str()));
        v.set("nameSource", Value::from(if derived { "derived" } else { "user" }));
        v.set("nameSince", Value::from(now_ms));
        v.set("formerNames", Value::Arr(former[1..].to_vec()));
        let tmp = dir.join(format!(".marspot-restore-{}.json.tmp", e.pid));
        if fs::write(&tmp, v.to_json()).and_then(|()| fs::rename(&tmp, &path)).is_ok() {
            out.push((e.pid, old));
        }
    }
    out
}

/// Remove the record for `pid` if it is ours.  A record that a Claude
/// session wrote for a recycled pid is left alone.
pub fn remove_ours(dir: &Path, pid: i32) -> io::Result<bool> {
    let path = record_path(dir, pid);
    match read_entry(&path, pid) {
        Some(e) if e.ours => match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        },
        _ => Ok(false),
    }
}

pub fn read_all(dir: &Path) -> Vec<Entry> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for ent in rd.flatten() {
        let name = ent.file_name();
        let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".json")) else { continue };
        // canonical decimal only, as Claude Code requires
        let Ok(pid) = stem.parse::<i32>() else { continue };
        if pid.to_string() != stem {
            continue;
        }
        out.extend(read_entry(&ent.path(), pid));
    }
    out.sort_by_key(|e| e.pid);
    out
}

pub fn list_live(dir: &Path) -> Vec<Entry> {
    read_all(dir).into_iter().filter(Entry::is_live).collect()
}

/// Remove our records whose process is gone or recycled.  Returns how
/// many were removed.
pub fn sweep_ours(dir: &Path) -> usize {
    read_all(dir)
        .into_iter()
        .filter(|e| e.ours && !e.is_live())
        .filter(|e| remove_ours(dir, e.pid).unwrap_or(false))
        .count()
}

/// Remove every record `owner` wrote, live or not: what a restarting
/// marspot does before binding again, since its old inbox sockets died
/// with the process that listened on them.  Returns the removed
/// records, so the caller can delete their socket files too.
pub fn remove_owned_by(dir: &Path, owner: &str) -> Vec<Entry> {
    read_all(dir)
        .into_iter()
        .filter(|e| e.ours && e.owner.as_deref() == Some(owner))
        .filter(|e| remove_ours(dir, e.pid).unwrap_or(false))
        .collect()
}

/// The registry directories this user has: `sessions/` under
/// `$CLAUDE_CONFIG_DIR`, `~/.claude` and every `~/.claude-profile-*`,
/// resolved through symlinks so a shared directory appears once.
/// Only directories that exist; none are created.
pub fn default_dirs() -> Vec<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let mut cands: Vec<PathBuf> = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).into_iter().collect();
    cands.push(home.join(".claude"));
    if let Ok(rd) = fs::read_dir(&home) {
        let mut profiles: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with(".claude-profile-")))
            .map(|e| e.path())
            .collect();
        profiles.sort();
        cands.extend(profiles);
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in cands.into_iter().filter_map(|c| c.join("sessions").canonicalize().ok()) {
        if dir.is_dir() && !out.contains(&dir) {
            out.push(dir);
        }
    }
    out
}

fn read_entry(path: &Path, pid: i32) -> Option<Entry> {
    let text = fs::read_to_string(path).ok()?;
    let v = json::parse(&text).ok()?;
    let ours = v.str_at("entrypoint") == Some(MARKER);
    let s = |k: &str| v.str_at(k).map(str::to_string);
    let strs = |k: &str, field: Option<&str>| -> Vec<String> {
        v.get(k)
            .map_or(&[][..], Value::arr)
            .iter()
            .filter_map(|x| match field {
                Some(f) => x.str_at(f),
                None => x.str(),
            })
            .map(str::to_string)
            .collect()
    };
    Some(Entry {
        pid,
        session_id: s("sessionId"),
        name: s("name"),
        cwd: s("cwd"),
        socket: s("messagingSocketPath"),
        status: s("status"),
        proc_start: s("procStart"),
        started_at_ms: v.get("startedAt").and_then(Value::i64),
        features: strs("peerFeatures", None),
        ours,
        class: v.str_at(CLASS_KEY).and_then(Mode::parse),
        relay: s(RELAY_KEY),
        owner: s(OWNER_KEY),
        // a record written before the field existed is a Codex one
        agent: ours.then(|| match v.str_at(AGENT_KEY) {
            Some("gemini") => Agent::Gemini,
            Some("kimi") => Agent::Kimi,
            _ => Agent::Codex,
        }),
        agent_pid: v.get(AGENT_PID_KEY).and_then(Value::i64).and_then(|p| i32::try_from(p).ok()),
        spool: s(SPOOL_KEY),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("marspot-peer-reg-{tag}-{}", crate::rand::hex(8)));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn record(pid: i32) -> Record {
        Record {
            pid,
            agent: Agent::Codex,
            agent_pid: pid,
            relay: "/tmp/r.sock".into(),
            session_id: "01a16103-3f4c-7883-b4ee-2a471b88dda9".into(),
            cwd: "/tmp/x".into(),
            started_at_ms: 1,
            proc_start: proc::lstart(pid).unwrap_or_default(),
            version: "0.163.0".into(),
            name: "x".into(),
            name_since_ms: 1,
            socket: format!("/tmp/cc-socks/{pid}-0a1b2c3d.sock"),
            status: Status::Idle,
            class: Mode::Bypass,
            owner: "/tmp/owner-a".into(),
            spool: None,
        }
    }

    #[test]
    fn a_written_record_lists_as_live_and_ours() {
        let dir = tmpdir("live");
        let me = std::process::id() as i32;
        write(&dir, &record(me), 2).unwrap();
        let live = list_live(&dir);
        assert_eq!(live.len(), 1);
        let e = &live[0];
        assert!(e.ours);
        assert_eq!(e.class, Some(Mode::Bypass));
        assert_eq!(e.relay.as_deref(), Some("/tmp/r.sock"));
        assert_eq!(e.socket.as_deref(), Some(format!("/tmp/cc-socks/{me}-0a1b2c3d.sock").as_str()));
        assert!(remove_ours(&dir, me).unwrap());
        assert!(read_all(&dir).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A Gemini record says what marspot knows and no more: no status,
    /// no idle notices, and where its mail waits.
    #[test]
    fn a_gemini_record_claims_no_status_and_no_idle_notice() {
        let dir = tmpdir("gemini");
        let me = std::process::id() as i32;
        let mut r = record(me);
        write(&dir, &r, 2).unwrap();
        let e = &list_live(&dir)[0];
        assert_eq!((e.agent, e.agent_pid, e.status.as_deref()), (Some(Agent::Codex), Some(me), Some("idle")));
        assert!(e.supports("notify_idle"));
        r.agent = Agent::Gemini;
        r.agent_pid = 4242;
        r.status = Status::Unknown;
        r.spool = Some("/tmp/spool/s1.jsonl".into());
        write(&dir, &r, 2).unwrap();
        let v = json::parse(&fs::read_to_string(record_path(&dir, me)).unwrap()).unwrap();
        assert!(v.get("status").is_none() && v.get("statusUpdatedAt").is_none());
        assert!(v.get("peerFeatures").unwrap().arr().is_empty());
        let e = &list_live(&dir)[0];
        assert!(e.ours && !e.supports("notify_idle"));
        assert_eq!((e.agent, e.agent_pid, e.status.as_deref()), (Some(Agent::Gemini), Some(4242), None));
        assert_eq!(e.spool.as_deref(), Some("/tmp/spool/s1.jsonl"));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A Kimi record reads back as one, offers the idle notice and
    /// carries a status; a record with no agent field stays a Codex
    /// one, as records written before the field existed are.
    #[test]
    fn a_kimi_record_reads_back_as_kimi_and_an_unmarked_one_as_codex() {
        let dir = tmpdir("kimi");
        let me = std::process::id() as i32;
        let mut r = record(me);
        r.agent = Agent::Kimi;
        r.agent_pid = 4242;
        r.status = Status::Busy;
        r.spool = Some("/tmp/spool/session_1.jsonl".into());
        write(&dir, &r, 2).unwrap();
        let e = &list_live(&dir)[0];
        assert_eq!((e.agent, e.agent_pid, e.status.as_deref()), (Some(Agent::Kimi), Some(4242), Some("busy")));
        assert!(e.supports("notify_idle"));
        let text = fs::read_to_string(record_path(&dir, me)).unwrap().replace(r#""marspotAgent":"kimi","#, "");
        fs::write(record_path(&dir, me), text).unwrap();
        assert_eq!(list_live(&dir)[0].agent, Some(Agent::Codex));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_stale_start_time_is_not_live_and_gets_swept() {
        let dir = tmpdir("stale");
        let me = std::process::id() as i32;
        let mut r = record(me);
        r.proc_start = "Thu Jan  1 00:00:00 1970".into();
        write(&dir, &r, 2).unwrap();
        assert!(list_live(&dir).is_empty());
        assert_eq!(sweep_ours(&dir), 1);
        assert!(read_all(&dir).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn removal_by_owner_spares_other_owners() {
        let dir = tmpdir("owner");
        let me = std::process::id() as i32;
        let parent = std::os::unix::process::parent_id() as i32;
        write(&dir, &record(me), 2).unwrap();
        let mut other = record(parent);
        other.owner = "/tmp/owner-b".into();
        write(&dir, &other, 2).unwrap();
        let gone = remove_owned_by(&dir, "/tmp/owner-a");
        assert_eq!(gone.iter().map(|e| e.pid).collect::<Vec<_>>(), vec![me]);
        let left = read_all(&dir);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].owner.as_deref(), Some("/tmp/owner-b"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn records_that_are_not_ours_are_never_removed() {
        let dir = tmpdir("theirs");
        let theirs = r#"{"pid":1,"sessionId":"s","procStart":"x","entrypoint":"cli","messagingSocketPath":"/tmp/cc-socks/1.sock"}"#;
        fs::write(record_path(&dir, 1), theirs).unwrap();
        fs::write(dir.join("01.json"), theirs).unwrap();
        fs::write(dir.join("1.5ab.key"), "k").unwrap();
        assert!(!remove_ours(&dir, 1).unwrap());
        assert_eq!(sweep_ours(&dir), 0);
        let all = read_all(&dir);
        assert_eq!(all.len(), 1, "only the canonical name counts");
        assert!(!all[0].ours);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A rename an earlier marspot made is undone in place: the old
    /// name back, every other field as Claude Code wrote it.
    #[test]
    fn an_earlier_rename_is_undone() {
        let dir = tmpdir("restore");
        let renamed = r#"{"pid":7,"sessionId":"s1","procStart":"x","entrypoint":"cli","name":"spg","nameSource":"peer","nameSince":5,"formerNames":[{"name":"spg-6a","until":5,"sessionId":"s1"}],"status":"busy"}"#;
        fs::write(record_path(&dir, 7), renamed).unwrap();
        let given = r#"{"pid":8,"entrypoint":"cli","name":"x","nameSource":"peer","formerNames":[{"name":"my-review","until":1}]}"#;
        fs::write(record_path(&dir, 8), given).unwrap();
        let own = r#"{"pid":9,"entrypoint":"cli","name":"luna-1a","nameSource":"derived"}"#;
        fs::write(record_path(&dir, 9), own).unwrap();
        let mut back = restore_foreign(&dir, 100);
        back.sort();
        assert_eq!(back, vec![(7, "spg-6a".to_string()), (8, "my-review".to_string())]);
        let v = json::parse(&fs::read_to_string(record_path(&dir, 7)).unwrap()).unwrap();
        assert_eq!((v.str_at("name"), v.str_at("nameSource"), v.str_at("status")), (Some("spg-6a"), Some("derived"), Some("busy")));
        assert!(v.get("formerNames").unwrap().arr().is_empty());
        let v = json::parse(&fs::read_to_string(record_path(&dir, 8)).unwrap()).unwrap();
        assert_eq!(v.str_at("nameSource"), Some("user"));
        assert_eq!(fs::read_to_string(record_path(&dir, 9)).unwrap(), own, "a session's own name is not touched");
        assert!(restore_foreign(&dir, 200).is_empty(), "and once is enough");
        fs::remove_dir_all(&dir).unwrap();
    }
}

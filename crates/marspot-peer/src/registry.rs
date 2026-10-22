//! The Claude Code session registry: `<config dir>/sessions/<pid>.json`,
//! one record per live session, read by every session to list its
//! peers and find their inbox sockets.
//!
//! marspot writes records here for Codex sessions, keyed by the Codex
//! process's pid, so Claude sessions list them and message them like
//! any peer.  Those records carry `entrypoint = MARKER`; that marker is
//! the only thing that makes a record ours, and only ours are ever
//! rewritten or removed.
//!
//! A reader counts a record as live while its pid exists and the pid's
//! start time still equals `procStart` (see `proc`).  Claude Code
//! deletes records whose process is gone, ours included.
//!
//! Names are the one field marspot also writes into a Claude session's
//! record (`rename_foreign`): every session is addressed by the name of
//! its marspot pane.  Claude Code reads a name whose `nameSource` is
//! `peer` as one somebody else gave the session, and its own updates to
//! the record merge into what is on disk, so the name stays until the
//! session renames itself.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::envelope::Mode;
use crate::json::{self, Value, obj};
use crate::proc;

pub const MARKER: &str = "marspot-codex";
/// Our own field: the Codex session's permission class, which the
/// sender CLI reads back to fill `from-mode`.  Claude Code ignores it.
const CLASS_KEY: &str = "marspotPermissionClass";
/// Our own fields: the Codex process the record stands for (the
/// record's `pid` is its relay), and the relay's send socket.
const CODEX_PID_KEY: &str = "marspotCodexPid";
const RELAY_KEY: &str = "marspotRelay";
/// Our own field: which marspot wrote the record (its state root).  A
/// restarting marspot clears its own leftovers and nobody else's — a
/// sandbox instance and the installed app share this directory.
const OWNER_KEY: &str = "marspotOwner";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Idle,
    Busy,
    Waiting,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Busy => "busy",
            Status::Waiting => "waiting",
        }
    }
}

/// How many earlier names a record keeps.  They still address the
/// session, so a name a sender stored before a pane was renumbered
/// keeps working for a while; the oldest go first.
pub const MAX_FORMER_NAMES: usize = 5;

/// What this side of the protocol supports beyond plain messages.
pub const FEATURES: &[&str] = &["notify_idle"];

#[derive(Clone, Debug, PartialEq)]
pub struct FormerName {
    pub name: String,
    pub until_ms: i64,
    pub session_id: String,
}

impl FormerName {
    fn to_value(&self) -> Value {
        obj([
            ("name", Value::from(self.name.as_str())),
            ("until", Value::from(self.until_ms)),
            ("sessionId", Value::from(self.session_id.as_str())),
        ])
    }
}

/// `list` with `old` pushed to the front: deduplicated, never holding
/// `current`, at most `MAX_FORMER_NAMES` long.
pub fn push_former(list: &mut Vec<FormerName>, old: FormerName, current: &str) {
    list.retain(|f| f.name != old.name && f.name != current);
    if old.name != current && !old.name.is_empty() {
        list.insert(0, old);
    }
    list.truncate(MAX_FORMER_NAMES);
}

/// A record marspot writes for one Codex session.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    /// The session's relay process (`relay`), which serves its inbox.
    pub pid: i32,
    pub codex_pid: i32,
    /// The relay's send socket.
    pub relay: String,
    /// The Codex thread id.
    pub session_id: String,
    pub cwd: String,
    pub started_at_ms: i64,
    /// `proc::lstart` of `pid`.
    pub proc_start: String,
    /// The Codex version.
    pub version: String,
    pub name: String,
    pub name_since_ms: i64,
    pub former_names: Vec<FormerName>,
    /// Absolute path of the inbox socket.
    pub socket: String,
    pub status: Status,
    pub class: Mode,
    pub owner: String,
}

impl Record {
    pub fn to_json(&self, now_ms: i64) -> String {
        obj([
            ("pid", Value::from(i64::from(self.pid))),
            ("sessionId", Value::from(self.session_id.as_str())),
            ("cwd", Value::from(self.cwd.as_str())),
            ("startedAt", Value::from(self.started_at_ms)),
            ("procStart", Value::from(self.proc_start.as_str())),
            ("version", Value::from(self.version.as_str())),
            ("peerProtocol", Value::from(1)),
            ("peerFeatures", Value::Arr(FEATURES.iter().map(|f| Value::from(*f)).collect())),
            ("kind", Value::from("interactive")),
            ("entrypoint", Value::from(MARKER)),
            ("pidDomain", Value::from("darwin")),
            ("messagingSocketPath", Value::from(self.socket.as_str())),
            ("name", Value::from(self.name.as_str())),
            ("nameSource", Value::from("peer")),
            ("nameSince", Value::from(self.name_since_ms)),
            ("formerNames", Value::Arr(self.former_names.iter().map(FormerName::to_value).collect())),
            ("status", Value::from(self.status.as_str())),
            ("updatedAt", Value::from(now_ms)),
            ("statusUpdatedAt", Value::from(now_ms)),
            (CLASS_KEY, Value::from(self.class.as_str())),
            (CODEX_PID_KEY, Value::from(i64::from(self.codex_pid))),
            (RELAY_KEY, Value::from(self.relay.as_str())),
            (OWNER_KEY, Value::from(self.owner.as_str())),
        ])
        .to_json()
    }
}

/// What a reader needs from any record, ours or a Claude session's.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub pid: i32,
    pub session_id: Option<String>,
    pub name: Option<String>,
    /// Earlier names that still address the session, newest first.
    pub former_names: Vec<String>,
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
}

impl Entry {
    /// Whether `name` addresses this session: its name, or one it had.
    pub fn answers_to(&self, name: &str) -> bool {
        self.name.as_deref() == Some(name) || self.former_names.iter().any(|n| n == name)
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

/// Give a Claude session's record the name `name`, keeping the name it
/// had among its former names.  Only the name fields are written; the
/// rest of the record is written back as it was read.  Returns the old
/// name when it changed.  A record that is ours, or that is not the
/// one for `pid`, is left alone.
pub fn rename_foreign(dir: &Path, pid: i32, name: &str, now_ms: i64) -> io::Result<Option<String>> {
    let path = record_path(dir, pid);
    let text = fs::read_to_string(&path)?;
    let Ok(mut v) = json::parse(&text) else { return Ok(None) };
    if v.str_at("entrypoint") == Some(MARKER) || v.get("pid").and_then(Value::i64) != Some(i64::from(pid)) {
        return Ok(None);
    }
    let old = v.str_at("name").unwrap_or_default().to_string();
    if old == name {
        return Ok(None);
    }
    let session = v.str_at("sessionId").unwrap_or_default().to_string();
    let mut former: Vec<FormerName> = v
        .get("formerNames")
        .map_or(&[][..], Value::arr)
        .iter()
        .filter_map(|f| {
            Some(FormerName {
                name: f.str_at("name")?.to_string(),
                until_ms: f.get("until").and_then(Value::i64).unwrap_or(0),
                session_id: f.str_at("sessionId").unwrap_or_default().to_string(),
            })
        })
        .collect();
    push_former(&mut former, FormerName { name: old.clone(), until_ms: now_ms, session_id: session }, name);
    v.set("name", Value::from(name));
    v.set("nameSource", Value::from("peer"));
    v.set("nameSince", Value::from(now_ms));
    v.set("formerNames", Value::Arr(former.iter().map(FormerName::to_value).collect()));
    let tmp = dir.join(format!(".marspot-rename-{pid}.json.tmp"));
    fs::write(&tmp, v.to_json())?;
    fs::rename(&tmp, &path)?;
    Ok(Some(old))
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
        former_names: strs("formerNames", Some("name")),
        cwd: s("cwd"),
        socket: s("messagingSocketPath"),
        status: s("status"),
        proc_start: s("procStart"),
        started_at_ms: v.get("startedAt").and_then(Value::i64),
        features: strs("peerFeatures", None),
        ours: v.str_at("entrypoint") == Some(MARKER),
        class: v.str_at(CLASS_KEY).and_then(Mode::parse),
        relay: s(RELAY_KEY),
        owner: s(OWNER_KEY),
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
            codex_pid: pid,
            relay: "/tmp/r.sock".into(),
            session_id: "01a16103-3f4c-7883-b4ee-2a471b88dda9".into(),
            cwd: "/tmp/x".into(),
            started_at_ms: 1,
            proc_start: proc::lstart(pid).unwrap_or_default(),
            version: "0.163.0".into(),
            name: "x".into(),
            name_since_ms: 1,
            former_names: Vec::new(),
            socket: format!("/tmp/cc-socks/{pid}-0a1b2c3d.sock"),
            status: Status::Idle,
            class: Mode::Bypass,
            owner: "/tmp/owner-a".into(),
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

    /// A Claude record is renamed in place: every other field, and the
    /// order of the fields, is what Claude Code wrote.
    #[test]
    fn a_foreign_rename_touches_only_the_name_fields() {
        let dir = tmpdir("rename");
        let theirs = r#"{"pid":7,"sessionId":"s1","cwd":"/w/spg","startedAt":1792674965957,"procStart":"x","peerFeatures":["notify_idle"],"entrypoint":"cli","messagingSocketPath":"/tmp/cc-socks/7.sock","name":"spg-6a","nameSource":"derived","nameSince":1,"status":"busy"}"#;
        fs::write(record_path(&dir, 7), theirs).unwrap();
        assert_eq!(rename_foreign(&dir, 7, "spg#2", 100).unwrap().as_deref(), Some("spg-6a"));
        let v = json::parse(&fs::read_to_string(record_path(&dir, 7)).unwrap()).unwrap();
        assert_eq!(v.str_at("name"), Some("spg#2"));
        assert_eq!(v.str_at("nameSource"), Some("peer"));
        assert_eq!(v.str_at("status"), Some("busy"));
        assert_eq!(v.get("startedAt").and_then(Value::i64), Some(1792674965957));
        let Value::Obj(kv) = &v else { panic!() };
        assert_eq!(kv[0].0, "pid", "fields keep their order");
        let e = read_entry(&record_path(&dir, 7), 7).unwrap();
        assert!(e.answers_to("spg#2") && e.answers_to("spg-6a"));
        assert!(e.supports("notify_idle"));
        // unchanged name: no write
        assert_eq!(rename_foreign(&dir, 7, "spg#2", 200).unwrap(), None);
        // back to the first name: it leaves the former names
        rename_foreign(&dir, 7, "spg-6a", 300).unwrap();
        let e = read_entry(&record_path(&dir, 7), 7).unwrap();
        assert_eq!(e.former_names, vec!["spg#2".to_string()]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn our_own_records_and_mismatched_pids_are_not_renamed() {
        let dir = tmpdir("rename-no");
        let me = std::process::id() as i32;
        write(&dir, &record(me), 2).unwrap();
        assert_eq!(rename_foreign(&dir, me, "y", 3).unwrap(), None);
        fs::write(record_path(&dir, 8), r#"{"pid":9,"entrypoint":"cli","name":"a"}"#).unwrap();
        assert_eq!(rename_foreign(&dir, 8, "y", 3).unwrap(), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn former_names_stay_short_and_never_hold_the_current_one() {
        let mut l = Vec::new();
        for i in 0..8 {
            push_former(&mut l, FormerName { name: format!("n{i}"), until_ms: i, session_id: String::new() }, "now");
        }
        assert_eq!(l.len(), MAX_FORMER_NAMES);
        assert_eq!(l[0].name, "n7");
        push_former(&mut l, FormerName { name: "n5".into(), until_ms: 9, session_id: String::new() }, "n6");
        assert_eq!(l.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), vec!["n5", "n7", "n4", "n3"]);
    }
}

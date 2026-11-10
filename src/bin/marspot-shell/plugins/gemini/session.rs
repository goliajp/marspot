//! Which session file a gemini process is writing, and what it says.
//!
//! gemini keeps one JSONL per session under
//! `<config>/tmp/<project>/chats/session-<minute>-<id prefix>.jsonl`.
//! `<project>` is a short name gemini assigns to the working directory
//! and records in `<config>/projects.json`; it is looked up there, never
//! derived, because the name depends on which other projects were
//! registered first.  The file's first line carries the whole session
//! id; a resumed session keeps appending to the file it was started in.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use marspot_peer::json;

use super::pane::is_session_id;
use crate::plugins::pidtree::ProcId;

/// `<config>/tmp/<project>/chats` for the project rooted at `cwd`.
fn chats_dir(config_dir: &Path, cwd: &str) -> Option<PathBuf> {
    let text = std::fs::read_to_string(config_dir.join("projects.json")).ok()?;
    let registry = json::parse(&text).ok()?;
    let slug = registry.get("projects")?.str_at(cwd)?;
    // a path component out of a file another program writes
    let plain = !slug.is_empty() && slug.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    plain.then(|| config_dir.join("tmp").join(slug).join("chats"))
}

/// The main-session files in `chats`, with when each was last written.
/// Subagent sessions live in subdirectories and are not listed.
fn session_files(chats: &Path) -> Vec<(SystemTime, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(chats) else { return Vec::new() };
    rd.flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with("session-") && n.ends_with(".jsonl")))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            meta.is_file().then(|| meta.modified().ok().map(|m| (m, e.path()))).flatten()
        })
        .collect()
}

/// How much of a session file's first line to read.  The metadata
/// record is a few hundred bytes; a line that does not end inside this
/// is not one.
const HEAD_BYTES: u64 = 16 * 1024;

/// The session id a file opens with.
pub(crate) fn head_id(path: &Path) -> Option<String> {
    let mut buf = Vec::new();
    std::fs::File::open(path).ok()?.take(HEAD_BYTES).read_to_end(&mut buf).ok()?;
    let end = buf.iter().position(|b| *b == b'\n')?;
    let head = json::parse(std::str::from_utf8(&buf[..end]).ok()?).ok()?;
    head.str_at("sessionId").filter(|id| is_session_id(id)).map(str::to_string)
}

/// The file of session `id` in the project at `cwd`.
///
/// The name carries the id's first eight characters, which narrows the
/// directory to a file or two; the first line then has to name the
/// whole id.
pub(crate) fn by_id(config_dir: &Path, cwd: &str, id: &str) -> Option<PathBuf> {
    let suffix = format!("-{}.jsonl", id.get(..8).unwrap_or(id));
    session_files(&chats_dir(config_dir, cwd)?)
        .into_iter()
        .map(|(_, p)| p)
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(&suffix)))
        .find(|p| head_id(p).as_deref() == Some(id))
}

/// How far a file's last write may trail the start of the process that
/// wrote it: the two clocks are not read at the same instant.
const START_SLACK: Duration = Duration::from_secs(2);

/// How many of the newest files are opened before giving up.
const NEWEST_TRIED: usize = 8;

/// The session written most recently since `started`, with its id.
fn newest_since(config_dir: &Path, cwd: &str, started: SystemTime) -> Option<(PathBuf, String)> {
    let mut files = session_files(&chats_dir(config_dir, cwd)?);
    files.retain(|(written, _)| *written + START_SLACK >= started);
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    files.into_iter().take(NEWEST_TRIED).find_map(|(_, p)| head_id(&p).map(|id| (p, id)))
}

/// How much of a session's end is searched for the model.  Every reply
/// is one record and names it; a reply carrying more tool output than
/// this leaves the badge with what it had.
const TAIL_BYTES: u64 = 256 * 1024;

/// The model of the last reply in the file's tail.
fn model_in_tail(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES))).ok()?;
    let mut buf = Vec::new();
    f.take(TAIL_BYTES).read_to_end(&mut buf).ok()?;
    let tail = String::from_utf8_lossy(&buf);
    tail.lines().rev().find_map(model_of)
}

/// The model a reply record names.  Parsed, not matched as text: a
/// tool call's arguments are JSON inside the same line and may carry a
/// `model` key of their own.
fn model_of(line: &str) -> Option<String> {
    if !line.contains(r#""type":"gemini""#) {
        return None;
    }
    let record = json::parse(line).ok()?;
    if record.str_at("type") != Some("gemini") {
        return None;
    }
    record.str_at("model").filter(|m| !m.is_empty()).map(str::to_string)
}

/// A gemini process asking for its session.
pub(super) struct Asker<'a> {
    pub proc: ProcId,
    pub started: SystemTime,
    pub config_dir: &'a Path,
    pub cwd: &'a str,
    /// The session its argv names, if it names one.
    pub session: Option<&'a str>,
    /// No other gemini without a session on its argv shares this home
    /// and directory.  When one does, the newest file could be either
    /// one's, so neither is bound.
    pub lone: bool,
}

/// What a bound session says.
#[derive(Debug, PartialEq)]
pub(super) struct Facts {
    pub session: String,
    pub model: Option<String>,
}

struct Bound {
    path: PathBuf,
    id: String,
    /// The write the model was last read at.
    read_at: Option<SystemTime>,
    model: Option<String>,
}

/// Which file each gemini process writes, and the model it last named.
///
/// Finding the file lists a directory, so it happens on a slow cadence;
/// between searches a bound process costs one `stat`, and the tail is
/// read again only when the file was written.  Both maps are keyed by
/// process and cut back to the live ones every tick.
#[derive(Default)]
pub(super) struct SessionIndex {
    bound: HashMap<ProcId, Bound>,
    searched_at: HashMap<ProcId, Instant>,
}

impl SessionIndex {
    /// How often a process's session is looked for again.  Not only
    /// while it has none: `/clear` starts another file under the same
    /// process, and the binding has to follow it.
    const SEARCH_EVERY: Duration = Duration::from_secs(20);

    pub(super) fn retain(&mut self, live: &[ProcId]) {
        self.bound.retain(|id, _| live.contains(id));
        self.searched_at.retain(|id, _| live.contains(id));
    }

    /// The session `who` is writing, or `None` when it cannot be told —
    /// in which case nothing is assumed about it.
    pub(super) fn facts(&mut self, who: &Asker) -> Option<Facts> {
        let due = self.searched_at.get(&who.proc).is_none_or(|t| t.elapsed() >= Self::SEARCH_EVERY);
        if due {
            self.searched_at.insert(who.proc, Instant::now());
            self.search(who);
        }
        let bound = self.bound.get_mut(&who.proc)?;
        let Ok(written) = std::fs::metadata(&bound.path).and_then(|m| m.modified()) else {
            self.bound.remove(&who.proc);
            return None;
        };
        if bound.read_at != Some(written) {
            bound.read_at = Some(written);
            if let Some(model) = model_in_tail(&bound.path) {
                bound.model = Some(model);
            }
        }
        Some(Facts { session: bound.id.clone(), model: bound.model.clone() })
    }

    fn search(&mut self, who: &Asker) {
        let found = match who.session {
            Some(id) => by_id(who.config_dir, who.cwd, id).map(|p| (p, id.to_string())),
            None if who.lone => newest_since(who.config_dir, who.cwd, who.started),
            None => None,
        };
        match found {
            Some((path, id)) => {
                if self.bound.get(&who.proc).is_none_or(|b| b.path != path) {
                    self.bound.insert(who.proc, Bound { path, id, read_at: None, model: None });
                }
            }
            None => {
                self.bound.remove(&who.proc);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod fixture {
    use std::path::{Path, PathBuf};

    /// The opening record, as a v0.67 build wrote it for a session
    /// started with `--session-id`.
    pub(crate) fn head(id: &str) -> String {
        format!(
            r#"{{"sessionId":"{id}","projectHash":"1a2fd07c4d49eacee98ce19e4fb41321035bb110963bb85856985c06736f09a4","startTime":"2026-11-10T06:10:20.221Z","lastUpdated":"2026-11-10T06:10:20.221Z","kind":"main"}}"#
        ) + "\n"
    }

    /// The context gemini records as a user message before the first
    /// real one.
    pub(crate) fn context() -> String {
        r#"{"id":"c0","timestamp":"2026-11-10T06:10:20.221Z","type":"user","content":[{"text":"<session_context>\nThis is the Gemini CLI.\n</session_context>"}]}"#.to_string()
            + "\n"
            + &touched()
    }

    /// The `lastUpdated` record gemini appends after every message.
    pub(crate) fn touched() -> String {
        r#"{"$set":{"lastUpdated":"2026-11-10T06:10:20.222Z"}}"#.to_string() + "\n"
    }

    pub(crate) fn user(id: &str, text: &str) -> String {
        format!(r#"{{"id":"{id}","timestamp":"2026-11-10T06:11:00.000Z","type":"user","content":[{{"text":"{text}"}}]}}"#)
            + "\n"
            + &touched()
    }

    pub(crate) fn reply(id: &str, text: &str, model: &str) -> String {
        format!(
            r#"{{"id":"{id}","timestamp":"2026-11-10T06:11:02.000Z","type":"gemini","content":"{text}","thoughts":[],"tokens":{{"input":1,"output":1,"cached":0,"thoughts":0,"tool":0,"total":2}},"model":"{model}"}}"#
        ) + "\n"
            + &touched()
    }

    /// A config directory whose registry maps `cwd` to `slug`; returns
    /// the project's chats directory.
    pub(crate) fn project(config_dir: &Path, cwd: &str, slug: &str) -> PathBuf {
        let chats = config_dir.join("tmp").join(slug).join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(
            config_dir.join("projects.json"),
            format!("{{\n  \"projects\": {{\n    \"{cwd}\": \"{slug}\"\n  }}\n}}"),
        )
        .unwrap();
        chats
    }

    /// The name gemini gives a session's file.
    pub(crate) fn file_name(id: &str) -> String {
        format!("session-2026-11-10T06-10-{}.jsonl", &id[..8])
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;
    use crate::plugins::handoff::history::tmp;

    const A: &str = "11111111-2222-4333-8444-555555555555";
    const B: &str = "99999999-2222-4333-8444-555555555555";

    fn asker<'a>(config_dir: &'a Path, session: Option<&'a str>, lone: bool) -> Asker<'a> {
        Asker {
            proc: ProcId { pid: 7, start_us: 7 },
            started: SystemTime::now() - Duration::from_secs(1),
            config_dir,
            cwd: "/w/a",
            session,
            lone,
        }
    }

    #[test]
    fn the_project_directory_is_the_one_the_registry_names() {
        let cfg = tmp("gemini-registry");
        let chats = project(&cfg, "/w/a", "a-1");
        assert_eq!(chats_dir(&cfg, "/w/a"), Some(chats));
        assert_eq!(chats_dir(&cfg, "/w/b"), None, "a directory gemini never ran in");
        std::fs::write(cfg.join("projects.json"), r#"{"projects":{"/w/a":"../../etc"}}"#).unwrap();
        assert_eq!(chats_dir(&cfg, "/w/a"), None, "a name that is not one gemini assigns");
        std::fs::write(cfg.join("projects.json"), "not json").unwrap();
        assert_eq!(chats_dir(&cfg, "/w/a"), None);
    }

    #[test]
    fn a_session_named_on_argv_is_found_by_its_whole_id() {
        let cfg = tmp("gemini-by-id");
        let chats = project(&cfg, "/w/a", "a");
        let file = chats.join(file_name(A));
        std::fs::write(&file, head(A) + &context()).unwrap();
        // same eight characters, another session
        let twin = "11111111-0000-4333-8444-555555555555";
        std::fs::write(chats.join("session-2026-11-10T06-12-11111111.jsonl"), head(twin)).unwrap();
        assert_eq!(head_id(&file).as_deref(), Some(A));
        assert_eq!(by_id(&cfg, "/w/a", A), Some(file));
        assert_eq!(by_id(&cfg, "/w/a", B), None);
        assert_eq!(by_id(&cfg, "/w/elsewhere", A), None, "sessions belong to one project");
    }

    #[test]
    fn a_process_without_an_id_gets_the_file_written_since_it_started() {
        let cfg = tmp("gemini-newest");
        let chats = project(&cfg, "/w/a", "a");
        let old = chats.join(file_name(B));
        std::fs::write(&old, head(B) + &reply("g0", "old", "gemini-3-pro")).unwrap();
        let before = SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options().write(true).open(&old).unwrap().set_modified(before).unwrap();
        let mut idx = SessionIndex::default();
        assert_eq!(idx.facts(&asker(&cfg, None, true)), None, "a file older than the process is not its own");

        std::fs::write(chats.join(file_name(A)), head(A) + &context()).unwrap();
        // the search is on a cadence; a process seen for the first time searches at once
        let mut idx = SessionIndex::default();
        assert_eq!(
            idx.facts(&asker(&cfg, None, true)),
            Some(Facts { session: A.into(), model: None }),
            "bound before any reply names a model"
        );
        let mut idx = SessionIndex::default();
        assert_eq!(idx.facts(&asker(&cfg, None, false)), None, "two candidates for one file: neither is bound");
    }

    #[test]
    fn the_model_is_the_last_reply_s() {
        let cfg = tmp("gemini-model");
        let chats = project(&cfg, "/w/a", "a");
        let file = chats.join(file_name(A));
        let tool_args = r#"{"id":"g2","timestamp":"t","type":"user","content":[{"functionResponse":{"name":"x","response":{"type":"gemini","model":"WRONG"}}}]}"#;
        let body = head(A) + &context() + &user("u1", "hi") + &reply("g1", "hello", "gemini-3-flash") + tool_args + "\n";
        std::fs::write(&file, &body).unwrap();
        let mut idx = SessionIndex::default();
        let who = asker(&cfg, Some(A), true);
        assert_eq!(idx.facts(&who).unwrap().model.as_deref(), Some("gemini-3-flash"));

        // a later reply under another model, and the file's time moves
        std::fs::write(&file, body + &reply("g3", "again", "gemini-3-pro")).unwrap();
        let later = SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(later).unwrap();
        assert_eq!(idx.facts(&who).unwrap().model.as_deref(), Some("gemini-3-pro"));
    }

    #[test]
    fn an_unreadable_session_binds_nothing() {
        let cfg = tmp("gemini-unreadable");
        let chats = project(&cfg, "/w/a", "a");
        let mut idx = SessionIndex::default();
        std::fs::write(chats.join(file_name(A)), "not a session\n").unwrap();
        assert_eq!(idx.facts(&asker(&cfg, None, true)), None);
        let mut idx = SessionIndex::default();
        assert_eq!(idx.facts(&asker(&cfg, Some(A), true)), None);
        std::fs::write(chats.join(file_name(A)), &head(A)[..40]).unwrap();
        let mut idx = SessionIndex::default();
        assert_eq!(idx.facts(&asker(&cfg, None, true)), None, "a first line still being written");
        let mut idx = SessionIndex::default();
        assert_eq!(idx.facts(&asker(&tmp("gemini-empty-home"), None, true)), None, "no registry at all");
    }

    #[test]
    fn the_index_keeps_only_live_processes() {
        let cfg = tmp("gemini-bounded");
        let chats = project(&cfg, "/w/a", "a");
        std::fs::write(chats.join(file_name(A)), head(A)).unwrap();
        let mut idx = SessionIndex::default();
        assert!(idx.facts(&asker(&cfg, Some(A), true)).is_some());
        idx.retain(&[]);
        assert!(idx.bound.is_empty() && idx.searched_at.is_empty());
    }
}

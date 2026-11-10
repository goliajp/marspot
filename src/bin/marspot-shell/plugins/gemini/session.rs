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

use super::model::is_model;
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

/// How much of a session file is read in one go.  The file is read
/// from its start, because a `/model set` stays in force however long
/// ago it was typed, and in pieces, because this runs on the main
/// thread and a session can be hundreds of megabytes.
const READ_BYTES: u64 = 1024 * 1024;

/// What a session file records about the model, in the order it was
/// written.
#[derive(Debug, Default, Clone, PartialEq)]
pub(super) struct Said {
    /// The model the last `/model set` named, as typed.
    pub chosen: Option<String>,
    /// The model a quota fallback moved the session to since then.
    pub fallback: Option<String>,
    /// The model the last reply ran on.  None before any, and after a
    /// change no reply has been made under yet.
    pub reply: Option<String>,
    /// The last change named a model this code will not repeat, so
    /// what the session is set to is not known.
    pub unreadable: bool,
}

/// What gemini's interface records when the model is changed by hand
/// and when a quota fallback is accepted.  The `/model` dialog records
/// nothing.
const SET: &str = "Model set to ";
const SET_SAVED: &str = " (persisted)";
const FELL_BACK: &str = "Switched to fallback model ";

impl Said {
    /// Take in one record.  Parsed, not matched as text: a tool call's
    /// arguments are JSON inside the same line and may carry a `model`
    /// key of their own.
    fn note(&mut self, line: &str) {
        if !line.contains(r#""type":"gemini""#) && !line.contains(r#""type":"info""#) {
            return;
        }
        let Ok(record) = json::parse(line) else { return };
        match record.str_at("type") {
            // a record gemini wrote for bookkeeping names no model
            Some("gemini") => {
                if let Some(model) = record.str_at("model") {
                    self.reply = is_model(model).then(|| model.to_string());
                }
            }
            Some("info") => {
                let text = record.str_at("content").unwrap_or_default();
                if let Some(name) = text.strip_prefix(SET) {
                    let name = name.strip_suffix(SET_SAVED).unwrap_or(name);
                    self.chosen = is_model(name).then(|| name.to_string());
                    self.unreadable = self.chosen.is_none();
                    (self.fallback, self.reply) = (None, None);
                } else if let Some(name) = text.strip_prefix(FELL_BACK) {
                    self.fallback = is_model(name).then(|| name.to_string());
                    self.unreadable = self.fallback.is_none();
                    self.reply = None;
                }
            }
            _ => {}
        }
    }
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
    /// What the file records about the model, once all of it has been
    /// read; until then nothing is said about the model at all.
    pub said: Option<Said>,
}

struct Bound {
    path: PathBuf,
    id: String,
    /// How far into the file `said` reaches.
    at: u64,
    /// `at` is inside a record too large to read in one go.
    mid_record: bool,
    said: Said,
    /// The end of the file has been reached at least once.
    whole: bool,
}

impl Bound {
    fn new(path: PathBuf, id: String, said: Said) -> Self {
        Self { path, id, at: 0, mid_record: false, said, whole: false }
    }

    /// Read on from where the last read stopped.  `None` when the file
    /// cannot be read.
    fn read_on(&mut self) -> Option<()> {
        let mut f = std::fs::File::open(&self.path).ok()?;
        let len = f.metadata().ok()?.len();
        if len < self.at {
            // not the file that was being followed: start over
            *self = Self::new(std::mem::take(&mut self.path), std::mem::take(&mut self.id), Said::default());
        }
        if len == self.at {
            self.whole = true;
            return Some(());
        }
        f.seek(SeekFrom::Start(self.at)).ok()?;
        let mut buf = Vec::new();
        f.take(READ_BYTES).read_to_end(&mut buf).ok()?;
        let reached_end = (buf.len() as u64) < READ_BYTES;
        match buf.iter().rposition(|b| *b == b'\n') {
            Some(end) => {
                let text = String::from_utf8_lossy(&buf[..end]);
                // the first piece is the rest of a record already given up on
                let skip = usize::from(std::mem::take(&mut self.mid_record));
                text.split('\n').skip(skip).for_each(|line| self.said.note(line));
                self.at += end as u64 + 1;
            }
            // A record larger than one read.  Only a reply or a tool
            // result is ever that large; if it is a reply, the model it
            // names goes unread, so the last one known is dropped.
            None if !reached_end => {
                self.at += READ_BYTES;
                self.mid_record = true;
                self.said.reply = None;
            }
            // the last record is still being written
            None => {}
        }
        self.whole |= reached_end;
        Some(())
    }
}

/// Which file each gemini process writes, and what it says of the model.
///
/// Finding the file lists a directory, so it happens on a slow cadence;
/// between searches a bound process costs one `stat`, and only what was
/// appended is read.  Both maps are keyed by process and cut back to
/// the live ones every tick.
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
        let Ok(len) = std::fs::metadata(&bound.path).map(|m| m.len()) else {
            self.bound.remove(&who.proc);
            return None;
        };
        if (len != bound.at || !bound.whole) && bound.read_on().is_none() {
            self.bound.remove(&who.proc);
            return None;
        }
        Some(Facts { session: bound.id.clone(), said: bound.whole.then(|| bound.said.clone()) })
    }

    fn search(&mut self, who: &Asker) {
        let found = match who.session {
            Some(id) => by_id(who.config_dir, who.cwd, id).map(|p| (p, id.to_string())),
            None if who.lone => newest_since(who.config_dir, who.cwd, who.started),
            None => None,
        };
        match found {
            Some((path, id)) => {
                let before = self.bound.get(&who.proc);
                if before.is_none_or(|b| b.path != path) {
                    // `/clear` starts another file and keeps the model
                    // the process was set to; no reply is under it yet
                    let said = before.map(|b| Said { reply: None, ..b.said.clone() }).unwrap_or_default();
                    self.bound.insert(who.proc, Bound::new(path, id, said));
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

    /// What the interface records beside the conversation: a line it
    /// printed, such as the answer to `/model set`.
    pub(crate) fn info(id: &str, text: &str) -> String {
        format!(r#"{{"id":"{id}","timestamp":"2026-11-10T06:11:04.000Z","type":"info","content":"{text}"}}"#) + "\n" + &touched()
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
            Some(Facts { session: A.into(), said: Some(Said::default()) }),
            "bound before any reply names a model"
        );
        let mut idx = SessionIndex::default();
        assert_eq!(idx.facts(&asker(&cfg, None, false)), None, "two candidates for one file: neither is bound");
    }

    /// The index over one session file that tests append to.
    struct Followed {
        cfg: PathBuf,
        file: PathBuf,
        idx: SessionIndex,
    }

    impl Followed {
        fn new(name: &str) -> Self {
            let cfg = tmp(name);
            let file = project(&cfg, "/w/a", "a").join(file_name(A));
            std::fs::write(&file, head(A) + &context()).unwrap();
            Self { cfg, file, idx: SessionIndex::default() }
        }
        fn append(&self, text: &str) {
            use std::io::Write;
            std::fs::File::options().append(true).open(&self.file).unwrap().write_all(text.as_bytes()).unwrap();
        }
        fn said(&mut self) -> Said {
            self.idx.facts(&asker(&self.cfg, Some(A), true)).unwrap().said.expect("read to its end")
        }
    }

    #[test]
    fn the_model_is_the_last_reply_s() {
        let mut s = Followed::new("gemini-model");
        assert_eq!(s.said(), Said::default(), "no reply yet");
        let tool_args = r#"{"id":"g2","timestamp":"t","type":"user","content":[{"functionResponse":{"name":"x","response":{"type":"gemini","model":"WRONG"}}}]}"#;
        s.append(&(user("u1", "hi") + &reply("g1", "hello", "gemini-3-flash-preview") + tool_args + "\n"));
        assert_eq!(s.said().reply.as_deref(), Some("gemini-3-flash-preview"));
        // the router picks another model for the next turn
        s.append(&reply("g3", "again", "gemini-3.1-pro-preview-customtools"));
        assert_eq!(s.said().reply.as_deref(), Some("gemini-3.1-pro-preview-customtools"));
        // a record gemini keeps for itself names no model and changes nothing
        s.append(r#"{"id":"g4","timestamp":"t","type":"gemini","content":[{"text":"snapshot"}]}"#);
        s.append("\n");
        assert_eq!(s.said().reply.as_deref(), Some("gemini-3.1-pro-preview-customtools"));
        // a record still being written is read once it ends
        let half = reply("g5", "more", "gemini-2.5-pro");
        s.append(&half[..40]);
        assert_eq!(s.said().reply.as_deref(), Some("gemini-3.1-pro-preview-customtools"));
        s.append(&half[40..]);
        assert_eq!(s.said().reply.as_deref(), Some("gemini-2.5-pro"));
    }

    #[test]
    fn a_model_set_by_hand_holds_until_the_next_one() {
        let mut s = Followed::new("gemini-model-set");
        s.append(&(user("u1", "hi") + &reply("g1", "hello", "gemini-3-pro-preview")));
        s.append(&info("i1", "Model set to gemini-2.5-flash"));
        assert_eq!(
            s.said(),
            Said { chosen: Some("gemini-2.5-flash".into()), ..Said::default() },
            "the reply before it ran on the model before it"
        );
        s.append(&(user("u2", "more") + &reply("g2", "ok", "gemini-2.5-flash")));
        assert_eq!(s.said().reply.as_deref(), Some("gemini-2.5-flash"));
        s.append(&info("i2", "Model set to pro (persisted)"));
        assert_eq!(s.said(), Said { chosen: Some("pro".into()), ..Said::default() });
        // other lines the interface prints, and the user quoting one
        s.append(&(info("i3", "Memory reloaded.") + &user("u3", "Model set to flash")));
        assert_eq!(s.said().chosen.as_deref(), Some("pro"));
        s.append(&info("i4", "Model set to it's complicated"));
        assert_eq!(s.said(), Said { unreadable: true, ..Said::default() }, "a name that will not be repeated");
        s.append(&info("i5", "Model set to auto"));
        assert_eq!(s.said(), Said { chosen: Some("auto".into()), ..Said::default() });
    }

    #[test]
    fn a_quota_fallback_is_recorded_apart_from_what_was_chosen() {
        let mut s = Followed::new("gemini-model-fallback");
        s.append(&info("i1", "Model set to pro"));
        s.append(&(user("u1", "hi") + &reply("g1", "hello", "gemini-3-pro-preview")));
        s.append(&info("i2", "Switched to fallback model gemini-3-flash-preview"));
        assert_eq!(
            s.said(),
            Said { chosen: Some("pro".into()), fallback: Some("gemini-3-flash-preview".into()), ..Said::default() }
        );
        s.append(&reply("g2", "retried", "gemini-3-flash-preview"));
        assert_eq!(s.said().reply.as_deref(), Some("gemini-3-flash-preview"));
        s.append(&info("i3", "Model set to pro"));
        assert_eq!(s.said(), Said { chosen: Some("pro".into()), ..Said::default() }, "choosing again ends the fallback");
    }

    /// A change typed long ago is still found, a megabyte at a time,
    /// and nothing is said about the model until the end is reached.
    #[test]
    fn a_long_session_is_read_from_its_start_in_pieces() {
        let s = Followed::new("gemini-model-long");
        s.append(&info("i1", "Model set to flash"));
        let filler = user("u", &"x".repeat(1000));
        s.append(&filler.repeat(2 * READ_BYTES as usize / filler.len() + 1));
        s.append(&reply("g1", "done", "gemini-3.8-flash"));
        let mut idx = SessionIndex::default();
        let who = asker(&s.cfg, Some(A), true);
        assert_eq!(idx.facts(&who), Some(Facts { session: A.into(), said: None }));
        assert_eq!(idx.facts(&who).unwrap().said, None);
        let whole = Said { chosen: Some("flash".into()), reply: Some("gemini-3.8-flash".into()), ..Said::default() };
        assert_eq!(idx.facts(&who).unwrap().said, Some(whole.clone()));
        assert_eq!(idx.facts(&who).unwrap().said, Some(whole), "nothing new: nothing read");
    }

    /// A record larger than one read cannot be searched for a model.
    /// If it was a reply the model known before it is out of date.
    #[test]
    fn a_record_too_large_to_read_drops_the_reply_s_model() {
        let mut s = Followed::new("gemini-model-huge");
        s.append(&info("i1", "Model set to pro"));
        s.append(&reply("g1", "hello", "gemini-3-pro-preview"));
        assert_eq!(s.said().reply.as_deref(), Some("gemini-3-pro-preview"));
        s.append(&reply("g2", &"y".repeat(READ_BYTES as usize + 4096), "gemini-2.5-pro"));
        s.said();
        assert_eq!(s.said(), Said { chosen: Some("pro".into()), ..Said::default() });
        s.append(&reply("g3", "small", "gemini-3-pro-preview"));
        assert_eq!(s.said().reply.as_deref(), Some("gemini-3-pro-preview"));
    }

    /// `/clear` starts another file under the same process, which keeps
    /// the model it was set to.
    #[test]
    fn a_new_file_under_the_same_process_keeps_the_model_it_was_set_to() {
        let s = Followed::new("gemini-model-clear");
        s.append(&(info("i1", "Model set to flash") + &reply("g1", "hello", "gemini-3.8-flash")));
        let mut idx = SessionIndex::default();
        assert!(idx.facts(&asker(&s.cfg, None, true)).unwrap().said.unwrap().reply.is_some());
        let next = s.file.with_file_name(file_name(B));
        std::fs::write(&next, head(B) + &context()).unwrap();
        let later = SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options().write(true).open(&next).unwrap().set_modified(later).unwrap();
        idx.searched_at.clear();
        assert_eq!(
            idx.facts(&asker(&s.cfg, None, true)),
            Some(Facts { session: B.into(), said: Some(Said { chosen: Some("flash".into()), ..Said::default() }) })
        );
    }

    #[test]
    fn a_file_that_shrank_is_read_again_from_its_start() {
        let mut s = Followed::new("gemini-model-shrunk");
        s.append(&(info("i1", "Model set to flash") + &reply("g1", "hello", "gemini-3.8-flash")));
        assert!(s.said().chosen.is_some());
        std::fs::write(&s.file, head(A) + &reply("g1", "hello", "gemini-2.5-pro")).unwrap();
        assert_eq!(s.said(), Said { reply: Some("gemini-2.5-pro".into()), ..Said::default() });
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

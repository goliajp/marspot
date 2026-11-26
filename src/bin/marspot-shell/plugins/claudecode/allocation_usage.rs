//! What a pane's session used, from its transcript and its background
//! agents' (`<session>/subagents/*.jsonl`): each reply's model and
//! `message.usage` tokens.
//!
//! Only bytes appended since the last read are parsed; a file is first
//! read from its end, so history before the shell started is not
//! counted twice (what was counted before is in the event log).
//!
//! A long session has thousands of agent files, nearly all finished,
//! so a scan must not touch each of them.  The directory is listed
//! again only when its own mtime moves (an agent file was added); a
//! name not seen before is a new agent and is read whole.  Only files
//! written in the last fifteen hours are followed; one left alone
//! longer is let go, and is not followed again if it is ever written
//! to after that.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use marspot_peer::json;

use super::allocation_prices::Tokens;

/// The most read from one file in one scan.
const READ_CAP: u64 = 4 << 20;

/// Reply ids remembered per file: one reply is written as several
/// records carrying its usage.
const IDS_KEPT: usize = 64;

/// A file not written for this long is no longer followed, seconds.
pub(super) const FOLLOW_S: i64 = 15 * 3600;

/// Agent files of one session remembered by name.
const NAMES_CAP: usize = 1 << 16;

/// Agent files of one session followed at once.
const FOLLOWED_CAP: usize = 4096;

/// One reply's tokens, or what a reply added since it was last seen.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Spent {
    pub at: i64,
    pub model: String,
    pub tokens: Tokens,
}

#[derive(Debug, Default)]
struct Cursor {
    offset: u64,
    /// Reply id and the tokens counted for it so far.
    ids: VecDeque<(String, Tokens)>,
}

impl Cursor {
    /// What this record adds: a reply seen before counts only its growth.
    fn add(&mut self, id: &str, t: Tokens) -> Tokens {
        if let Some((_, was)) = self.ids.iter_mut().find(|(i, _)| i == id) {
            let more = t.beyond(was);
            *was = was.max(&t);
            return more;
        }
        if self.ids.len() == IDS_KEPT {
            self.ids.pop_front();
        }
        self.ids.push_back((id.to_string(), t));
        t
    }
}

/// One session's agent directory.
#[derive(Debug, Default)]
struct Agents {
    /// The directory's mtime at the last listing; `None` before one.
    listed: Option<SystemTime>,
    /// Every name seen in it.
    names: HashSet<OsString>,
    /// The files followed.
    followed: HashMap<PathBuf, Cursor>,
}

/// Where each watched file was read up to.
#[derive(Debug, Default)]
pub(super) struct Readers {
    main: HashMap<PathBuf, Cursor>,
    agents: HashMap<PathBuf, Agents>,
    /// Directory listings and file stats done, for the tests.
    pub(super) listings: u64,
    pub(super) stats: u64,
}

fn secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

impl Readers {
    /// What the session at `transcript` and its background agents used
    /// since the last call, as of `now`.
    pub(super) fn read(&mut self, transcript: &Path, now: i64) -> Vec<Spent> {
        let mut out = Vec::new();
        let first = !self.main.contains_key(transcript);
        if first {
            self.stats += 1;
            let Ok(len) = fs::metadata(transcript).map(|m| m.len()) else { return out };
            self.main.insert(transcript.to_path_buf(), Cursor { offset: len, ids: VecDeque::new() });
        } else if let Some(cur) = self.main.get_mut(transcript) {
            self.stats += 1;
            if let Ok(m) = fs::metadata(transcript) {
                follow(transcript, m.len(), cur, &mut out);
            }
        }
        self.read_agents(transcript, !first, now, &mut out);
        out
    }

    /// `watched`: the session was read before this scan.
    fn read_agents(&mut self, transcript: &Path, watched: bool, now: i64, out: &mut Vec<Spent>) {
        let dir = transcript.with_extension("").join("subagents");
        let ag = self.agents.entry(transcript.to_path_buf()).or_default();
        self.stats += 1;
        let mtime = fs::metadata(&dir).and_then(|m| m.modified()).ok();
        if mtime.is_some() && mtime != ag.listed {
            ag.listed = mtime;
            self.listings += 1;
            for name in fs::read_dir(&dir).into_iter().flatten().filter_map(|e| e.ok()).map(|e| e.file_name()) {
                if !name.to_string_lossy().ends_with(".jsonl") || ag.names.contains(&name) {
                    continue;
                }
                let path = dir.join(&name);
                if ag.names.len() < NAMES_CAP {
                    ag.names.insert(name);
                }
                if ag.followed.contains_key(&path) || ag.followed.len() >= FOLLOWED_CAP {
                    continue;
                }
                // an agent started while its session was watched is read
                // whole; at the first listing only recent ones are followed,
                // from their end
                if watched {
                    ag.followed.insert(path, Cursor::default());
                    continue;
                }
                self.stats += 1;
                if let Ok(m) = fs::metadata(&path)
                    && m.modified().is_ok_and(|t| now - secs(t) < FOLLOW_S)
                {
                    ag.followed.insert(path, Cursor { offset: m.len(), ids: VecDeque::new() });
                }
            }
        }
        let stats = &mut self.stats;
        ag.followed.retain(|path, cur| {
            *stats += 1;
            let Ok(m) = fs::metadata(path) else { return false };
            if m.modified().is_ok_and(|t| now - secs(t) >= FOLLOW_S) {
                return false;
            }
            follow(path, m.len(), cur, out);
            true
        });
    }

    /// The context of the last reply read from the session at
    /// `transcript`: what it sent and what it said.  `None` until a
    /// reply has been read since the shell started.
    pub(super) fn context(&self, transcript: &Path) -> Option<u64> {
        let (_, t) = self.main.get(transcript)?.ids.back()?;
        Some(t.input + t.cache_5m + t.cache_1h + t.cache_read + t.output)
    }

    /// Forget every session not in `keep`.
    pub(super) fn retain(&mut self, keep: &HashSet<PathBuf>) {
        self.main.retain(|p, _| keep.contains(p));
        self.agents.retain(|p, _| keep.contains(p));
    }
}

/// Parse what was appended to `path` beyond `cur`, `len` long now.
fn follow(path: &Path, len: u64, cur: &mut Cursor, out: &mut Vec<Spent>) {
    if len < cur.offset {
        cur.offset = len;
        return;
    }
    if len == cur.offset {
        return;
    }
    let Ok(mut f) = fs::File::open(path) else { return };
    if f.seek(SeekFrom::Start(cur.offset)).is_err() {
        return;
    }
    let mut buf = Vec::new();
    if f.take(READ_CAP).read_to_end(&mut buf).is_err() {
        return;
    }
    let Some(end) = buf.iter().rposition(|b| *b == b'\n') else { return };
    cur.offset += end as u64 + 1;
    for line in String::from_utf8_lossy(&buf[..end]).lines() {
        if let Some(s) = spent(line, cur) {
            out.push(s);
        }
    }
}

fn spent(line: &str, cur: &mut Cursor) -> Option<Spent> {
    if !line.contains(r#""usage""#) || !line.contains(r#""assistant""#) {
        return None;
    }
    let v = json::parse(line).ok()?;
    if v.str_at("type") != Some("assistant") {
        return None;
    }
    let msg = v.get("message")?;
    let model = msg.str_at("model")?;
    if model.starts_with('<') || model.contains(char::is_whitespace) {
        return None;
    }
    let t = Tokens::of(msg.get("usage")?);
    let id = msg.str_at("id").unwrap_or("");
    let tokens = if id.is_empty() { t } else { cur.add(id, t) };
    let at = marspot::cc_usage::parse_iso_utc(v.str_at("timestamp")?.to_string())?;
    (!tokens.is_empty()).then(|| Spent { at, model: model.to_string(), tokens })
}

#[cfg(test)]
#[path = "allocation_usage_tests.rs"]
mod tests;

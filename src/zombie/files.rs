//! Where a claude session's records are, and keeping a [`ClaudeLog`]
//! current from them.
//!
//! `<project dir>/<session>.jsonl` is the main conversation;
//! `<project dir>/<session>/subagents/agent-<id>.jsonl` and
//! `agent-<id>.meta.json` are one subagent each.  A long session has
//! hundreds of finished subagents there, so only those created since
//! the current claude process started and not marked finished are
//! read; the directory is listed again only when its mtime moves (a
//! subagent was added).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::pidtree::ProcId;

use super::tail::{Line, Tail};
use super::transcript::{ClaudeLog, MainLog, SubLog, SubMeta, Tasks};

/// Subagents read at once; a process running more is past what any
/// session has done here (seven at the most, 2026-11-25).
const TRACK: usize = 64;
/// Subagent ids remembered as already judged, per claude process.
const LOOKED: usize = 16 * 1024;

pub struct PaneFiles {
    pub agent: ProcId,
    pub uuid: String,
    dir: PathBuf,
    main: Tail,
    subs: HashMap<String, Tail>,
    looked: HashSet<String>,
    listed: Option<SystemTime>,
    /// Unlooked subagents left out of the last listing for want of room.
    waiting: usize,
    pub log: ClaudeLog,
}

impl PaneFiles {
    /// The records of session `uuid` in project directory `dir`, as
    /// written by the claude process `agent`.
    pub fn new(agent: ProcId, uuid: &str, dir: &Path) -> PaneFiles {
        PaneFiles {
            agent,
            uuid: uuid.to_string(),
            main: Tail::new(dir.join(format!("{uuid}.jsonl"))),
            dir: dir.to_path_buf(),
            subs: HashMap::new(),
            looked: HashSet::new(),
            listed: None,
            waiting: 0,
            log: ClaudeLog::default(),
        }
    }

    /// Bring the log up to the files.  `agent_start` is when the claude
    /// process started, unix seconds.
    pub fn refresh(&mut self, agent_start: u64) {
        let log = &mut self.log;
        let _ = self.main.read(|l| match l {
            Line::Restart => {
                log.main = MainLog::default();
                log.tasks = Tasks::default();
            }
            Line::Text(t) => log.feed_main(t),
        });
        let subdir = self.dir.join(&self.uuid).join("subagents");
        let mtime = fs::metadata(&subdir).and_then(|m| m.modified()).ok();
        // while some wait for room, list each time: a finished one frees it
        if mtime.is_some() && (mtime != self.listed || self.waiting > 0) {
            self.listed = mtime;
            self.list(&subdir, agent_start);
        }
        let ids: Vec<String> = self.subs.keys().cloned().collect();
        for id in ids {
            let meta = read_meta(&subdir, &id);
            if meta.finished {
                self.subs.remove(&id);
                self.log.subs.remove(&id);
                continue;
            }
            let Some(tail) = self.subs.get_mut(&id) else { continue };
            let log = &mut self.log;
            let _ = tail.read(|l| match l {
                Line::Restart => {
                    if let Some((sub, m)) = log.subs.get_mut(&id) {
                        *sub = SubLog::born(sub.first);
                        *m = meta.clone();
                    }
                }
                Line::Text(t) => log.feed_sub(&id, t),
            });
        }
    }

    fn list(&mut self, subdir: &Path, agent_start: u64) {
        let Ok(entries) = fs::read_dir(subdir) else { return };
        if self.looked.len() > LOOKED {
            self.looked.clear();
        }
        let mut over = 0;
        for e in entries.flatten() {
            let name = e.file_name();
            let Some(id) = name.to_str().and_then(|n| n.strip_prefix("agent-")?.strip_suffix(".jsonl")) else {
                continue;
            };
            if self.looked.contains(id) {
                continue;
            }
            // left unlooked, so a later listing takes it once there is room
            if self.subs.len() >= TRACK {
                over += 1;
                continue;
            }
            self.looked.insert(id.to_string());
            let Ok(m) = e.metadata() else { continue };
            let born = m.created().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
            if born < agent_start {
                continue;
            }
            let meta = read_meta(subdir, id);
            if meta.finished {
                continue;
            }
            self.subs.insert(id.to_string(), Tail::new(e.path()));
            let sub = SubLog::born(Some(born));
            self.log.subs.insert(id.to_string(), (sub, meta));
        }
        if over != self.waiting && over > 0 {
            crate::lx_warn!("zombie.subagents.over", "more unfinished subagents than are read at once; the rest wait", uuid = self.uuid.as_str(), tracked = TRACK, waiting = over);
        }
        self.waiting = over;
    }

    /// The subagent `id` is being read: it is of this process and not finished.
    pub fn has_sub(&self, id: &str) -> bool {
        self.subs.contains_key(id)
    }

    /// Bytes read from all of this pane's records so far.
    pub fn read_bytes(&self) -> u64 {
        self.main.read_bytes + self.subs.values().map(|t| t.read_bytes).sum::<u64>()
    }
}

fn read_meta(subdir: &Path, id: &str) -> SubMeta {
    fs::read_to_string(subdir.join(format!("agent-{id}.meta.json"))).map(|b| SubMeta::parse(&b)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zombie_files_take_a_subagent_left_out_for_want_of_room_once_one_finishes() {
        let dir = std::env::temp_dir().join(format!("zombie-files-{}", std::process::id()));
        let sub = dir.join("u").join("subagents");
        fs::create_dir_all(&sub).unwrap();
        let meta = |i: usize, finished: bool| {
            fs::write(sub.join(format!("agent-a{i}.meta.json")), format!(r#"{{"description":"d","finished":{finished}}}"#)).unwrap();
        };
        for i in 0..=TRACK {
            fs::write(sub.join(format!("agent-a{i}.jsonl")), "").unwrap();
            meta(i, false);
        }
        fs::write(dir.join("u.jsonl"), "").unwrap();
        let mut f = PaneFiles::new(ProcId { pid: 1, start_us: 0 }, "u", &dir);
        f.refresh(0);
        assert_eq!(f.subs.len(), TRACK);
        let left: Vec<usize> = (0..=TRACK).filter(|i| !f.has_sub(&format!("a{i}"))).collect();
        assert_eq!(left.len(), 1);
        let done = (0..=TRACK).find(|i| f.has_sub(&format!("a{i}"))).unwrap();
        meta(done, true);
        f.refresh(0);
        assert!(!f.has_sub(&format!("a{done}")));
        f.refresh(0);
        assert!(f.has_sub(&format!("a{}", left[0])), "the one left out is read now");
        let _ = fs::remove_dir_all(&dir);
    }
}

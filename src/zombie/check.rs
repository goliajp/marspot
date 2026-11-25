//! One check: every rule over a scene, and which hits have held long
//! enough to be suspects.
//!
//! A hit becomes a suspect when it was found in a check at least
//! [`STABLE`] before this one and in every check since; one check
//! without it starts it over.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use crate::pidtree::{ProcId, ProcRow, ProcTable};

use super::probe::Probe;
use super::rules::{Limits, MINUTE, Rule};
use super::transcript::ClaudeLog;

/// How long a hit must hold.
pub const STABLE: u64 = 10 * MINUTE;
/// Bytes of one hit's evidence; a command line can be far longer.
pub const EVIDENCE: usize = 2048;
/// CPU samples kept per subtree: an hour of checks five minutes apart,
/// with room.
const SAMPLES: usize = 32;

/// A claude pane in the scene.
pub struct PaneIn<'a> {
    pub sid: u64,
    /// The claude process.
    pub agent: &'a ProcRow,
    pub log: &'a ClaudeLog,
}

/// A process outside every pane that belongs to a claude pane.
pub struct OutsideIn<'a> {
    pub sid: u64,
    pub row: &'a ProcRow,
    /// The other processes of its process group outside the panes.
    pub group: Vec<&'a ProcRow>,
    /// The claude session that started it runs in no pane.
    pub session_gone: bool,
    /// Why it is this pane's, for the person reading the suspect.
    pub why: String,
}

pub struct Scene<'a> {
    /// Unix seconds.
    pub now: u64,
    pub table: &'a ProcTable,
    pub panes: Vec<PaneIn<'a>>,
    pub outside: Vec<OutsideIn<'a>>,
    pub uid: u32,
    pub tmpdir: Option<PathBuf>,
    /// A process claude keeps while it rests (an MCP server, a language
    /// server, an empty shell), given whether it has children: never a task.
    pub helper: &'a dyn Fn(&ProcRow, bool) -> bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    pub sid: u64,
    pub rule: Rule,
    /// `pid@start_us` of a process, or `agent:<id>` of a subagent.
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub key: Key,
    /// What was seen, in the words a session is told.
    pub evidence: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suspect {
    pub hit: Hit,
    /// When it was first found in the run of checks that holds it.
    pub since: u64,
}

pub fn proc_key(row: &ProcRow) -> String {
    format!("{}@{}", row.pid, row.start_us)
}

#[derive(Default)]
pub struct Watch {
    pub limits: Limits,
    /// `STABLE` is divided by this (0 reads as 1); see `limits::scale`.
    pub scale: u64,
    cpu: HashMap<ProcId, VecDeque<(u64, u64)>>,
    seen: HashMap<Key, u64>,
}

impl Watch {
    pub fn new(limits: Limits) -> Watch {
        Watch { limits, ..Watch::default() }
    }

    /// The rules' hits in this scene.
    pub fn hits(&mut self, s: &Scene, probe: &mut dyn Probe) -> Vec<Hit> {
        self.cpu.retain(|id, _| s.table.get(id.pid).is_some_and(|r| r.id() == *id));
        let mut hits = Vec::new();
        for p in &s.panes {
            hits.extend(super::pane::hits(self, s, p, probe));
        }
        for o in &s.outside {
            hits.extend(super::outside::hits(self, s, o, probe));
        }
        for h in &mut hits {
            cap(&mut h.evidence, EVIDENCE);
        }
        hits.sort_by(|a, b| a.key.cmp(&b.key));
        hits
    }

    /// The suspects of this scene: its hits that have held for [`STABLE`].
    pub fn check(&mut self, s: &Scene, probe: &mut dyn Probe) -> Vec<Suspect> {
        let hits = self.hits(s, probe);
        self.seen.retain(|k, _| hits.iter().any(|h| h.key == *k));
        let mut out = Vec::new();
        for h in hits {
            let since = *self.seen.entry(h.key.clone()).or_insert(s.now);
            if s.now >= since + STABLE / self.scale.max(1) {
                out.push(Suspect { hit: h, since });
            }
        }
        out
    }

    /// The hits of the last check, suspects or not yet.
    pub fn current(&self) -> impl Iterator<Item = &Key> {
        self.seen.keys()
    }

    /// CPU of `root`'s subtree, its reaped children included, over the
    /// last `window` seconds; None until a sample that old exists.
    /// Takes a sample now.
    pub(super) fn cpu_over(&mut self, s: &Scene, root: &ProcRow, window: u64, probe: &mut dyn Probe) -> Option<u64> {
        let own = probe.cpu(root)?;
        let total = own + s.table.descendants(root.pid).iter().filter_map(|r| probe.cpu(r)).sum::<u64>();
        let q = self.cpu.entry(root.id()).or_default();
        q.push_back((s.now, total));
        while q.len() > SAMPLES {
            q.pop_front();
        }
        let then = q.iter().rev().find(|(t, _)| s.now >= t + window)?;
        Some(total.saturating_sub(then.1))
    }
}

/// Cut `s` to at most `max` bytes on a character boundary, marking the cut.
fn cap(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max - "…".len();
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push('…');
}

/// `1 小时 5 分`, `12 分`, `40 秒`.
pub fn span(secs: u64) -> String {
    let (h, m) = (secs / 3600, secs % 3600 / 60);
    match (h, m) {
        (0, 0) => format!("{secs} 秒"),
        (0, m) => format!("{m} 分"),
        (h, 0) => format!("{h} 小时"),
        (h, m) => format!("{h} 小时 {m} 分"),
    }
}

/// Nanoseconds as seconds with one decimal.
pub fn secs(ns: u64) -> String {
    format!("{:.1} 秒", ns as f64 / 1e9)
}

#[cfg(test)]
mod tests;

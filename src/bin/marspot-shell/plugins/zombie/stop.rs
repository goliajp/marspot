//! Ending a process a session asked to end (`proc.stop`).
//!
//! A session names each process by pid and start time, and may end only
//! processes that belong to its own pane (`attrib`), never its agent,
//! its pane's shell, its own ancestors or a marspot process.  A tree is
//! ended top down so a parent that restarts its child cannot start a
//! new one: every process is stopped first, parent before children, the
//! children of each listed only once it is stopped; then all get
//! SIGTERM, then SIGCONT so they act on it.  Signals go by `ProcId`
//! only, never to a group or by name.
//!
//! Which pane is asking comes from `pane_control::caller`.  It scopes
//! the request to that pane and guards against mistakes; it is not a
//! security boundary: a caller that sends another pane's Codex thread
//! id is taken for that pane.

use std::collections::HashSet;
use std::path::PathBuf;

use marspot::pidtree::{self, ProcId, ProcRow, SignalError};

use super::attrib::{Attributor, Known, Owner};
use crate::observe::Observation;

pub(crate) mod force;

/// The most processes one tree is taken to hold; a larger one is ended
/// up to here.
pub(crate) const MAX_TREE: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// SIGTERM went to it (and to its tree).
    Stopped,
    /// No such process, or the pid names another process now.
    Gone,
    /// The kernel would not let it be signalled.
    Denied,
    /// It is not the caller's pane's.
    NotYours,
    /// It is the caller's own agent, shell, ancestor or a marspot
    /// process, or the id cannot be read.
    Refused,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Stopped => "stopped",
            Outcome::Gone => "gone",
            Outcome::Denied => "denied",
            Outcome::NotYours => "not_yours",
            Outcome::Refused => "refused",
        }
    }
}

/// What happened to one id of a request.
#[derive(Clone, Debug)]
pub(crate) struct Verdict {
    /// The id as the caller wrote it.
    pub asked: String,
    /// The process, as it was before anything was sent.
    pub row: Option<ProcRow>,
    /// Its argv joined by spaces, at most [`ARGV_BYTES`].
    pub argv: String,
    pub outcome: Outcome,
    pub message: String,
    /// Every process SIGTERM went to, the named one first.
    pub signalled: Vec<ProcId>,
}

/// What the caller may not end.
pub(crate) struct Guard {
    pub sid: u64,
    pub shell: Option<ProcId>,
    pub agent: Option<ProcId>,
    /// The agent's foreground group and name.
    pub agent_group: Option<(i32, String)>,
    /// The calling process and its ancestors up to launchd.
    pub ancestors: HashSet<ProcId>,
}

impl Guard {
    /// The guard for a caller in pane `sid` whose connecting process is
    /// `peer`.
    pub fn of(obs: &Observation, sid: u64, peer: Option<i32>) -> Guard {
        let s = obs.session(sid);
        let mut ancestors = HashSet::new();
        let mut at = peer.unwrap_or(0);
        // a process table is a tree; the bound is for one that changes under the walk
        while at > 1 && ancestors.len() < 64 {
            let Some(r) = pidtree::proc_row(at).or_else(|| obs.table.get(at).cloned()) else { break };
            ancestors.insert(r.id());
            at = r.ppid;
        }
        let front = s.and_then(|s| s.front()).map(|p| &p.row);
        Guard {
            sid,
            shell: s.and_then(|s| s.shell),
            agent: front.map(ProcRow::id),
            agent_group: front.map(|r| (r.pgid, r.comm.clone())),
            ancestors,
        }
    }

    /// Why `row` may not be ended by this caller, if it may not.
    fn forbids(&self, row: &ProcRow) -> Option<&'static str> {
        let id = row.id();
        if row.pid <= 1 {
            Some("pid 0 and 1 are never signalled")
        } else if Some(id) == self.shell {
            Some("it is your pane's shell")
        } else if Some(id) == self.agent || self.agent_group.as_ref().is_some_and(|(g, c)| row.pgid == *g && row.comm == *c) {
            // an agent can restart itself as a child of the same name in
            // its group (gemini does), and that child is the agent then
            Some("it is your pane's agent")
        } else if self.ancestors.contains(&id) {
            Some("it is one of your own ancestors")
        } else if is_marspot(row) {
            Some("it is a marspot process")
        } else {
            None
        }
    }
}

fn is_marspot(row: &ProcRow) -> bool {
    row.comm.to_ascii_lowercase().starts_with("marspot")
}

/// How much of a process's argv a verdict keeps.
pub(crate) const ARGV_BYTES: usize = 200;

fn verdict(asked: &str, row: Option<ProcRow>, outcome: Outcome, message: impl Into<String>) -> Verdict {
    let argv = row.as_ref().and_then(|r| pidtree::proc_argv(r.pid)).map_or_else(String::new, |a| clip(a.join(" "), ARGV_BYTES));
    Verdict { asked: asked.to_string(), row, argv, outcome, message: message.into(), signalled: Vec::new() }
}

/// `s` cut to at most `n` bytes, on a character boundary.
pub(crate) fn clip(mut s: String, n: usize) -> String {
    if s.len() > n {
        let mut end = n;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// Judge and, when it may be, end the process `asked` names.
pub(crate) fn one(obs: &Observation, attrib: &mut Attributor, guard: &Guard, asked: &str, tree: bool) -> Verdict {
    let Some((pid, start_us)) = marspot_peer::control::request::proc::parse_id(asked) else {
        return verdict(asked, None, Outcome::Refused, "not an id: write pid@start_us");
    };
    let Some(now) = pidtree::proc_row(pid) else { return verdict(asked, None, Outcome::Gone, "no process has this pid") };
    if now.start_us != start_us {
        return verdict(asked, None, Outcome::Gone, format!("pid {pid} is another process now (started at {})", now.start_us));
    }
    let row = obs.table.get(pid).filter(|r| r.id() == now.id()).cloned();
    let Some(row) = row else {
        return verdict(asked, Some(now), Outcome::NotYours, "it started after the latest look at the processes; ask again");
    };
    if let Some(why) = guard.forbids(&row) {
        return verdict(asked, Some(row), Outcome::Refused, why);
    }
    match attrib.owner(obs, &row) {
        Owner::Pane(sid) if sid == guard.sid => {}
        Owner::Pane(sid) => return verdict(asked, Some(row), Outcome::NotYours, format!("it belongs to pane {sid}")),
        Owner::Closed(sid) => return verdict(asked, Some(row), Outcome::NotYours, format!("it belongs to the closed pane {sid}")),
        Owner::Unknown | Owner::Later => return verdict(asked, Some(row), Outcome::NotYours, "nothing ties it to your pane"),
    }
    let id = row.id();
    // argv is read while the process still runs
    let mut v = verdict(asked, Some(row), Outcome::Stopped, "");
    let mut skip = |r: &ProcRow| {
        guard.forbids(r).is_some() || matches!(attrib.owner(obs, r), Owner::Pane(s) | Owner::Closed(s) if s != guard.sid)
    };
    let ended = if tree {
        end_tree(id, &mut skip)
    } else {
        pidtree::signal(id, libc::SIGTERM).map(|()| Tree { signalled: vec![id], denied: 0 })
    };
    match ended {
        Ok(t) => {
            v.signalled = t.signalled;
            if t.denied > 0 {
                v.message = format!("{} descendants could not be signalled, left alone", t.denied);
            }
        }
        Err(SignalError::Gone) => (v.outcome, v.message) = (Outcome::Gone, "it ended before it was signalled".into()),
        Err(SignalError::Denied) => (v.outcome, v.message) = (Outcome::Denied, "not permitted to signal it".into()),
        Err(e) => (v.outcome, v.message) = (Outcome::Denied, e.to_string()),
    }
    v
}

/// What [`end_tree`] signalled.
pub(crate) struct Tree {
    /// Every process signalled, the root first.
    pub signalled: Vec<ProcId>,
    /// Descendants the kernel would not let be stopped (a setuid
    /// program); they and what is under them were left alone.
    pub denied: usize,
}

/// Stop `root` and everything under it, parents first, then SIGTERM
/// and SIGCONT them all.  `skip` names processes left alone, with what
/// is under them.
pub(crate) fn end_tree(root: ProcId, skip: &mut dyn FnMut(&ProcRow) -> bool) -> Result<Tree, SignalError> {
    pidtree::signal(root, libc::SIGSTOP)?;
    let mut t = Tree { signalled: vec![root], denied: 0 };
    let mut seen = HashSet::from([root]);
    // a stopped process forks no more, so its children listed after the
    // stop are all it will have; the second round finds any forked while
    // a parent was still being reached
    for _ in 0..2 {
        let mut i = 0;
        while i < t.signalled.len() && t.signalled.len() < MAX_TREE {
            let parent = t.signalled[i];
            i += 1;
            for r in children(parent) {
                if t.signalled.len() >= MAX_TREE || !seen.insert(r.id()) || skip(&r) {
                    continue;
                }
                match pidtree::signal(r.id(), libc::SIGSTOP) {
                    Ok(()) => t.signalled.push(r.id()),
                    Err(SignalError::Gone) => {}
                    Err(_) => t.denied += 1,
                }
            }
        }
    }
    for id in &t.signalled {
        let _ = pidtree::signal(*id, libc::SIGTERM);
    }
    for id in &t.signalled {
        let _ = pidtree::signal(*id, libc::SIGCONT);
    }
    Ok(t)
}

/// The children of `parent`; none when its pid names another process.
/// A pid is reused only after its process is gone, so what is listed
/// while `parent` is the same process before and after is its own.
fn children(parent: ProcId) -> Vec<ProcRow> {
    if pidtree::proc_id(parent.pid) != Some(parent) {
        return Vec::new();
    }
    let rows: Vec<ProcRow> = pidtree::child_pids(parent.pid)
        .into_iter()
        .filter_map(pidtree::proc_row)
        .filter(|r| r.ppid == parent.pid && r.start_us >= parent.start_us)
        .collect();
    if pidtree::proc_id(parent.pid) != Some(parent) {
        return Vec::new();
    }
    rows
}

/// Everything one request decides, for the op to answer and audit.
pub(crate) struct Stopped {
    pub verdicts: Vec<Verdict>,
    /// The ids the zombie watch has told this pane about.
    pub told: HashSet<String>,
}

/// End the processes `ids` name for a caller in pane `sid`.
pub(crate) fn run(obs: &Observation, known: &Known, sid: u64, peer: Option<i32>, ids: &[String], tree: bool) -> Stopped {
    let guard = Guard::of(obs, sid, peer);
    let mut attrib = Attributor::default();
    attrib.begin(obs, known);
    let verdicts = ids.iter().map(|a| one(obs, &mut attrib, &guard, a, tree)).collect();
    Stopped { verdicts, told: told(&told_path(), sid) }
}

pub(crate) fn told_path() -> PathBuf {
    marspot_term::paths::state_root().join("zombie").join("told.tsv")
}

/// The ids the watch's ledger (`sid  id  first_told_unix` per line)
/// lists for pane `sid`; empty when there is no ledger.
pub(crate) fn told(path: &std::path::Path, sid: u64) -> HashSet<String> {
    let Ok(text) = std::fs::read_to_string(path) else { return HashSet::new() };
    let want = sid.to_string();
    text.lines()
        .filter_map(|l| {
            let mut cols = l.split('\t');
            (cols.next() == Some(want.as_str())).then(|| cols.next().map(String::from)).flatten()
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests;

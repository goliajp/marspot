//! What a claude session's records say, folded record by record: the
//! main conversation's turn and tasks, and each subagent's last state.
//!
//! Every map is bounded: a session that ran for weeks has hundreds of
//! thousands of records, and only the recent ones matter.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::hash::Hash;

use super::records::{Note, Rec, ToolUse};
use super::rules::{Last, Subagent};

/// A map that forgets its oldest keys past `cap`.
#[derive(Clone, Debug)]
pub struct Bounded<K, V> {
    map: HashMap<K, V>,
    order: VecDeque<K>,
    cap: usize,
}

impl<K: Clone + Eq + Hash, V> Bounded<K, V> {
    pub fn new(cap: usize) -> Self {
        Bounded { map: HashMap::new(), order: VecDeque::new(), cap }
    }

    pub fn insert(&mut self, k: K, v: V) {
        if self.map.insert(k.clone(), v).is_none() {
            self.order.push_back(k);
            while self.order.len() > self.cap {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
    }

    /// Insert only when absent: the first answer stays.
    pub fn first(&mut self, k: K, v: V) {
        if !self.map.contains_key(&k) {
            self.insert(k, v);
        }
    }

    pub fn get(&self, k: &K) -> Option<&V> {
        self.map.get(k)
    }

    pub fn remove(&mut self, k: &K) -> Option<V> {
        let v = self.map.remove(k)?;
        self.order.retain(|o| o != k);
        Some(v)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.map.values()
    }
}

const TASKS: usize = 512;
const CALLS: usize = 256;

/// A task an agent started, as its records say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskRec {
    pub tool: String,
    pub background: bool,
    pub command: Option<String>,
}

/// What every transcript of a session shares: the tool calls whose
/// results name a task, and the notifications of the tasks' ends.
#[derive(Clone, Debug)]
pub struct Tasks {
    calls: Bounded<String, ToolUse>,
    pub started: Bounded<String, TaskRec>,
    /// task id → when the first notification of its end came.
    pub ended: Bounded<String, u64>,
    /// subagent id → when it was last reported finished or stopped; a
    /// subagent resumed after that writes records past it.
    pub finished: Bounded<String, u64>,
}

impl Default for Tasks {
    fn default() -> Self {
        Tasks {
            calls: Bounded::new(CALLS),
            started: Bounded::new(TASKS),
            ended: Bounded::new(TASKS),
            finished: Bounded::new(TASKS),
        }
    }
}

impl Tasks {
    fn uses(&mut self, uses: &[ToolUse]) {
        for u in uses {
            self.calls.insert(u.id.clone(), u.clone());
        }
    }

    /// The tasks `results` started, in order.
    fn results(&mut self, results: &[(String, Option<String>)]) -> Vec<String> {
        let mut out = Vec::new();
        for (call, task) in results {
            let use_ = self.calls.remove(call);
            let Some(task) = task else { continue };
            let rec = use_.map_or(TaskRec { tool: String::new(), background: true, command: None }, |u| TaskRec {
                tool: u.name,
                background: u.background,
                command: u.command,
            });
            self.started.insert(task.clone(), rec);
            out.push(task.clone());
        }
        out
    }

    fn notes(&mut self, at: u64, notes: &[Note]) {
        for n in notes.iter().filter(|n| n.ended) {
            self.ended.first(n.task.clone(), at);
            if !n.waiting {
                self.finished.insert(n.task.clone(), at);
            }
        }
    }
}

/// The main conversation.
#[derive(Clone, Debug, Default)]
pub struct MainLog {
    /// When the current turn ended; None while one runs.
    pub turn_ended: Option<u64>,
    pub last_at: u64,
}

impl MainLog {
    pub fn feed(&mut self, r: &Rec, tasks: &mut Tasks) {
        self.last_at = self.last_at.max(r.at());
        match r {
            Rec::Assistant { uses, .. } => {
                tasks.uses(uses);
                self.turn_ended = None;
            }
            Rec::User { results, notes, at } => {
                tasks.results(results);
                tasks.notes(*at, notes);
                self.turn_ended = None;
            }
            Rec::TurnEnd { at } => self.turn_ended = Some(*at),
            Rec::Queued { at, notes } => tasks.notes(*at, notes),
        }
    }
}

/// One subagent's transcript.
#[derive(Clone, Debug)]
pub struct SubLog {
    pub first: Option<u64>,
    pub last_at: u64,
    pub last: Last,
    open: Bounded<String, ToolUse>,
    /// The tasks it started, newest last.
    pub tasks: VecDeque<String>,
}

impl Default for SubLog {
    fn default() -> Self {
        SubLog { first: None, last_at: 0, last: Last::Busy, open: Bounded::new(CALLS), tasks: VecDeque::new() }
    }
}

impl SubLog {
    /// A log whose record began at `first`, read or not.
    pub fn born(first: Option<u64>) -> SubLog {
        SubLog { first, ..SubLog::default() }
    }

    pub fn feed(&mut self, r: &Rec, tasks: &mut Tasks) {
        let at = r.at();
        self.first.get_or_insert(at);
        match r {
            Rec::Assistant { uses, end, .. } => {
                tasks.uses(uses);
                for u in uses {
                    self.open.insert(u.id.clone(), u.clone());
                }
                let bash = uses.iter().rev().find(|u| u.name == "Bash" && !u.background);
                self.last = match bash {
                    Some(u) => Last::Bash { at, timeout_ms: u.timeout_ms },
                    None if *end && uses.is_empty() && self.open.is_empty() => Last::Idle,
                    None => Last::Busy,
                };
                self.last_at = at;
            }
            Rec::User { results, notes, .. } => {
                for (call, _) in results {
                    self.open.remove(call);
                }
                for t in tasks.results(results) {
                    if self.tasks.len() == TASKS {
                        self.tasks.pop_front();
                    }
                    self.tasks.push_back(t);
                }
                tasks.notes(at, notes);
                self.last = Last::Busy;
                self.last_at = at;
            }
            Rec::Queued { notes, .. } => tasks.notes(at, notes),
            Rec::TurnEnd { .. } => {}
        }
    }

    /// The view the rules take, with `alive` saying which of its tasks
    /// still have a process.
    pub fn view<'a>(&self, id: &'a str, finished: bool, alive: impl Fn(&str) -> bool) -> Subagent<'a> {
        Subagent {
            id,
            first: self.first.unwrap_or(0),
            last_at: self.last_at,
            last: self.last,
            finished,
            tasks_alive: self.tasks.iter().filter(|t| alive(t)).count(),
        }
    }
}

/// What a subagent's meta file says about it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubMeta {
    pub description: String,
    pub depth: u32,
    pub parent: Option<String>,
    pub finished: bool,
}

impl SubMeta {
    pub fn parse(body: &str) -> SubMeta {
        let Ok(v) = marspot_peer::json::parse(body) else { return SubMeta::default() };
        SubMeta {
            description: v.str_at("description").unwrap_or("").to_string(),
            depth: v.get("spawnDepth").and_then(|d| d.i64()).and_then(|d| u32::try_from(d).ok()).unwrap_or(1),
            parent: v.str_at("parentAgentId").map(str::to_string),
            finished: v.get("finished").and_then(|f| f.bool()) == Some(true),
        }
    }
}

/// One claude session: its main conversation and the subagents of its
/// current process that have not finished.
#[derive(Clone, Debug, Default)]
pub struct ClaudeLog {
    pub main: MainLog,
    pub tasks: Tasks,
    pub subs: BTreeMap<String, (SubLog, SubMeta)>,
}

impl ClaudeLog {
    pub fn feed_main(&mut self, line: &str) {
        if let Some(r) = super::records::parse(line) {
            self.main.feed(&r, &mut self.tasks);
        }
    }

    pub fn feed_sub(&mut self, id: &str, line: &str) {
        let Some(r) = super::records::parse(line) else { return };
        if let Some((log, _)) = self.subs.get_mut(id) {
            log.feed(&r, &mut self.tasks);
        }
    }
}

#[cfg(test)]
mod tests;

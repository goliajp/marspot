//! The tool calls and tasks one record file has shown, in fixed space.
//!
//! A table that runs out of room forgets its oldest entry and sets
//! `overflowed`: from then on it is no longer the whole story, and a
//! reader that judges by it (zombie) does not judge that session.

use super::short_id::ShortId;

/// Open tool calls kept.
pub const CALLS: usize = 256;
/// Tasks kept: started, ended, or both.
pub const TASKS: usize = 512;

/// What a subagent last did, as its record says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubLast {
    /// Its own reply, with no tool call left open.
    Idle,
    /// A foreground `Bash` call it still waits on, and the call's limit.
    Bash { at: u64, timeout_ms: Option<u64> },
    Busy,
}

#[derive(Clone, Copy, Debug)]
struct Call {
    id: ShortId,
    background: bool,
    seq: u32,
}

/// How a task was started: `known_call`: the call that started it was
/// read; `background`: that call's, or true when it was not read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskStart {
    pub known_call: bool,
    pub background: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Task {
    pub id: ShortId,
    pub started: Option<TaskStart>,
    /// When the first notification of its end came.
    pub ended_at: Option<u64>,
    /// When it was last reported finished (not waiting on work of its own).
    pub finished_at: Option<u64>,
    /// Order among the tasks this file started.
    pub own: Option<u32>,
    seq: u32,
}

const NO_CALL: Call = Call { id: ShortId::EMPTY, background: false, seq: 0 };
const NO_TASK: Task = Task { id: ShortId::EMPTY, started: None, ended_at: None, finished_at: None, own: None, seq: 0 };

#[derive(Clone, Debug)]
pub struct TaskTable {
    calls: [Call; CALLS],
    n_calls: usize,
    tasks: [Task; TASKS],
    n_tasks: usize,
    seq: u32,
    own: u32,
    /// The first record's time.
    pub first: Option<u64>,
    /// The latest time of any record.
    pub last_seen: u64,
    /// The time of the last reply or result.
    pub last_active: u64,
    pub last: SubLast,
    pub overflowed: bool,
}

impl Default for TaskTable {
    fn default() -> Self {
        TaskTable {
            calls: [NO_CALL; CALLS],
            n_calls: 0,
            tasks: [NO_TASK; TASKS],
            n_tasks: 0,
            seq: 0,
            own: 0,
            first: None,
            last_seen: 0,
            last_active: 0,
            last: SubLast::Busy,
            overflowed: false,
        }
    }
}

impl TaskTable {
    fn next(&mut self) -> u32 {
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }

    /// A tool call was made; a call already open keeps its place.
    pub fn called(&mut self, call: &str, background: bool) {
        let id = ShortId::new(call);
        if let Some(c) = self.calls[..self.n_calls].iter_mut().find(|c| c.id == id) {
            c.background = background;
            return;
        }
        if self.n_calls == CALLS {
            let oldest = (0..self.n_calls).min_by_key(|i| self.calls[*i].seq).unwrap_or(0);
            self.calls.swap(oldest, self.n_calls - 1);
            self.n_calls -= 1;
            self.overflowed = true;
        }
        let seq = self.next();
        self.calls[self.n_calls] = Call { id, background, seq };
        self.n_calls += 1;
    }

    /// The result of `call` came back: the call is closed.  Its
    /// `background`, when it was open.
    pub fn answered(&mut self, call: &str) -> Option<bool> {
        let id = ShortId::new(call);
        let i = self.calls[..self.n_calls].iter().position(|c| c.id == id)?;
        let bg = self.calls[i].background;
        self.calls.swap(i, self.n_calls - 1);
        self.n_calls -= 1;
        Some(bg)
    }

    pub fn open_calls(&self) -> usize {
        self.n_calls
    }

    fn slot(&mut self, task: &str) -> &mut Task {
        let id = ShortId::new(task);
        if let Some(i) = self.tasks[..self.n_tasks].iter().position(|t| t.id == id) {
            return &mut self.tasks[i];
        }
        if self.n_tasks == TASKS {
            let oldest = (0..self.n_tasks).min_by_key(|i| self.tasks[*i].seq).unwrap_or(0);
            self.tasks.swap(oldest, self.n_tasks - 1);
            self.n_tasks -= 1;
            self.overflowed = true;
        }
        let seq = self.next();
        self.tasks[self.n_tasks] = Task { id, seq, ..NO_TASK };
        self.n_tasks += 1;
        &mut self.tasks[self.n_tasks - 1]
    }

    /// A result says `task` started; `own`: this file's agent started it.
    pub fn started(&mut self, task: &str, how: TaskStart, own: bool) {
        self.own += u32::from(own);
        let order = self.own;
        let t = self.slot(task);
        t.started = Some(how);
        if own {
            t.own = Some(order);
        }
    }

    /// A notification says `task` ended at `at`.
    pub fn ended(&mut self, task: &str, at: u64, waiting: bool) {
        let t = self.slot(task);
        t.ended_at.get_or_insert(at);
        if !waiting {
            t.finished_at = Some(at);
        }
    }

    pub fn tasks(&self) -> &[Task] {
        &self.tasks[..self.n_tasks]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_table_closes_calls_and_keeps_the_first_end() {
        let mut t = TaskTable::default();
        t.called("c1", true);
        assert_eq!(t.answered("c1"), Some(true));
        assert_eq!(t.answered("c1"), None, "a call closes once");
        t.started("b1", TaskStart { known_call: true, background: true }, true);
        t.ended("b1", 20, true);
        t.ended("b1", 30, false);
        let b1 = t.tasks()[0];
        assert_eq!((b1.ended_at, b1.finished_at, b1.own), (Some(20), Some(30), Some(1)));
        assert!(!t.overflowed);
    }

    #[test]
    fn task_table_says_when_it_ran_out_of_room() {
        let mut t = TaskTable::default();
        for n in 0..TASKS {
            t.ended(&format!("t{n}"), 1, false);
        }
        assert!(!t.overflowed);
        t.ended("one more", 1, false);
        assert!(t.overflowed);
        assert_eq!(t.tasks().len(), TASKS);
        assert!(!t.tasks().iter().any(|x| x.id.is("t0")), "the oldest went");
        for n in 0..=CALLS {
            t.called(&format!("c{n}"), false);
        }
        assert_eq!(t.open_calls(), CALLS);
    }
}

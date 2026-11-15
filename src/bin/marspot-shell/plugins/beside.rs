//! Which processes of one agent have run where, as far as they were
//! seen: what tells a pane's agent that it is the only one of its kind
//! in its place.
//!
//! An agent's session is sometimes told by elimination: the one thread
//! or session file in a directory that nobody else has.  That holds
//! only while no other process of the same agent has been in that
//! place since this one started, in a pane or not; what another could
//! have opened is not this one's for being the only one found.
//!
//! What is known comes from looks at the process table, and a look is
//! a sample: a process that started and exited between two of them is
//! in neither.

use std::collections::HashMap;

use marspot::pidtree::{ProcId, ProcRow, ProcTable};

/// Which processes are one agent's, asked once per process image:
/// telling takes the command line, and the name the kernel keeps for
/// a process changes when it execs.
#[derive(Default)]
pub(crate) struct Kinds(HashMap<ProcId, (String, bool)>);

impl Kinds {
    /// The rows of `table` that `is` says yes to.  What was asked
    /// about a process no longer in the table is dropped.
    pub(crate) fn of<'t>(&mut self, table: &'t ProcTable, is: impl Fn(&ProcRow) -> bool) -> Vec<&'t ProcRow> {
        let mut out = Vec::new();
        for row in table.rows() {
            let id = row.id();
            let known = self.0.get(&id).filter(|(comm, _)| *comm == row.comm).map(|(_, yes)| *yes);
            let yes = known.unwrap_or_else(|| {
                let yes = is(row);
                self.0.insert(id, (row.comm.clone(), yes));
                yes
            });
            if yes {
                out.push(row);
            }
        }
        self.0.retain(|id, _| table.get(id.pid).is_some_and(|r| r.id() == *id));
        out
    }
}

/// Where each process of one agent was running and when it was last
/// seen there, in milliseconds since the epoch.  A place is whatever
/// the agent keeps sessions apart by (a directory, or a home and a
/// directory); one that could not be read is None, and counts as any
/// place.
#[derive(Default)]
pub(crate) struct Beside {
    procs: HashMap<ProcId, (Option<String>, u64)>,
    /// When the last look was taken.
    looked_ms: u64,
}

impl Beside {
    /// A look taken at `now_ms` found `alive`.  A process that has
    /// gone is forgotten once no pane's agent is old enough to have
    /// run beside it, so what is kept is bounded by the panes there
    /// are.
    pub(crate) fn note(&mut self, alive: Vec<(ProcId, Option<String>)>, now_ms: u64, oldest_pane_ms: Option<u64>) {
        for (id, place) in alive {
            self.procs.insert(id, (place, now_ms));
        }
        self.procs.retain(|_, (_, last)| *last == now_ms || oldest_pane_ms.is_some_and(|t| *last >= t));
        self.looked_ms = now_ms;
    }

    /// No process but `me` has been seen in `place` since `since_ms`.
    ///
    /// Only a look that found `me` says so: one that did not is not a
    /// look at the processes there are (the observer has stopped, or
    /// the table could not be read), and seeing nobody in it is not
    /// evidence that nobody is there.
    pub(crate) fn alone(&self, me: ProcId, place: &str, since_ms: u64) -> bool {
        self.procs.get(&me).is_some_and(|(_, last)| *last == self.looked_ms)
            && !self
                .procs
                .iter()
                .any(|(id, (at, last))| *id != me && *last >= since_ms && at.as_deref().is_none_or(|p| p == place))
    }
}

/// Milliseconds since the epoch, now.
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: ProcId = ProcId { pid: 10, start_us: 60_000_000 };
    const OTHER: ProcId = ProcId { pid: 11, start_us: 1 };
    fn at(place: &str) -> Option<String> {
        Some(place.to_string())
    }

    #[test]
    fn a_process_is_alone_until_another_is_seen_in_its_place_after_it_started() {
        let mut s = Beside::default();
        s.note(vec![(ME, at("/p"))], 61_000, Some(60_000));
        assert!(s.alone(ME, "/p", 60_000), "itself does not count");
        s.note(vec![(ME, at("/p")), (OTHER, at("/q"))], 62_000, Some(60_000));
        assert!(s.alone(ME, "/p", 60_000), "another place");
        s.note(vec![(ME, at("/p")), (OTHER, at("/p"))], 63_000, Some(60_000));
        assert!(!s.alone(ME, "/p", 60_000));
        // the other exits: it was still there after this one started
        s.note(vec![(ME, at("/p"))], 64_000, Some(60_000));
        assert!(!s.alone(ME, "/p", 60_000));
    }

    #[test]
    fn one_last_seen_before_this_started_was_not_beside_it() {
        let mut s = Beside::default();
        s.note(vec![(OTHER, at("/p"))], 50_000, None);
        s.note(vec![(ME, at("/p"))], 61_000, Some(60_000));
        assert!(s.alone(ME, "/p", 60_000));
    }

    #[test]
    fn a_process_whose_place_is_not_known_counts_everywhere() {
        let mut s = Beside::default();
        s.note(vec![(ME, at("/p")), (OTHER, None)], 61_000, Some(60_000));
        assert!(!s.alone(ME, "/p", 60_000));
    }

    #[test]
    fn one_that_has_gone_is_forgotten_when_no_pane_ran_beside_it() {
        let mut s = Beside::default();
        s.note(vec![(OTHER, at("/p"))], 50_000, Some(40_000));
        s.note(vec![], 51_000, Some(40_000));
        assert_eq!(s.procs.len(), 1, "a pane older than its last sighting ran beside it");
        s.note(vec![], 52_000, Some(60_000));
        assert!(s.procs.is_empty());
        s.note(vec![(OTHER, at("/p"))], 53_000, None);
        s.note(vec![], 54_000, None);
        assert!(s.procs.is_empty());
    }

    /// A look that does not hold the pane's own agent saw nothing that
    /// can be relied on.
    #[test]
    fn a_look_that_missed_the_process_itself_says_nothing() {
        let mut s = Beside::default();
        assert!(!s.alone(ME, "/p", 60_000), "no look at all");
        s.note(vec![(ME, at("/p"))], 61_000, Some(60_000));
        assert!(s.alone(ME, "/p", 60_000));
        s.note(vec![], 62_000, Some(60_000));
        assert!(!s.alone(ME, "/p", 60_000), "an empty table");
    }
}

//! The inbox of the session that runs in a parent pane, found by the
//! pane's processes when its name finds nothing.
//!
//! The pane table names an agent only while it is the program in front
//! of its pane (`Session::agent`).  A caller started under a wrapper
//! (`timeout 600 claude`, `nice claude`) is not in front, so its pane's
//! name reaches nobody; its registry record still has an inbox, and the
//! helper's parent pane says whose processes to look among.

use marspot_peer::registry::Entry;

use crate::observe::Observation;

/// The inbox of the agent session running in pane `parent`: the one
/// registered session among the pane's processes that no other one
/// there started.  None when there is none, or more than one.
pub(super) fn in_pane(entries: &[Entry], obs: &Observation, parent: u64) -> Option<String> {
    let x = obs.session(parent).filter(|x| x.live)?;
    let ppid_of = |pid: i32| x.descendants.iter().find(|p| p.row.pid == pid).map(|p| p.row.ppid);
    // a record of ours is keyed by its relay; its agent is in the pane
    let pid_in_pane = |e: &Entry| [Some(e.pid), e.agent_pid].into_iter().flatten().find(|p| ppid_of(*p).is_some());
    let found: Vec<(i32, &str)> = entries.iter().filter_map(|e| Some((pid_in_pane(e)?, e.socket.as_deref()?))).collect();
    // bounded: a table taken while pids are reused may hold a cycle
    let started_by_another = |pid: i32| {
        let mut ups = std::iter::successors(ppid_of(pid), |p| ppid_of(*p)).take(x.descendants.len());
        ups.any(|p| found.iter().any(|(f, _)| *f == p))
    };
    let mut top = found.iter().filter(|(pid, _)| !started_by_another(*pid));
    match (top.next(), top.next()) {
        (Some((_, socket)), None) => Some((*socket).to_string()),
        _ => None,
    }
}

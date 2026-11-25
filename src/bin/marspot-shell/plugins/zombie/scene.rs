//! A check's input, from the observation the shell already has: the
//! claude panes with their records, and the processes outside every
//! pane a rule may look at, placed by `attrib`.
//!
//! Only two kinds of outside process are placed at all, the ones O1
//! and O2 can name: a `tail`, with the rest of its process group, and
//! anything running for `o2_age`.  Placing reads an environment, and
//! there are hundreds of processes launchd adopted.

use std::collections::{HashMap, HashSet};

use marspot::pidtree::{ProcId, ProcRow};
use marspot::zombie::check::{OutsideIn, PaneIn};
use marspot::zombie::files::PaneFiles;
use marspot::zombie::rules::Limits;

use super::attrib::{Attributor, Known, Owner};
use crate::observe::Observation;
use crate::plugins::claudecode::{pane_claude, pane_of_session};

/// The panes' processes: shells and everything under them.
pub(super) fn in_panes(obs: &Observation) -> HashSet<i32> {
    let mut pids = HashSet::new();
    for s in obs.live() {
        if let Some(shell) = s.shell {
            pids.insert(shell.pid);
        }
        pids.extend(s.descendants.iter().map(|p| p.row.pid));
    }
    pids
}

/// Bring each claude pane's records up to date; drop the panes gone.
/// Returns each pane's claude process.
pub(super) fn refresh(obs: &Observation, known: &Known, files: &mut HashMap<u64, PaneFiles>) -> Vec<(u64, ProcId)> {
    let mut agents = Vec::new();
    for cp in &known.claude {
        let Some(s) = obs.session(cp.sid).filter(|s| s.live) else { continue };
        let (Some(agent), Some(dir)) = (pane_claude(s), cp.transcripts.as_deref()) else { continue };
        let id = agent.row.id();
        let f = files.entry(cp.sid).or_insert_with(|| PaneFiles::new(id, &cp.uuid, dir));
        if f.agent != id || f.uuid != cp.uuid {
            *f = PaneFiles::new(id, &cp.uuid, dir);
        }
        f.refresh(agent.row.start_unix);
        agents.push((cp.sid, id));
    }
    files.retain(|sid, _| agents.iter().any(|(s, _)| s == sid));
    agents
}

pub(super) fn panes<'a>(obs: &'a Observation, agents: &[(u64, ProcId)], files: &'a HashMap<u64, PaneFiles>) -> Vec<PaneIn<'a>> {
    agents
        .iter()
        .filter_map(|(sid, id)| {
            let agent = obs.table.get(id.pid).filter(|r| r.id() == *id)?;
            Some(PaneIn { sid: *sid, agent, log: &files.get(sid)?.log })
        })
        .collect()
}

/// The outside processes that belong to a claude pane and that O1 or
/// O2 may name.
pub(super) fn outside<'a>(
    obs: &'a Observation,
    in_pane: &HashSet<i32>,
    known: &Known,
    attrib: &mut Attributor,
    now: u64,
    l: &Limits,
) -> Vec<OutsideIn<'a>> {
    let claude: HashSet<u64> = known.claude.iter().map(|c| c.sid).collect();
    let outside = |r: &&ProcRow| r.pid > 1 && !in_pane.contains(&r.pid);
    let mut out = Vec::new();
    for r in obs.table.rows().iter().filter(outside) {
        let tail = r.comm == "tail";
        if !tail && now.saturating_sub(r.start_unix) < l.o2_age {
            continue;
        }
        let group: Vec<&ProcRow> =
            if tail { obs.table.rows().iter().filter(outside).filter(|o| o.pgid == r.pgid && o.pid != r.pid).collect() } else { Vec::new() };
        // a tail of a system binary keeps its environment to itself; its pipe partner does not
        let placed = std::iter::once(r).chain(group.iter().copied()).find_map(|m| match attrib.owner(obs, m) {
            Owner::Pane(sid) => Some((sid, m)),
            _ => None,
        });
        let Some((sid, by)) = placed.filter(|(sid, _)| claude.contains(sid)) else { continue };
        let session = attrib.session(obs, by);
        let gone = session.as_deref().is_some_and(|s| pane_of_session(&known.claude, s).is_none());
        let why = match session.as_deref().and_then(|s| pane_of_session(&known.claude, s)) {
            Some(p) if p == sid => "它继承的环境变量 CLAUDE_CODE_SESSION_ID 是这个会话",
            _ => "它继承的窗格令牌是这个窗格",
        };
        out.push(OutsideIn { sid, row: r, group, session_gone: gone, why: why.into() });
    }
    out
}

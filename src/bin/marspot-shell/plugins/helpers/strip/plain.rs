//! Shadow panes no helper runs in (`pane.open` in the shadows): each is
//! listed under its top pane as a helper is, so that every shadow pane
//! the title counts has a row to look at it from.  A row says what is
//! in front of the pane, what it is titled and how it is; no figures,
//! as nothing times it.

use marspot::pane_name::base_name;
use marspot::pane_state::{BusyKind, PaneStatus};
use marspot::shell_proto::{HelperStrip, StripAgent, StripRow, StripState};

use super::super::State;
use super::{Memory, above, agent_at};
use crate::observe::{Observation, Session};

/// What the shell knows of a pane besides the helpers' table: its
/// state, and the title its plugin gave it.
pub(in crate::plugins::helpers) struct Panes<'a> {
    pub status: &'a dyn Fn(u64) -> Option<PaneStatus>,
    pub title: &'a dyn Fn(u64) -> Option<String>,
}

/// Whether `obs` has a shadow pane open.
pub(super) fn any(obs: &Observation) -> bool {
    obs.live().any(|x| x.entry.shadow_of != 0)
}

/// The shadow panes open in `obs` that `is_helper` says no helper runs in.
fn of<'a>(obs: &'a Observation, is_helper: impl Fn(u64) -> bool + 'a) -> impl Iterator<Item = &'a Session> + 'a {
    obs.live().filter(move |x| x.entry.shadow_of != 0 && !is_helper(x.entry.id))
}

fn state(status: Option<PaneStatus>) -> StripState {
    match status {
        Some(PaneStatus::Busy(BusyKind::ToolAwaitingApproval)) => StripState::Waiting,
        Some(PaneStatus::Busy(_) | PaneStatus::Contradiction { .. }) => StripState::Working,
        Some(PaneStatus::Empty | PaneStatus::AwaitingUser | PaneStatus::Dormant) => StripState::Idle,
        // not looked at yet
        Some(PaneStatus::Unknown) | None => StripState::Preparing,
    }
}

fn agent_name(a: StripAgent) -> Option<&'static str> {
    match a {
        StripAgent::Claude => Some("claude"),
        StripAgent::Codex => Some("codex"),
        StripAgent::Gemini => Some("gemini"),
        StripAgent::Kimi => Some("kimi"),
        StripAgent::Other | StripAgent::Unknown(_) => None,
    }
}

fn plain(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// The row of shadow pane `x`, whose agent in front is `agent`: named by
/// that agent, else by the program in front, else by its shell.
fn row(x: &Session, obs: &Observation, agent: StripAgent, panes: &Panes, number: Option<u16>) -> StripRow {
    let sid = x.entry.id;
    let shell = x.shell.and_then(|id| obs.table.get(id.pid));
    let name = match agent_name(agent) {
        Some(a) => a.to_string(),
        None => x.front().map(|p| p.name().to_string()).filter(|n| !n.is_empty()).or_else(|| shell.map(|r| r.comm.clone())).unwrap_or_default(),
    };
    let task = (panes.title)(sid).filter(|t| !t.trim().is_empty()).unwrap_or_else(|| base_name(&x.entry.cwd));
    // the shell outlives the session's process over an update
    let started_ms = shell.map_or(x.entry.created_at_unix.saturating_mul(1000), |r| r.start_us / 1000);
    StripRow {
        sid,
        number,
        state: state((panes.status)(sid)),
        agent,
        gone: false,
        had_tool: true,
        plain: true,
        started_ms,
        worked_ms: 0,
        running_since_ms: None,
        ended_ms: None,
        tokens: None,
        name: plain(&name),
        model: String::new(),
        task: plain(task.lines().next().unwrap_or("")),
    }
}

/// Whether pane `sid` is a helper's, at work or stopped while its pane
/// is being closed.
fn helper_pane(st: &State, sid: u64) -> bool {
    st.table.map.contains_key(&sid) || st.stopped.contains(&sid)
}

/// The shadow panes open in `obs` that are no helper's.
pub(super) fn sids(st: &State, obs: &Observation) -> Vec<u64> {
    of(obs, |sid| helper_pane(st, sid)).map(|x| x.entry.id).collect()
}

/// A row for each shadow pane no helper runs in, in the record of its
/// top pane; a pane with no helpers gets one numbered by its first
/// such pane's start.
pub(super) fn add(st: &State, obs: &Observation, m: &Memory, panes: &Panes, out: &mut Vec<HelperStrip>) {
    let above = |sid| above(obs, sid);
    for x in of(obs, |sid| helper_pane(st, sid)) {
        let sid = x.entry.id;
        let root = st.batches.top(&st.table, sid, &above);
        if root == sid || obs.session(root).is_none() {
            continue;
        }
        let row = row(x, obs, agent_at(obs, sid), panes, m.numbers.get(&sid).copied().flatten());
        match out.iter_mut().find(|r| r.root_sid == root) {
            Some(r) => {
                if r.rows.iter().all(|o| o.plain) {
                    r.batch = r.batch.min(row.started_ms.max(1));
                }
                r.rows.push(row);
            }
            None => out.push(HelperStrip { batch: row.started_ms.max(1), parent_agent: agent_at(obs, root), rows: vec![row], ..HelperStrip::empty(root) }),
        }
    }
}

#[cfg(test)]
#[path = "helper_strip_plain_tests.rs"]
mod helper_strip_plain_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_strip_plain_pane_state_follows_its_pane() {
        use BusyKind::*;
        assert_eq!(state(Some(PaneStatus::Busy(Output))), StripState::Working);
        assert_eq!(state(Some(PaneStatus::Busy(ForegroundJob))), StripState::Working);
        assert_eq!(state(Some(PaneStatus::Busy(ToolAwaitingApproval))), StripState::Waiting);
        assert_eq!(state(Some(PaneStatus::AwaitingUser)), StripState::Idle);
        assert_eq!(state(Some(PaneStatus::Empty)), StripState::Idle);
        assert_eq!(state(Some(PaneStatus::Unknown)), StripState::Preparing, "unknown is never idle");
        assert_eq!(state(None), StripState::Preparing);
    }
}

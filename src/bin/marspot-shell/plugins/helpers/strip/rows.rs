//! One row of a strip: what a helper is called and what it is doing.

use marspot::shell_proto::{StripAgent, StripRow, StripState};
use marspot_peer::control::AgentKind;

use super::super::pending::Notice;
use super::super::table::{Event, Helper};
use super::super::turn::{Turn, turn_reported};
use super::batch::Ended;
use super::used::Used;

/// What is known of a running helper besides the table.
#[derive(Clone, Copy, Default)]
pub(super) struct Live<'a> {
    pub turn: Option<Turn>,
    /// Its plugin has reported a turn.
    pub turned: bool,
    /// Its plugin reports one now.
    pub at_work: bool,
    /// A notice about it that could not be delivered.
    pub undelivered: Option<&'a Notice>,
}

fn stop_of(blocked: bool) -> StripState {
    if blocked { StripState::Waiting } else { StripState::Idle }
}

/// Whether one in this state is at work: one waiting for an answer is.
pub(super) fn works(s: StripState) -> bool {
    matches!(s, StripState::Preparing | StripState::Working | StripState::Waiting)
}

/// With its clock ended, only a sign of work since makes one working:
/// what a start of the shell takes for a turn may be the one it told.
pub(super) fn state(h: &Helper, live: Live) -> StripState {
    let resting = !h.clock.runs();
    if h.exited {
        return StripState::Exited;
    }
    if h.delivered_ms.is_none() {
        return StripState::Preparing;
    }
    if turn_reported(h) {
        return match live.turn {
            Some(Turn::BlockedTold | Turn::BlockedBefore) => StripState::Waiting,
            Some(Turn::Working | Turn::Held) if live.at_work || !resting => StripState::Working,
            Some(Turn::Working | Turn::Held) => StripState::Idle,
            // with no turn seen yet, its first is about to start
            _ if resting || live.turned || h.noticed || !h.notices.is_empty() => StripState::Idle,
            _ => StripState::Working,
        };
    }
    let untold = live.undelivered.and_then(|n| {
        n.events.iter().find_map(|e| match e {
            Event::IdleNoticed => Some(StripState::Idle),
            Event::BlockedNoticed { .. } => Some(StripState::Waiting),
            _ => None,
        })
    });
    if h.noticed {
        stop_of(h.blocked_told)
    } else if let Some(s) = untold {
        s
    } else if h.suppressed > 0 {
        stop_of(h.held_blocked)
    } else if resting {
        StripState::Idle
    } else {
        StripState::Working
    }
}

fn agent(a: AgentKind) -> StripAgent {
    match a {
        AgentKind::Claude => StripAgent::Claude,
        AgentKind::Codex => StripAgent::Codex,
        AgentKind::Gemini => StripAgent::Gemini,
        AgentKind::Kimi => StripAgent::Kimi,
    }
}

fn plain(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

pub(super) fn row(h: &Helper, state: StripState, number: Option<u16>, used: Used) -> StripRow {
    let model = if h.started.model == "default" { String::new() } else { plain(&h.started.model) };
    let label = plain(&h.started.label);
    let named = !label.is_empty();
    let name = match (named, model.is_empty()) {
        (true, _) => label,
        (false, true) => h.started.agent.as_str().to_string(),
        (false, false) => format!("{} {model}", h.started.agent.as_str()),
    };
    StripRow {
        sid: h.sid,
        number,
        state,
        agent: agent(h.started.agent),
        gone: false,
        had_tool: used.had_tool.unwrap_or(true),
        named,
        started_ms: h.started_ms,
        worked_ms: h.clock.worked_ms,
        running_since_ms: h.clock.since_ms,
        ended_ms: (!h.clock.runs()).then_some(h.clock.ended_ms),
        tokens: used.tokens,
        name,
        model,
        task: plain(&h.started.task_line),
    }
}

pub(super) fn ended_row(e: &Ended, number: Option<u16>, used: Used) -> StripRow {
    let state = if e.was_stopped() {
        StripState::Stopped
    } else if e.was_idle && !e.helper.exited {
        StripState::Idle
    } else {
        StripState::Exited
    };
    StripRow { gone: true, ..row(&e.helper, state, number, used) }
}

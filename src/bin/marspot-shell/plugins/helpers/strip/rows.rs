//! One row of a strip: what a helper is called and what it is doing.

use marspot::shell_proto::{StripAgent, StripRow, StripState};
use marspot_peer::control::AgentKind;

use super::super::pending::Notice;
use super::super::service::first_line;
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
    /// The call it is at asks the user a question of its own.
    pub question: bool,
}

fn stop_of(blocked: bool) -> StripState {
    if blocked { StripState::Waiting } else { StripState::Idle }
}

/// Whether one in this state is at work: one waiting for an answer is.
pub(super) fn works(s: StripState) -> bool {
    matches!(s, StripState::Preparing | StripState::Working | StripState::Waiting | StripState::Question)
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
            Some(Turn::BlockedTold | Turn::BlockedBefore) if live.question => StripState::Question,
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

/// What a row calls a helper: its agent and the model in use, so that it
/// never reads as one of the agent's own subagents.
fn name_of(agent: AgentKind, model: &str) -> String {
    let short = model.strip_prefix(agent.as_str()).and_then(|m| m.strip_prefix('-')).unwrap_or(model);
    if short.is_empty() { agent.as_str().to_string() } else { format!("{} {short}", agent.as_str()) }
}

/// What a row says the helper does: the caller's title for it, else
/// the name it was given, else the task: the first line of either.
fn summary_of(h: &Helper) -> String {
    let given = [&h.started.title, &h.started.label].into_iter().find(|t| !t.trim().is_empty());
    plain(&first_line(given.unwrap_or(&h.started.task_line)))
}

pub(super) fn row(h: &Helper, state: StripState, number: Option<u16>, used: Used) -> StripRow {
    // the model its records name, else the one it was started with
    let model = match used.model {
        Some(m) => m.as_str().to_string(),
        None if h.started.model == "default" => String::new(),
        None => plain(&h.started.model),
    };
    StripRow {
        sid: h.sid,
        number,
        state,
        agent: agent(h.started.agent),
        gone: false,
        had_tool: used.had_tool.unwrap_or(true),
        started_ms: h.started_ms,
        worked_ms: h.clock.worked_ms,
        running_since_ms: h.clock.since_ms,
        ended_ms: (!h.clock.runs()).then_some(h.clock.ended_ms),
        tokens: used.tokens,
        name: name_of(h.started.agent, &model),
        model,
        task: summary_of(h),
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

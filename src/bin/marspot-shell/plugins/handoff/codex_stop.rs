//! Stopping a codex thread when its pane is handed to another agent.
//!
//! Taking the TUI down does not stop codex.  The thread lives in the
//! app-server daemon, and one with a goal set starts its next turn by
//! itself: on 2026-11-10 a pane was handed to claude at 18:09 and the
//! thread behind it began another turn at 18:11, two agents now at work
//! in one repository with neither told about the other.
//!
//! So the daemon is asked: the goal is paused, not cleared — it is the
//! user's, and handing the pane back should find it there — and a turn
//! in flight is interrupted.  Paused first, or the interrupt is answered
//! with the goal's next turn.

use std::path::Path;
use std::time::Duration;

use marspot_peer::json::{obj, Value};
use marspot_peer::rpc::Rpc;

const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// What was found and done, for the handoff document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Stopped {
    pub goal_paused: bool,
    pub turn_interrupted: bool,
}

/// Pause `thread`'s goal and interrupt its running turn, through the
/// daemon of the codex home it belongs to.
pub(super) fn stop_thread(home: &Path, thread: &str) -> Result<Stopped, String> {
    let sock = home.join("app-server-control/app-server-control.sock");
    let mut rpc = Rpc::connect(&sock, "marspot-handoff", env!("CARGO_PKG_VERSION"), &[], CALL_TIMEOUT)
        .map_err(|e| format!("codex daemon: {e}"))?;
    let id = || ("threadId", Value::from(thread));
    let mut call = |method: &str, params: Value| rpc.call(method, params).map_err(|e| format!("{method}: {e}"));

    let goal = call("thread/goal/get", obj([id()]))?;
    let goal_paused = goal_is_active(&goal);
    if goal_paused {
        call("thread/goal/set", obj([id(), ("status", Value::from("paused"))]))?;
    }
    let turns = call(
        "thread/turns/list",
        obj([id(), ("limit", Value::from(1i64)), ("itemsView", Value::from("notLoaded"))]),
    )?;
    let running = running_turn(&turns);
    if let Some(turn) = &running {
        call("turn/interrupt", obj([id(), ("turnId", Value::from(turn.as_str()))]))?;
    }
    Ok(Stopped { goal_paused, turn_interrupted: running.is_some() })
}

/// `thread/goal/get`'s answer: is there a goal that would start turns?
fn goal_is_active(answer: &Value) -> bool {
    answer.at(&["goal", "status"]).and_then(Value::str) == Some("active")
}

/// `thread/turns/list`'s answer, newest first: the turn in progress.
fn running_turn(answer: &Value) -> Option<String> {
    let newest = answer.get("data")?.arr().first()?;
    (newest.str_at("status") == Some("inProgress")).then(|| newest.str_at("id").map(str::to_string))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_peer::json::parse;

    #[test]
    fn only_an_active_goal_is_one_to_pause() {
        let goal = |status: &str| parse(&format!(r#"{{"goal":{{"threadId":"t","objective":"ship","status":"{status}"}}}}"#)).unwrap();
        assert!(goal_is_active(&goal("active")));
        for s in ["paused", "blocked", "usageLimited", "budgetLimited", "complete"] {
            assert!(!goal_is_active(&goal(s)), "{s}");
        }
        assert!(!goal_is_active(&parse(r#"{"goal":null}"#).unwrap()));
    }

    #[test]
    fn the_newest_turn_is_interrupted_only_while_it_runs() {
        let turns = |status: &str| parse(&format!(r#"{{"data":[{{"id":"turn-9","status":"{status}"}}],"nextCursor":"c"}}"#)).unwrap();
        assert_eq!(running_turn(&turns("inProgress")).as_deref(), Some("turn-9"));
        for s in ["completed", "interrupted", "failed"] {
            assert_eq!(running_turn(&turns(s)), None, "{s}");
        }
        assert_eq!(running_turn(&parse(r#"{"data":[]}"#).unwrap()), None);
    }
}

//! What the receiving agent is told became of the one it takes over from.

use std::path::Path;

use super::switch::Leaving;
use super::transcript::Agent;
use super::{codex_stop, history, transcript};

/// What the receiving agent is told about the switch.  Stopping codex
/// happens here, before its TUI is taken down: the note has to say
/// what became of it.
pub(super) fn switch_note(leaving: &Leaving, history: Option<&Path>) -> transcript::Switch {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (month, day, hour, minute) = marspot::cc_usage::local_mdhm(now);
    let at = format!("{month}/{day} {hour:02}:{minute:02}");
    let thread = history.filter(|_| leaving.agent == Agent::Codex).and_then(history::codex_id_of);
    let (left, stopped) = match thread.map(|t| codex_stop::stop_thread(&leaving.home, &t)) {
        // claude and gemini are their process; so is a codex with no thread yet
        None => ("has exited".to_string(), true),
        Some(Ok(s)) => (left_codex(&s), true),
        Some(Err(e)) => (format!("has exited, but its background thread could not be reached ({e}) and may be carrying on"), false),
    };
    transcript::Switch { at, left, stopped }
}

fn left_codex(s: &codex_stop::Stopped) -> String {
    match (s.goal_paused, s.turn_interrupted) {
        (true, true) => "has exited, its goal was paused and the turn it was running was interrupted",
        (true, false) => "has exited and its goal was paused",
        (false, true) => "has exited and the turn it was running was interrupted",
        (false, false) => "has exited; it had no active goal and no turn running",
    }
    .to_string()
}

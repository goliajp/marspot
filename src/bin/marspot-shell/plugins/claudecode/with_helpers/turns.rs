//! When each pane's claude ended its turn, by its own record of it.
//!
//! Claude Code writes one `system` record, `subtype` `turn_duration`,
//! at the end of a turn and after its Stop hooks: a hook that sends it
//! back to work leaves none, and nothing in the middle of a turn writes
//! one (17 861 of them in 150 transcripts of 2.1.2xx and 2.1.3xx).  So
//! the end of a turn is known at the scan that reads it, with no wait
//! for the screen to go still: the pane's composed state reads busy for
//! half a minute after the last byte claude drew.
//!
//! Turns that did not end by claude's own choice (an API error, a
//! refused account, an interrupt) and those whose background agents are
//! still out are left to the pane's state.

use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use marspot::pidtree::ProcTable;

use super::super::transcript::{is_bookkeeping_record, record_type, top_level_str};

// process-global: its tests rely on nextest running each test in its own process
static FOUND: Mutex<(Option<Instant>, Vec<(u64, Look)>)> = Mutex::new((None, Vec::new()));

/// A scan older than this (five of their intervals) says nothing: the
/// plugin was disabled, or its worker is gone.
const STALE: Duration = Duration::from_secs(10);

/// What a pane's transcript says of its claude's last turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::plugins) enum Look {
    /// Over, by claude's own record, at `at`; `began`: its last `user`
    /// record, when that is in reach (both ms after the epoch).
    Over { began: Option<u64>, at: u64 },
    /// Under way, or over in a way left to the pane's state.
    NotOver,
}

/// What the last scan found in pane `sid`; None when no recent one could
/// say.
pub(in crate::plugins) fn look(sid: u64) -> Option<Look> {
    look_at(sid, Instant::now())
}

fn look_at(sid: u64, now: Instant) -> Option<Look> {
    let found = FOUND.lock().unwrap_or_else(|e| e.into_inner());
    found.0.filter(|t| now.saturating_duration_since(*t) < STALE)?;
    found.1.iter().find(|(s, _)| *s == sid).map(|(_, l)| *l)
}

/// What one scan found, in place of what the last one did: as many as
/// there are panes running claude.
pub(in crate::plugins) fn publish(found: Vec<(u64, Look)>) {
    *FOUND.lock().unwrap_or_else(|e| e.into_inner()) = (Some(Instant::now()), found);
}

/// What `tail` says of the last turn: the last of the transcript a pane is
/// bound to.  Only a binding claude itself named says which transcript
/// is the pane's; one guessed from the files there are may be a
/// neighbour's.
pub(in crate::plugins::claudecode) fn look_in(bound_by: &str, tail: Option<&str>) -> Option<Look> {
    matches!(bound_by, "record" | "statusline_hook").then_some(tail).flatten().and_then(look_of)
}

/// The turn the transcript ends with; None when its end is out of reach.
fn look_of(tail: &str) -> Option<Look> {
    let mut at = None;
    let mut lines = tail.lines().rev().filter(|l| !l.trim().is_empty()).take(64);
    while let Some(line) = lines.next() {
        if !is_bookkeeping_record(line) {
            let by_choice = record_type(line) == Some("assistant") && line.contains(r#""stop_reason":"end_turn""#);
            let Some(at) = at.filter(|_| by_choice) else { return Some(Look::NotOver) };
            let user = lines.find(|l| record_type(l) == Some("user"));
            let began = user.and_then(|l| top_level_str(l, "timestamp")).and_then(ms_of);
            return Some(Look::Over { began, at });
        }
        if at.is_none() && top_level_str(line, "subtype") == Some("turn_duration") {
            if agents_out(line) {
                return Some(Look::NotOver);
            }
            at = Some(ms_of(top_level_str(line, "timestamp")?)?);
        }
    }
    None
}

/// Whether the turn ended with background agents still at work: they
/// start the next one when they report.
fn agents_out(line: &str) -> bool {
    const KEY: &str = r#""pendingBackgroundAgentCount":"#;
    line.split_once(KEY).is_some_and(|(_, n)| !n.trim_start().starts_with('0'))
}

/// `2026-11-23T15:14:57.635Z` to milliseconds.
fn ms_of(iso: &str) -> Option<u64> {
    let secs = u64::try_from(marspot::cc_usage::parse_iso_utc(iso.to_string())?).ok()?;
    let frac: String = iso.get(19..)?.strip_prefix('.').unwrap_or("").chars().take_while(char::is_ascii_digit).take(3).collect();
    Some(secs * 1000 + format!("{frac:0<3}").parse::<u64>().ok()?)
}

/// Whether `claude` has a process younger than the last write to
/// `transcript`: a tool that shells out is one, while long-lived
/// children (MCP servers, started with the session) are older.
pub(in crate::plugins::claudecode) fn young_child(transcript: &Path, claude: i32, table: &ProcTable) -> bool {
    let written = std::fs::metadata(transcript).and_then(|m| m.modified()).ok();
    let written = written.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    table.descendants(claude).iter().any(|d| d.start_unix >= written)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ASKED: &str = r#"{"type":"user","message":{"role":"user","content":"go"},"timestamp":"2026-11-23T15:14:52.000Z"}"#;
    const TOOL: &str = r#"{"message":{"role":"assistant","content":[{"type":"tool_use","id":"t","name":"Bash","input":{}}],"stop_reason":"tool_use"},"type":"assistant"}"#;
    const SAID: &str = r#"{"message":{"role":"assistant","content":[{"type":"text","text":"\"subtype\":\"turn_duration\""}],"stop_reason":"end_turn"},"type":"assistant"}"#;
    const REFUSED: &str = r#"{"message":{"role":"assistant","content":[{"type":"text","text":"API Error"}],"stop_reason":"stop_sequence"},"type":"assistant"}"#;
    const HOOKS: &str = r#"{"type":"system","subtype":"stop_hook_summary","timestamp":"2026-11-23T15:14:57.632Z"}"#;
    const FEEDBACK: &str = r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"Stop hook feedback: again"}}"#;
    const OVER: &str = r#"{"isSidechain":false,"type":"system","subtype":"turn_duration","durationMs":5733,"messageCount":9,"timestamp":"2026-11-23T15:14:57.635Z","uuid":"u"}"#;
    const AFTER: &str = r#"{"type":"last-prompt","sessionId":"s"}"#;
    const AT: u64 = 1_795_446_897_635;

    const NOT_OVER: Option<Look> = Some(Look::NotOver);
    const ENDED: Option<Look> = Some(Look::Over { began: Some(AT - 5_635), at: AT });

    fn of(lines: &[&str]) -> Option<Look> {
        look_in("record", Some(&lines.join("\n")))
    }

    #[test]
    fn helpers_a_claude_turn_is_over_at_its_own_record_of_it() {
        assert_eq!(of(&[ASKED, TOOL, SAID, HOOKS, OVER, AFTER, AFTER]), ENDED);
        assert_eq!(of(&[SAID, OVER]), Some(Look::Over { began: None, at: AT }), "its start is out of reach");
        assert_eq!(of(&[AFTER, OVER, AFTER]), None, "so is its end");
        // none of these is the end of a turn
        assert_eq!(of(&[ASKED, TOOL]), NOT_OVER);
        assert_eq!(of(&[ASKED, SAID]), NOT_OVER, "its Stop hooks may send it back");
        assert_eq!(of(&[ASKED, SAID, FEEDBACK, HOOKS]), NOT_OVER, "sent back");
        assert_eq!(of(&[SAID, OVER, ASKED]), NOT_OVER, "the next turn began");
        assert_eq!(of(&[SAID, OVER, ASKED, TOOL]), NOT_OVER);
        assert_eq!(of(&[ASKED, REFUSED, OVER]), NOT_OVER, "refused: another account may go on with it");
        let out = |n: u32| OVER.replace("\"messageCount\":9", &format!("\"messageCount\":9,\"pendingBackgroundAgentCount\":{n}"));
        assert_eq!(of(&[ASKED, SAID, &out(2)]), NOT_OVER);
        assert_eq!(of(&[ASKED, SAID, &out(0)]), ENDED);
    }

    #[test]
    fn helpers_only_a_binding_claude_named_tells_its_turns() {
        let tail = [ASKED, SAID, OVER].join("\n");
        assert_eq!(look_in("statusline_hook", Some(&tail)), ENDED);
        for by in ["newest_transcript", "argv", "argv_successor", "record_overridden", "unbound"] {
            assert_eq!(look_in(by, Some(&tail)), None, "{by}");
        }
        assert_eq!(look_in("record", None), None);
    }

    #[test]
    fn helpers_a_scan_replaces_what_the_last_one_found() {
        let over = |at| Look::Over { began: None, at };
        publish(vec![(1, over(10)), (2, over(20))]);
        assert_eq!((look(1), look(2)), (Some(over(10)), Some(over(20))));
        publish(vec![(2, Look::NotOver)]);
        assert_eq!((look(1), look(2)), (None, NOT_OVER));
    }

    #[test]
    fn helpers_a_scan_says_nothing_once_the_plugin_stopped() {
        use crate::plugins::Plugin;
        publish(vec![(1, Look::NotOver)]);
        let now = Instant::now();
        assert_eq!(look_at(1, now + STALE - Duration::from_secs(1)), NOT_OVER);
        assert_eq!(look_at(1, now + STALE), None, "its worker is gone");
        crate::plugins::claudecode::ClaudecodePlugin::new().stop(&crate::plugins::host::ShellPluginHost::new());
        assert_eq!(look(1), None);
    }
}

//! Sending from outside every agent session: a scheduled job or a script
//! the person runs, which has no registry record to send for.
//!
//! Such a sender names itself, and the recipient sees that name behind
//! `external:`, so it never reads as a pane or a session.  It is refused
//! inside a session, whose messages carry the session's own name, and
//! under a name a live pane or session already has.  Everything else is
//! the session path: the same resolver, body cap and inbox write, and
//! the recipient's own rate cap.

use crate::envelope::Mode;
use crate::frame::Priority;
use crate::panes::Pane;
use crate::peers::{self, Sent};
use crate::registry::Entry;

/// What the recipient sees before the sender's own name.
pub const PREFIX: &str = "external:";
/// Longest sender name, in bytes (the charset is ASCII).
pub const MAX_NAME: usize = 40;

/// Whether `name` may stand behind `external:`.  It reaches agents as
/// part of the sender header, so it is kept to a charset that cannot end
/// that header, start a line, or pass for another attribute.
pub fn check_name(name: &str) -> Result<(), String> {
    let ok_char = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.');
    if name.is_empty() || name.len() > MAX_NAME {
        return Err(format!("--from takes a name of 1 to {MAX_NAME} characters"));
    }
    if !name.bytes().all(ok_char) || !name.as_bytes()[0].is_ascii_alphanumeric() {
        return Err(format!(
            "--from {name:?}: a sender name is letters, digits, '-', '_' and '.', starting with a letter or digit (the recipient adds {PREFIX:?} itself)"
        ));
    }
    Ok(())
}

/// The pane or session that already answers to `name`, ignoring case.
pub fn taken(name: &str, entries: &[Entry], panes: &[Pane]) -> Option<String> {
    let same = |s: &str| s.eq_ignore_ascii_case(name);
    for p in panes {
        let full = p.target().full_name();
        if same(&p.name) || same(&full) || p.base.as_deref().is_some_and(same) {
            return Some(format!("pane {full}"));
        }
    }
    let e = entries.iter().find(|e| e.name.as_deref().is_some_and(same))?;
    Some(format!("session {} (pid {})", e.name.as_deref().unwrap_or_default(), e.pid))
}

/// The agent session `pid` runs in, when it runs in one: by the variable
/// Codex or Claude Code sets for every command, else by an ancestor that
/// a registry record names (a Claude session's own pid, the agent behind
/// a Codex, Gemini or Kimi record).
pub fn session_around(
    entries: &[Entry],
    pid: i32,
    parent_of: impl Fn(i32) -> Option<i32>,
    env: impl Fn(&str) -> bool,
) -> Option<String> {
    if env("CODEX_THREAD_ID") {
        return Some("a Codex session (CODEX_THREAD_ID is set)".into());
    }
    if env("CLAUDECODE") {
        return Some("a Claude Code session (CLAUDECODE is set)".into());
    }
    let names = |p: i32| entries.iter().any(|e| e.pid == p || e.agent_pid == Some(p));
    let at = crate::proc::nearest_ancestor(pid, parent_of, names)?;
    let e = entries.iter().find(|e| e.pid == at || e.agent_pid == Some(at))?;
    Some(format!("the {} session {} (pid {at})", e.kind(), e.name.as_deref().unwrap_or("?")))
}

/// What a sender inside a session is told when it passes `--from`.
pub fn inside_text(what: &str) -> String {
    format!(
        "--from is only for senders outside every agent session, and this runs inside {what}; nothing was sent. \
         A session's messages go out under its own name: send without --from"
    )
}

/// Send `body` to `to` as `external:<name>`, after the checks above.
/// `around` is what `session_around` found for this process.
pub fn send(name: &str, around: Option<String>, entries: &[Entry], panes: &[Pane], to: &str, body: &str, priority: Priority) -> Result<Sent, String> {
    if let Some(what) = around {
        return Err(inside_text(&what));
    }
    check_name(name)?;
    if let Some(who) = taken(name, entries, panes) {
        return Err(format!("--from {name:?} is the name of {who}; an outside sender must not pass for a pane or session, so nothing was sent"));
    }
    peers::send(&sender(name), entries, panes, to, body, priority, false)
}

/// The record an outside sender stands on.  The reply address is one
/// nobody listens on: an outside sender reads no replies, and the
/// recipient's header check wants an address.  Its class is bypass: the
/// sender runs without anyone approving its steps, as the sessions
/// marspot serves do; a recipient in the prompting class holds or
/// refuses the message as it would any bypass sender's.
fn sender(name: &str) -> Entry {
    Entry {
        pid: 0,
        session_id: None,
        name: Some(format!("{PREFIX}{name}")),
        cwd: None,
        socket: Some(format!("/tmp/marspot-external-{name}.sock")),
        status: None,
        proc_start: None,
        started_at_ms: None,
        features: Vec::new(),
        ours: false,
        class: Some(Mode::Bypass),
        relay: None,
        owner: None,
        agent: None,
        agent_pid: None,
        spool: None,
    }
}

#[cfg(test)]
#[path = "external_tests.rs"]
mod tests;

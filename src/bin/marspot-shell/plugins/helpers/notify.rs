//! Telling the parent session what became of a helper: one message per
//! stop, the result and the stop together, into the parent pane's inbox
//! as it is found at the moment of sending.
//!
//! Every message starts a turn of the parent and spends its quota, so
//! there are few: a stop judged from the screen is told once until input
//! or a new result comes, and a helper sends at most three in ten
//! minutes; the rest are counted and told with the next.  The last word
//! of a helper (it exited, its budget ran out) is always told.

use std::path::Path;

use marspot_peer::control::{AgentKind, screen, sha256};
use marspot_peer::panes::Pane;
use marspot_peer::registry::Entry;

use super::table::Helper;
use crate::plugins::claudecode::with_helpers::NOTICE_HEAD;
use crate::plugins::plain_file;
use crate::plugins::usage::for_helpers;

/// The most of a result file a notice carries.
pub(super) const MAX_RESULT: usize = 256 * 1024;
/// Screen lines a stop notice carries; a budget notice twice as many.
pub(super) const SCREEN_LINES: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stop {
    /// Waiting for its next input.
    Idle,
    /// Stopped at a prompt that needs an answer.
    Blocked,
    /// Its program left; the pane is back at its shell.
    Exited,
    /// Its time ran out.
    Budget,
}

impl Stop {
    /// Whether the helper is closed with this notice.
    pub fn is_last(self) -> bool {
        matches!(self, Stop::Exited | Stop::Budget)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    Send,
    /// Over the rate: counted, told with the next.
    Hold,
    /// Told already, and nothing came since.
    Skip,
}

/// Whether a stop of `h` at `now_ms` is told.  `by_screen`: judged from
/// a quiet screen rather than reported by the agent's plugin, which
/// reports each turn's end exactly once.
pub(super) fn verdict(h: &Helper, stop: Stop, by_screen: bool, now_ms: u64) -> Verdict {
    if stop.is_last() {
        return Verdict::Send;
    }
    if by_screen && h.noticed && !h.result_new {
        return Verdict::Skip;
    }
    if !h.may_notify(now_ms) {
        return Verdict::Hold;
    }
    Verdict::Send
}

/// The last `n` non-blank-tailed lines of `text`.
pub(super) fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let end = lines.iter().rposition(|l| !l.trim().is_empty()).map_or(0, |i| i + 1);
    lines[end.saturating_sub(n)..end].join("\n")
}

/// The bottom `n` lines of a helper's screen, as any reader of a pane
/// gets it: without the status lines `pane.run` typed.
pub(super) fn screen_end(screen: &str, n: usize) -> String {
    last_lines(&crate::pane_control::run_trailer::drop_status_lines(screen), n)
}

/// What a claude helper's stop says when every account refuses its
/// model; None while one can take it.
pub(super) fn refused_note(h: &Helper, stop: Stop) -> Option<String> {
    if h.started.agent != AgentKind::Claude || h.contained() || stop.is_last() {
        return None;
    }
    let model = Some(h.started.model.as_str()).filter(|m| *m != "default");
    let r = for_helpers::claude_all_refused(model)?;
    let earliest = for_helpers::earliest_text(&r).map_or_else(|| "at a time the usage feed does not give".to_string(), |t| format!("at {t}"));
    Some(format!("All claude accounts are refusing it (out of quota); the earliest recovers {earliest}. Per account: {}.", for_helpers::recoveries_text(&r)))
}

/// The result file, cut at [`MAX_RESULT`]; None when there is none.
pub(super) fn read_result(path: &Path) -> Option<String> {
    // a helper in a container writes there
    let (bytes, len) = plain_file::read(path, MAX_RESULT as u64)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if len > MAX_RESULT as u64 {
        text.push_str(&format!("\n[cut: the file has {len} bytes, this is the first 256 KiB]"));
    }
    Some(text)
}

/// What [`prompt_sha`] puts first; a prompt written down without it,
/// or not at all, is not known.
const PROMPT_FORM: &str = "p2:";

/// What tells one prompt from another, by the prompt alone (`prompt_of`):
/// not by where its cursor is, nor by how the lines are spaced.
pub(super) fn prompt_sha(screen: &str) -> String {
    let mark = |c: char| c.is_whitespace() || matches!(c, '❯' | '›' | '→' | '▶' | '●' | '○' | '│');
    let screen = crate::pane_control::run_trailer::drop_status_lines(screen);
    format!("{PROMPT_FORM}{}", sha(&prompt_of(&screen).split(mark).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")))
}

/// Whether the prompt on `screen` is known to be the one written down as
/// `told`: one whose screen was not read, or written down another way
/// before, may be any.
pub(super) fn same_prompt(told: &str, screen: &str) -> bool {
    told.starts_with(PROMPT_FORM) && prompt_sha(screen) == told
}

/// How codex's approval prompt begins; it draws no rule above it.
const CODEX_ASKS: &str = "Would you like to ";

/// The prompt at the bottom of `screen`: from below the last rule among
/// the [`SCREEN_LINES`] rows that end at its last choice down to that
/// choice; with no rule there, from codex's question in those rows.
/// What is above (the transcript, a status that changes) and the hints
/// below are not it; with neither, those rows.
pub(super) fn prompt_of(screen: &str) -> String {
    let rows: Vec<&str> = screen.lines().collect();
    let last = rows.iter().rposition(|r| is_choice(r)).or_else(|| rows.iter().rposition(|r| !r.trim().is_empty()));
    let end = last.map_or(0, |i| i + 1);
    let near = end.saturating_sub(SCREEN_LINES);
    let below_rule = rows[near..end].iter().rposition(|r| is_rule(r)).map(|i| near + i + 1);
    let asks = |r: &&str| r.trim_start().starts_with(CODEX_ASKS) && r.trim_end().ends_with('?');
    let question = || rows[near..end].iter().rposition(asks).map(|i| near + i);
    let start = below_rule.or_else(question).unwrap_or(near);
    rows[start..end].join("\n")
}

/// Rows at the bottom of a screen that a prompt's choices are among: its
/// hints under them take a few.
const CHOICE_ROWS: usize = 8;

/// Whether a numbered choice is among the last rows drawn on `screen`,
/// as when a prompt waits for an answer.
pub(super) fn choices_at_bottom(screen: &str) -> bool {
    let screen = crate::pane_control::run_trailer::drop_status_lines(screen);
    screen.lines().rev().filter(|r| !r.trim().is_empty()).take(CHOICE_ROWS).any(is_choice)
}

/// A numbered choice, as `❯ 1. Yes` or `│   2) No │`.
fn is_choice(row: &str) -> bool {
    let bare = row.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '│' | '┃' | '❯' | '›' | '>' | '→' | '▶' | '●' | '○'));
    let digits = bare.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && bare[digits..].starts_with(['.', ')'])
}

/// A row that is a line across: a rule, or the top of a box.
fn is_rule(row: &str) -> bool {
    let t = row.trim();
    t.chars().filter(|c| matches!(c, '─' | '━' | '═')).count() >= 8 && t.chars().all(|c| matches!(c, '─' | '━' | '═' | '╭' | '╮' | '┌' | '┐' | '├' | '┤' | ' '))
}

/// What tells one result text from another.
pub(super) fn sha(text: &str) -> String {
    sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// A helper's full name as a notice may say it: its directory's name is
/// part of it, and may hold any character.
pub(super) fn shown(name: &str) -> String {
    screen::filter(name).replace('\n', " ").replace('"', "'")
}

/// The message for a stop of helper `name` (`h`), with its result file
/// at `result_path` (`result`: what it holds) and, when given, the
/// bottom of its screen, and a note on why it stopped when one is known.
pub(super) fn text(name: &str, h: &Helper, stop: Stop, result_path: &Path, result: Option<&str>, screen_text: Option<&str>, note: Option<&str>) -> String {
    let name = &shown(name);
    let who = format!("Helper \"{name}\" ({} on account {})", h.started.agent.as_str(), h.account);
    let what = match stop {
        Stop::Idle => "stopped and waits for its next input.".to_string(),
        Stop::Blocked => "stopped at a prompt that needs an answer: answer it with PaneSend, or stop it with HelperStop.".to_string(),
        Stop::Exited => "exited back to its shell (not installed, not logged in, a bad argument, or a crash); its pane was closed.".to_string(),
        Stop::Budget => format!("ran out of its {}-minute budget; its pane was closed.", h.started.budget_min),
    };
    let mut out = format!("{NOTICE_HEAD} {who} {what}");
    if h.suppressed > 0 {
        out.push_str(&format!(" It also stopped {} more time(s) that were not told one by one.", h.suppressed));
    }
    if let Some(n) = note {
        out.push(' ');
        out.push_str(n);
    }
    out.push('\n');
    match result {
        // a last stop takes the file with it, so the text is carried again
        Some(r) if h.result_sha == sha(r) && !stop.is_last() => {
            out.push_str(&format!("Its result is unchanged since the last notice; it is in {}.\n", result_path.display()));
        }
        Some(r) => {
            out.push_str(&screen::wrap_result(name, r));
            out.push('\n');
        }
        None if result_path.symlink_metadata().is_ok_and(|m| !m.is_file()) => {
            out.push_str(&format!("What it left in the place of its result file ({}) is not a regular file; it was not read.\n", result_path.display()));
        }
        None => out.push_str(&format!("It has not written its result file ({}) yet.\n", result_path.display())),
    }
    if let Some(s) = screen_text {
        out.push_str(&screen::wrap_part(name, None, Some("last lines"), s));
        out.push('\n');
    }
    if !stop.is_last() {
        let by = if h.contained() { "PaneSend" } else { "SendMessage or PaneSend" };
        out.push_str(&format!("Ask it more with {by}; stop it with HelperStop.\n"));
    }
    out.push_str(marspot_peer::notice::TRAILER);
    out
}

/// Put `body` into the inbox of pane `parent`, called `name` now; a
/// pane the names miss is looked into as `obs` shows it.
pub(super) fn deliver(parent: u64, name: &str, body: &str, obs: &crate::observe::Observation) -> Result<(), String> {
    let entries = marspot_peer::peers::live_entries();
    let panes = marspot_peer::panes::live(&entries);
    deliver_among(&entries, &panes, (parent, name), body, obs)
}

/// [`deliver`], with the sessions and pane names given.
pub(super) fn deliver_among(entries: &[Entry], panes: &[Pane], (parent, name): (u64, &str), body: &str, obs: &crate::observe::Observation) -> Result<(), String> {
    let socket = match marspot_peer::peers::resolve(entries, panes, name) {
        Ok(r) => r.socket,
        Err(e) if e.starts_with(marspot_peer::resolve::NO_PANE_NAMED) => super::inbox_of::in_pane(entries, obs, parent).ok_or(e)?,
        Err(e) => return Err(e),
    };
    let line = crate::plugins::claudecode::heads_up::frame_line(&socket, body);
    marspot_peer::inbox::write(&socket, &line)
}

#[cfg(test)]
#[path = "tests_prompt.rs"]
mod tests;

//! A Claude claim through the official CLI: a window marspot opens for
//! it, a background pane under that window running `claude` on the
//! account with an explicit model, `/limit-reset`, the result line read,
//! Esc, and both panes closed.  No pane of the user's is touched.
//!
//! Only the weekly session reset is claimed this way.  It has no
//! dialog: on an account at its five-hour limit the command uses it at
//! once, so typing the command is the claim.  Stored grants of any
//! other kind would be chosen from a list the CLI shows behind
//! `tengu_cedar_ember`, which no account has had and no screen of
//! which has been captured; until one is, a claim of any other kind is
//! `unavailable` and nothing is typed, whatever the account's flags say.

use std::path::PathBuf;
use std::time::Instant;

use marspot_peer::control::client::{self, CallError};
use marspot_peer::control::{Input, Op, OpenArgs, ReadArgs, Request, Response, SendArgs, Wait};
use marspot_peer::json::Value;

use super::claude_screen::{self, SessionReset};
use super::{Grant, Outcome, Res};
use crate::plugins::pty_command::PtyCommand;

/// A model the account surely has; the claim sends no message to it.
const MODEL: &str = "sonnet";
/// The CLI's idle footer: it is ready for a command.
const READY: &str = "? for shortcuts";

fn config_dir(profile: u8) -> PathBuf {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    if profile == 0 { home.join(".claude") } else { home.join(format!(".claude-profile-{profile}")) }
}

fn flags_of(dir: &std::path::Path) -> String {
    std::fs::read_to_string(dir.join(".claude.json")).unwrap_or_default()
}

fn ask(op: Op) -> Result<Value, String> {
    match client::call(&Request { op, caller_thread: None }) {
        Ok(Response::Ok(v)) => Ok(v),
        Ok(Response::Err(r)) => Err(format!("{}: {}", r.code.as_str(), r.message)),
        Err(CallError::Unreachable(e)) => Err(format!("the control socket is not reachable ({e})")),
        Err(CallError::Lost) => Err("the control socket connection was lost".into()),
    }
}

fn secs_left(deadline: Instant) -> u32 {
    deadline.saturating_duration_since(Instant::now()).as_secs().clamp(1, 90) as u32
}

/// The pane's screen once `wait` is met or the deadline comes.
fn read(pane: &str, wait: Wait, deadline: Instant) -> String {
    let a = ReadArgs { target: pane.into(), history_lines: 0, wait: Some(wait), since: None, timeout_s: secs_left(deadline) };
    ask(Op::Read(a)).ok().and_then(|v| v.str_at("text").map(str::to_string)).unwrap_or_default()
}

fn send(pane: &str, input: Input) -> Result<Value, String> {
    ask(Op::Send(SendArgs { target: pane.into(), input, force: true }))
}

fn keys(pane: &str, k: &[&str]) {
    let _ = send(pane, Input::Keys(k.iter().map(|s| s.to_string()).collect()));
}

/// The two panes of one claim; closing them ends it.
struct Panes {
    window: String,
    cli: Option<String>,
}

impl Drop for Panes {
    fn drop(&mut self) {
        if let Some(cli) = self.cli.take() {
            keys(&cli, &["escape"]);
            let _ = ask(Op::Close { target: cli });
        }
        let _ = ask(Op::Close { target: self.window.clone() });
        if let Ok(sid) = self.window.parse() {
            super::panes::unmark(sid);
        }
    }
}

fn open(new_window: bool, parent: Option<String>, command: Option<String>) -> Result<String, String> {
    let a = OpenArgs { cwd: std::env::var("HOME").ok(), shadow: !new_window, new_window, parent, cols: 120, rows: 40, command };
    let v = ask(Op::Open(a))?;
    v.get("id").and_then(Value::i64).map(|n| n.to_string()).ok_or_else(|| "the pane was opened with no id".into())
}

pub(super) fn claim(profile: u8, grant: &Grant, deadline: Instant) -> Outcome {
    if !marspot::agent_resets::claim_path_on("claude", &grant.kind) {
        return Outcome::of(Res::Unavailable, "marspot claims no stored Claude reset but the weekly session reset yet");
    }
    let dir = config_dir(profile);
    let session = claude_screen::session_reset(&flags_of(&dir));
    if !session.enabled {
        return Outcome::of(Res::Unavailable, "the CLI offers no session reset on this account (its cached flags say so)");
    }
    let Some(line) = PtyCommand::new("claude").env("CLAUDE_CONFIG_DIR", dir.to_string_lossy()).arg("--model").arg(MODEL).to_bytes() else {
        return Outcome::of(Res::Error, "the account's directory cannot be put on a command line");
    };
    let command = String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']).to_string();
    let mut panes = match open(true, None, None) {
        Ok(window) => Panes { window, cli: None },
        Err(e) => return Outcome::of(Res::Error, &format!("no window for the CLI: {e}")),
    };
    // marked before the CLI pane exists, so no scan sees it unmarked
    if !panes.window.parse().is_ok_and(super::panes::mark) {
        return Outcome::of(Res::Error, "the claim's window could not be kept out of account switching");
    }
    match open(false, Some(panes.window.clone()), Some(command)) {
        Ok(cli) => panes.cli = Some(cli),
        Err(e) => return Outcome::of(Res::Error, &format!("no pane for the CLI: {e}")),
    }
    let cli = panes.cli.clone().unwrap_or_default();
    drive(&cli, &session, deadline)
}

fn drive(cli: &str, session: &SessionReset, deadline: Instant) -> Outcome {
    let ready = read(cli, Wait::Text(READY.into()), deadline);
    if !ready.contains(READY) {
        let last = ready.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
        return Outcome::untried(Res::Error, &format!("the CLI did not become ready: {last}"));
    }
    if Instant::now() >= deadline {
        return Outcome::untried(Res::Error, "the claim's time ran out before the command was typed");
    }
    if let Err(e) = send(cli, Input::Text { text: "/limit-reset".into(), enter: true }) {
        return Outcome::of(Res::Error, &format!("could not type the command: {e}"));
    }
    let screen = read(cli, Wait::Quiet(1500), deadline);
    match claude_screen::result_of(&screen, session) {
        Some((result, line)) => Outcome::of(result, &line),
        None => Outcome::of(Res::Error, "the CLI printed no line this build knows after /limit-reset"),
    }
}

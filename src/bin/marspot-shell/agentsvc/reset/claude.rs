//! A Claude claim through the official CLI: a window marspot opens for
//! it, a background pane under that window running `claude` on the
//! account with an explicit model, `/limit-reset`, the confirmed grant
//! chosen where the CLI lists grants, the result line read, Esc, and
//! both panes closed.  No pane of the user's is touched.
//!
//! The weekly session reset has no dialog: on an account at its
//! five-hour limit the command uses it at once.  So the command is typed
//! only for kind `session`, or for a stored grant where the account's
//! cached flags say the CLI lists them; a listed grant is chosen only
//! when exactly one row matches the confirmed kind, count and end.

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
    }
}

fn open(new_window: bool, parent: Option<String>, command: Option<String>) -> Result<String, String> {
    let a = OpenArgs { cwd: std::env::var("HOME").ok(), shadow: !new_window, new_window, parent, cols: 120, rows: 40, command };
    let v = ask(Op::Open(a))?;
    v.get("id").and_then(Value::i64).map(|n| n.to_string()).ok_or_else(|| "the pane was opened with no id".into())
}

pub(super) fn claim(profile: u8, grant: &Grant, deadline: Instant) -> Outcome {
    let dir = config_dir(profile);
    let flags = flags_of(&dir);
    let session = claude_screen::session_reset(&flags);
    let typed_ok = match grant.kind.as_str() {
        "session" => session.enabled,
        _ => claude_screen::lists_grants(&flags),
    };
    if !typed_ok {
        return Outcome::of(Res::Unavailable, "the CLI offers no reset of this kind on this account (its cached flags say so)");
    }
    let Some(line) = PtyCommand::new("claude").env("CLAUDE_CONFIG_DIR", dir.to_string_lossy()).arg("--model").arg(MODEL).to_bytes() else {
        return Outcome::of(Res::Error, "the account's directory cannot be put on a command line");
    };
    let command = String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']).to_string();
    let mut panes = match open(true, None, None) {
        Ok(window) => Panes { window, cli: None },
        Err(e) => return Outcome::of(Res::Error, &format!("no window for the CLI: {e}")),
    };
    match open(false, Some(panes.window.clone()), Some(command)) {
        Ok(cli) => panes.cli = Some(cli),
        Err(e) => return Outcome::of(Res::Error, &format!("no pane for the CLI: {e}")),
    }
    let cli = panes.cli.clone().unwrap_or_default();
    drive(&cli, grant, &session, deadline)
}

fn drive(cli: &str, grant: &Grant, session: &SessionReset, deadline: Instant) -> Outcome {
    let ready = read(cli, Wait::Text(READY.into()), deadline);
    if !ready.contains(READY) {
        let last = ready.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
        return Outcome::of(Res::Error, &format!("the CLI did not become ready: {last}"));
    }
    if let Err(e) = send(cli, Input::Text { text: "/limit-reset".into(), enter: true }) {
        return Outcome::of(Res::Error, &format!("could not type the command: {e}"));
    }
    let screen = read(cli, Wait::Quiet(1500), deadline);
    // a list's rows carry words a result line has too: a list is read as a list
    if grant.kind != "session" && claude_screen::lists(&screen, session, super::unix_now()) {
        return choose(cli, grant, session, &screen, deadline);
    }
    match claude_screen::result_of(&screen, session) {
        Some((result, line)) => Outcome::of(result, &line),
        None => Outcome::of(Res::Error, "the CLI printed no line this build knows after /limit-reset"),
    }
}

/// Choose the confirmed grant on the CLI's list and confirm it.
fn choose(cli: &str, grant: &Grant, session: &SessionReset, screen: &str, deadline: Instant) -> Outcome {
    let Some(row) = claude_screen::row_for(screen, grant, super::unix_now()) else {
        return Outcome::of(Res::Unavailable, "the CLI does not list exactly one grant like the confirmed one; nothing was chosen");
    };
    let mut down = vec!["down"; row];
    down.push("enter");
    keys(cli, &down);
    let mut screen = read(cli, Wait::Quiet(1000), deadline);
    if claude_screen::asks_to_confirm(&screen) {
        keys(cli, &["enter"]);
        screen = read(cli, Wait::Quiet(1500), deadline);
    }
    match claude_screen::result_of(&screen, session) {
        Some((result, line)) => Outcome::of(result, &line),
        None => Outcome::of(Res::Error, "the CLI printed no result line this build knows after the choice"),
    }
}

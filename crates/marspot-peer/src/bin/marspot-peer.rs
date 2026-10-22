//! marspot-peer — send a message from a Codex session to any other
//! agent session on this machine, over the recipient's inbox socket.
//!
//! ```text
//! marspot-peer list
//! marspot-peer whoami
//! marspot-peer send [--now] <name | uds:address> <text… | ->
//! ```
//!
//! The sender is the Codex session this runs under, found by
//! `CODEX_THREAD_ID` (Codex sets it for every command it runs) in the
//! registry record marspot keeps for that session.  Without such a
//! record there is no reply address and nothing is sent: an anonymous
//! message could not be answered and could not say who it is from.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use marspot_peer::envelope::{self, Header, Mode};
use marspot_peer::frame::{self, Priority};
use marspot_peer::registry::{self, Entry};

/// Longest message body, in characters; the receiver's own cap.
const MAX_BODY: usize = 1_000_000;

const USAGE: &str = "usage:
  marspot-peer list
  marspot-peer whoami
  marspot-peer send [--now] <name | uds:address> <text... | ->";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("list") => list(),
        Some("whoami") => whoami(),
        Some("send") => send(&args[1..]),
        _ => Err(Fail::Usage),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Fail::Usage) => {
            eprintln!("{USAGE}");
            ExitCode::from(1)
        }
        Err(Fail::Msg(code, m)) => {
            eprintln!("marspot-peer: {m}");
            ExitCode::from(code)
        }
    }
}

enum Fail {
    Usage,
    Msg(u8, String),
}

fn fail<T>(code: u8, m: impl Into<String>) -> Result<T, Fail> {
    Err(Fail::Msg(code, m.into()))
}

/// Registry directories that may hold records: the Claude config dirs
/// this user has, resolved through symlinks so a shared directory is
/// read once.
fn registry_dirs() -> Vec<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let mut cands: Vec<PathBuf> = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).into_iter().collect();
    cands.push(home.join(".claude"));
    if let Ok(rd) = std::fs::read_dir(&home) {
        let mut profiles: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with(".claude-profile-")))
            .map(|e| e.path())
            .collect();
        profiles.sort();
        cands.extend(profiles);
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in cands.into_iter().filter_map(|c| c.join("sessions").canonicalize().ok()) {
        if !out.contains(&dir) {
            out.push(dir);
        }
    }
    out
}

fn live_entries() -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    for dir in registry_dirs() {
        for e in registry::list_live(&dir) {
            if !out.iter().any(|o| o.pid == e.pid) {
                out.push(e);
            }
        }
    }
    out
}

fn me(entries: &[Entry]) -> Result<&Entry, Fail> {
    let Ok(thread) = std::env::var("CODEX_THREAD_ID") else {
        return fail(2, "not running inside a Codex session (CODEX_THREAD_ID is not set)");
    };
    match entries.iter().find(|e| e.ours && e.session_id.as_deref() == Some(thread.as_str())) {
        Some(e) if e.socket.is_some() => Ok(e),
        _ => fail(
            2,
            format!("this Codex session ({thread}) has no inbox: marspot has not bound it to a pane, so it cannot send or be replied to"),
        ),
    }
}

fn list() -> Result<(), Fail> {
    let entries = live_entries();
    let mine = std::env::var("CODEX_THREAD_ID").ok();
    let mut out = std::io::stdout().lock();
    for e in &entries {
        let kind = if e.ours { "codex" } else { "claude" };
        let me = if e.ours && e.session_id == mine { "  (you)" } else { "" };
        let _ = writeln!(
            out,
            "{}\t{kind}\t{}\t{}\tuds:{}{me}",
            e.name.as_deref().unwrap_or("?"),
            e.status.as_deref().unwrap_or("?"),
            e.cwd.as_deref().unwrap_or("?"),
            e.socket.as_deref().unwrap_or("?"),
        );
    }
    Ok(())
}

fn whoami() -> Result<(), Fail> {
    let entries = live_entries();
    let e = me(&entries)?;
    println!("{}\tuds:{}", e.name.as_deref().unwrap_or("?"), e.socket.as_deref().unwrap_or_default());
    Ok(())
}

fn resolve<'a>(entries: &'a [Entry], to: &str) -> Result<(String, Option<&'a Entry>), Fail> {
    if let Some(path) = to.strip_prefix("uds:") {
        let e = entries.iter().find(|e| e.socket.as_deref() == Some(path));
        return Ok((path.to_string(), e));
    }
    let hits: Vec<&Entry> = entries.iter().filter(|e| e.name.as_deref() == Some(to)).collect();
    match hits.as_slice() {
        [e] => match &e.socket {
            Some(s) => Ok((s.clone(), Some(e))),
            None => fail(2, format!("{to} has no inbox socket")),
        },
        [] => fail(2, format!("no live session named {to:?} (see `marspot-peer list`)")),
        _ => fail(2, format!("{} live sessions are named {to:?}; use the uds: address from `marspot-peer list`", hits.len())),
    }
}

fn send(args: &[String]) -> Result<(), Fail> {
    let mut priority = Priority::Next;
    let mut rest = args;
    if rest.first().map(String::as_str) == Some("--now") {
        priority = Priority::Now;
        rest = &rest[1..];
    }
    let [to, words @ ..] = rest else { return Err(Fail::Usage) };
    if words.is_empty() {
        return Err(Fail::Usage);
    }
    let body = if words == ["-"] {
        let mut s = String::new();
        if std::io::stdin().read_to_string(&mut s).is_err() {
            return fail(1, "stdin is not UTF-8 text");
        }
        s.trim_end_matches('\n').to_string()
    } else {
        words.join(" ")
    };
    if body.trim().is_empty() {
        return fail(1, "empty message");
    }
    if body.chars().count() > MAX_BODY {
        return fail(1, format!("message longer than {MAX_BODY} characters"));
    }

    let entries = live_entries();
    let me = me(&entries)?;
    let my_socket = me.socket.clone().unwrap_or_default();
    let (target, target_entry) = resolve(&entries, to)?;
    if target == my_socket {
        return fail(2, "that address is this session's own inbox");
    }
    let from = format!("uds:{my_socket}");
    let header = Header {
        from: from.clone(),
        from_name: me.name.clone(),
        // a record without a class is reported as prompting: claiming
        // bypass for a session we cannot vouch for would get it
        // delivered where it should be held
        from_mode: Some(me.class.unwrap_or(Mode::Prompting)),
    };
    let content = envelope::encode(&header, &body);
    let msg_id = marspot_peer::rand::uuid_v4();
    let line = frame::user_line(&content, &from, priority, &msg_id);

    let mut s = match UnixStream::connect(&target) {
        Ok(s) => s,
        Err(e) => return fail(3, format!("cannot reach {to}: {e}")),
    };
    let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
    if let Err(e) = s.write_all(line.as_bytes()) {
        return fail(3, format!("cannot write to {to}: {e}"));
    }
    let name = target_entry.and_then(|e| e.name.as_deref()).unwrap_or(to);
    println!("sent {msg_id} to {name}");
    Ok(())
}

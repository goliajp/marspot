//! marspot-peer — send a message from a Codex session to any other
//! agent session on this machine, over the recipient's inbox socket.
//!
//! ```text
//! marspot-peer list
//! marspot-peer whoami
//! marspot-peer send [--now] <name | uds:address> <text… | ->
//! marspot-peer mcp
//! ```
//!
//! On the command line the sender is the Codex session this runs
//! under, found by `CODEX_THREAD_ID` (Codex sets it for every command
//! it runs).  `mcp` serves the same over stdio as MCP tools, where each
//! call names its thread instead.

use std::io::Read;
use std::process::ExitCode;

use marspot_peer::frame::Priority;
use marspot_peer::peers;
use marspot_peer::registry::Entry;

const USAGE: &str = "usage:
  marspot-peer list
  marspot-peer whoami
  marspot-peer send [--now] <name | uds:address> <text... | ->
  marspot-peer mcp";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("list") => list(),
        Some("whoami") => whoami(),
        Some("send") => send(&args[1..]),
        Some("mcp") => {
            marspot_peer::mcp::serve(std::io::stdin().lock(), std::io::stdout().lock());
            Ok(())
        }
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

fn me(entries: &[Entry]) -> Result<&Entry, Fail> {
    let Ok(thread) = std::env::var("CODEX_THREAD_ID") else {
        return Err(Fail::Msg(2, "not running inside a Codex session (CODEX_THREAD_ID is not set)".into()));
    };
    peers::me_by_thread(entries, &thread).map_err(|m| Fail::Msg(2, m))
}

fn list() -> Result<(), Fail> {
    let entries = peers::live_entries();
    let me = me(&entries).ok();
    print!("{}", peers::listing(&entries, me, now_ms()));
    Ok(())
}

fn whoami() -> Result<(), Fail> {
    let entries = peers::live_entries();
    let e = me(&entries)?;
    println!("{}\tuds:{}", e.name.as_deref().unwrap_or("?"), e.socket.as_deref().unwrap_or_default());
    Ok(())
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
            return Err(Fail::Msg(1, "stdin is not UTF-8 text".into()));
        }
        s.trim_end_matches('\n').to_string()
    } else {
        words.join(" ")
    };
    let entries = peers::live_entries();
    let me = me(&entries)?;
    let sent = peers::send(me, &entries, to, &body, priority, false).map_err(|m| Fail::Msg(3, m))?;
    println!("sent {} to {}", sent.msg_id, sent.to);
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

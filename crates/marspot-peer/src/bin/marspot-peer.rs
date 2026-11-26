//! marspot-peer — send a message from a Codex session to any other
//! agent session on this machine, over the recipient's inbox socket.
//!
//! ```text
//! marspot-peer list
//! marspot-peer whoami
//! marspot-peer send [--now] <name | uds:address> <text… | - | -- TEXT…>
//! marspot-peer mcp [--client-timeout-s N]
//! marspot-peer relay <inbox> <upstream> <send>
//! marspot-peer claude-hook
//! marspot-peer gemini-hook
//! marspot-peer kimi-hook
//! marspot-peer pane …
//! marspot-peer helper …
//! marspot-peer procs …
//! ```
//!
//! On the command line the sender is the Codex session this runs
//! under, found by `CODEX_THREAD_ID` (Codex sets it for every command
//! it runs).  `mcp` serves the same over stdio as MCP tools, where each
//! call names its thread instead.  `relay` is started by marspot, one
//! per Codex session (see `marspot_peer::relay`).  `claude-hook` is the
//! Claude Code hook that lets a Claude session use pane names
//! (`marspot_peer::claude_hook`).  `gemini-hook` is the Gemini CLI hook
//! that hands a Gemini session its waiting messages
//! (`marspot_peer::gemini_hook`), and `kimi-hook` the same for Kimi
//! Code (`marspot_peer::kimi_hook`).

use std::io::Read;
use std::process::ExitCode;

use marspot_peer::bound;
use marspot_peer::control::cli_args;
use marspot_peer::control::cli_top::{self, Top};
use marspot_peer::frame::Priority;
use marspot_peer::peers;
use marspot_peer::registry::Entry;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let top = match cli_top::parse(&args) {
        Ok(top) => top,
        Err(stop) => return ExitCode::from(cli_args::report(stop, "marspot-peer", cli_top::USAGE, cli_top::USAGE_CODE)),
    };
    let result = match top {
        Top::Control("pane", rest) => return ExitCode::from(marspot_peer::control::cli::main(&rest)),
        Top::Control("helper", rest) => return ExitCode::from(marspot_peer::control::cli_helper::main(&rest)),
        Top::Control(_, rest) => return ExitCode::from(marspot_peer::control::cli_proc::main(&rest)),
        Top::List => list(),
        Top::Whoami => whoami(),
        Top::Send { now, to, words, stdin } => send(now, &to, &words, stdin),
        Top::Mcp { client_timeout_s, ignored } => {
            if !ignored.is_empty() {
                eprintln!("marspot-peer mcp: ignoring {ignored:?}; serving as usual");
            }
            // the tool-call deadline marspot wrote into this client's own
            // config, when it wrote one
            let configured = client_timeout_s.and_then(|n| match n.parse() {
                Ok(s) => Some(s),
                Err(e) => {
                    eprintln!("marspot-peer mcp: --client-timeout-s {n:?}: {e}; using the client's default deadline");
                    None
                }
            });
            let server = marspot_peer::mcp::Server::new(configured);
            marspot_peer::mcp::serve(server, std::io::stdin().lock(), std::io::stdout());
            Ok(())
        }
        Top::ClaudeHook => {
            // a hook that fails must not get in the session's way: no
            // answer leaves the call as it was
            let mut input = String::new();
            if std::io::stdin().read_to_string(&mut input).is_ok()
                && let Some(out) = marspot_peer::claude_hook::answer(&input)
            {
                println!("{out}");
            }
            Ok(())
        }
        Top::GeminiHook => {
            // the same rule: whatever goes wrong here, the user's turn
            // goes on as if the hook were not installed
            let mut input = String::new();
            if std::io::stdin().read_to_string(&mut input).is_ok()
                && let Some(out) = marspot_peer::gemini_hook::answer(&input)
            {
                println!("{out}");
            }
            Ok(())
        }
        Top::KimiHook => {
            use marspot_peer::kimi_hook::Answer;
            let mut input = String::new();
            let answer = match std::io::stdin().read_to_string(&mut input) {
                Ok(_) => marspot_peer::kimi_hook::answer(&input),
                Err(_) => Answer::Nothing,
            };
            match answer {
                Answer::Nothing => {}
                Answer::Context(text) => println!("{text}"),
                // exit 2 is how a Stop hook keeps the turn going
                Answer::Continue(text) => {
                    eprintln!("{text}");
                    return ExitCode::from(2);
                }
            }
            Ok(())
        }
        Top::Relay { public, upstream, send } => {
            marspot_peer::relay::run(public.as_ref(), upstream.as_ref(), send.as_ref()).map_err(|e| Fail(1, format!("relay: {e}")))
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Fail(code, m)) => {
            eprintln!("marspot-peer: {m}");
            ExitCode::from(code)
        }
    }
}

struct Fail(u8, String);

fn me(entries: &[Entry]) -> Result<&Entry, Fail> {
    let Ok(thread) = std::env::var("CODEX_THREAD_ID") else {
        return Err(Fail(2, "not running inside a Codex session (CODEX_THREAD_ID is not set)".into()));
    };
    peers::me_by_thread(entries, &thread).map_err(|m| Fail(2, m))
}

fn list() -> Result<(), Fail> {
    let entries = peers::live_entries();
    let panes = marspot_peer::panes::live(&entries);
    let me = me(&entries).ok();
    print!("{}", peers::listing(&entries, &panes, me, now_ms()));
    Ok(())
}

fn whoami() -> Result<(), Fail> {
    let entries = peers::live_entries();
    let e = me(&entries)?;
    println!("{}\tuds:{}", e.name.as_deref().unwrap_or("?"), e.socket.as_deref().unwrap_or_default());
    Ok(())
}

fn send(now: bool, to: &str, words: &[String], stdin: bool) -> Result<(), Fail> {
    let priority = if now { Priority::Now } else { Priority::Next };
    let body = if stdin {
        let mut s = String::new();
        if std::io::stdin().read_to_string(&mut s).is_err() {
            return Err(Fail(1, "stdin is not UTF-8 text".into()));
        }
        s.trim_end_matches('\n').to_string()
    } else {
        words.join(" ")
    };
    if let Some(thread) = std::env::var_os("CODEX_THREAD_ID") {
        let found = || me(&peers::live_entries()).is_ok();
        let waiter = bound::Waiter::new(marspot_peer::control::socket_path(), bound::LIMIT);
        waiter.until(&thread.to_string_lossy(), &marspot_peer::registry::default_dirs(), &Default::default(), found);
    }
    let entries = peers::live_entries();
    let panes = marspot_peer::panes::live(&entries);
    let me = me(&entries)?;
    let sent = peers::send(me, &entries, &panes, to, &body, priority, false).map_err(|m| Fail(3, m))?;
    println!("sent {} to {}", sent.msg_id, sent.to);
    if let Some(note) = sent.note {
        println!("{note}");
    }
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

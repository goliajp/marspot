//! `marspot-peer pane …`: the control socket from a shell.
//!
//! Exit codes: 0 done (`pane run`: the command's own status), 1 the
//! request was refused or its outcome is unknown (the reason on
//! stderr), 2 a usage mistake, 3 no app to ask.

use std::io::Read;

use super::client::{self, CallError};
use super::{Input, Op, OpenArgs, ReadArgs, Request, Response, RunArgs, SendArgs, Wait, Where, cli_out};

pub const USAGE: &str = "usage:
  marspot-peer pane list
  marspot-peer pane open [--visible] [--parent PANE] [--cwd DIR] [--size 120x40] [-- COMMAND...]
  marspot-peer pane send PANE [--no-enter] [--force] <text... | ->
  marspot-peer pane keys PANE [--force] KEY...
  marspot-peer pane run PANE [--timeout S] [--force] COMMAND...
  marspot-peer pane read PANE [-n LINES] [--wait-text S | --wait-quiet MS | --wait-command-end
                              | --wait-prompt | --wait-change | --wait-exit] [--since POS] [--timeout S]
  marspot-peer pane resize PANE COLSxROWS
  marspot-peer pane place PANE [--force] window|shadow|peek|unpeek   (peek: shown in its parent's place)
  marspot-peer pane close PANE
  marspot-peer pane audit [-n N]
any of them takes --json for the answer as the app sent it";

/// What a shell waiting on a pane gets by default: inside a Claude Bash
/// tool call's two minutes, with room to print.
const CLI_TIMEOUT_S: u32 = 110;

/// Run `pane <args>`; the process exit code.
pub fn main(args: &[String]) -> u8 {
    let json = args.iter().any(|a| a == "--json");
    let args: Vec<String> = args.iter().filter(|a| *a != "--json").cloned().collect();
    let op = match parse(&args) {
        Ok(op) => op,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("marspot-peer pane: {e}");
            }
            eprintln!("{USAGE}");
            return 2;
        }
    };
    ask(op, json)
}

/// Send `op` and print its answer; the process exit code.
pub(super) fn ask(op: Op, json: bool) -> u8 {
    let caller_thread = std::env::var("CODEX_THREAD_ID").ok();
    let req = Request { op, caller_thread };
    match client::call(&req) {
        Err(CallError::Unreachable(e)) => {
            eprintln!("marspot-peer: cannot reach the app ({e})");
            3
        }
        Err(CallError::Lost) => {
            eprintln!("marspot-peer: the connection was lost before the answer; the result is unknown, check with `pane list` or `helper list` before doing it again");
            1
        }
        Ok(resp) if json => {
            println!("{}", resp.to_json());
            cli_out::exit_code(&req.op, &resp)
        }
        Ok(Response::Err(r)) => {
            eprintln!("{}: {}", r.code.as_str(), r.message);
            if !r.candidates.is_empty() {
                eprintln!("candidates: {}", r.candidates.join(", "));
            }
            1
        }
        Ok(resp @ Response::Ok(_)) => {
            if let Response::Ok(v) = &resp {
                cli_out::print(&req.op, v);
            }
            cli_out::exit_code(&req.op, &resp)
        }
    }
}

pub(super) fn take_value(rest: &mut Vec<String>, flag: &str) -> Result<Option<String>, String> {
    let Some(i) = rest.iter().position(|a| a == flag) else { return Ok(None) };
    if i + 1 >= rest.len() {
        return Err(format!("{flag} needs a value"));
    }
    let v = rest.remove(i + 1);
    rest.remove(i);
    Ok(Some(v))
}

pub(super) fn take_flag(rest: &mut Vec<String>, flag: &str) -> bool {
    match rest.iter().position(|a| a == flag) {
        Some(i) => {
            rest.remove(i);
            true
        }
        None => false,
    }
}

pub(super) fn number<T: std::str::FromStr>(v: Option<String>, what: &str) -> Result<Option<T>, String> {
    v.map(|s| s.parse().map_err(|_| format!("{what}: {s:?} is not a number"))).transpose()
}

fn size(s: &str) -> Result<(u16, u16), String> {
    let (c, r) = s.split_once('x').ok_or_else(|| format!("{s:?}: write the size as COLSxROWS"))?;
    Ok((c.parse().map_err(|_| format!("{c:?} is not a number"))?, r.parse().map_err(|_| format!("{r:?} is not a number"))?))
}

pub(super) fn text_or_stdin(words: &[String]) -> Result<String, String> {
    if words == ["-"] {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).map_err(|_| "stdin is not UTF-8 text".to_string())?;
        return Ok(s.trim_end_matches('\n').to_string());
    }
    Ok(words.join(" "))
}

/// The op the arguments ask for; `Err("")` for a plain usage mistake.
pub fn parse(args: &[String]) -> Result<Op, String> {
    let Some((sub, rest)) = args.split_first() else { return Err(String::new()) };
    // a command after `--` is taken whole, flags and all
    let (mut rest, tail) = match rest.iter().position(|a| a == "--") {
        Some(i) => (rest[..i].to_vec(), Some(rest[i + 1..].to_vec())),
        None => (rest.to_vec(), None),
    };
    let force = take_flag(&mut rest, "--force");
    let op = match sub.as_str() {
        "list" => Op::List,
        "open" => {
            let visible = take_flag(&mut rest, "--visible");
            let parent = take_value(&mut rest, "--parent")?;
            let cwd = take_value(&mut rest, "--cwd")?;
            let (cols, rows) = take_value(&mut rest, "--size")?.map_or(Ok((120, 40)), |s| size(&s))?;
            let command = tail.map(|t| t.join(" ")).filter(|c| !c.is_empty());
            Op::Open(OpenArgs { cwd, shadow: !visible, parent, cols, rows, command })
        }
        "send" => {
            let enter = !take_flag(&mut rest, "--no-enter");
            let [target, words @ ..] = &rest[..] else { return Err(String::new()) };
            if words.is_empty() {
                return Err(String::new());
            }
            Op::Send(SendArgs { target: target.clone(), input: Input::Text { text: text_or_stdin(words)?, enter }, force })
        }
        "keys" => {
            let [target, keys @ ..] = &rest[..] else { return Err(String::new()) };
            if keys.is_empty() {
                return Err(String::new());
            }
            Op::Send(SendArgs { target: target.clone(), input: Input::Keys(keys.to_vec()), force })
        }
        "run" => {
            let timeout_s = number(take_value(&mut rest, "--timeout")?, "--timeout")?.unwrap_or(CLI_TIMEOUT_S);
            let mut words = rest.clone();
            if let Some(t) = tail {
                words.extend(t);
            }
            let [target, cmd @ ..] = &words[..] else { return Err(String::new()) };
            if cmd.is_empty() {
                return Err(String::new());
            }
            Op::Run(RunArgs { target: target.clone(), command: cmd.join(" "), timeout_s, force })
        }
        "read" => read_op(&mut rest)?,
        "resize" => match &rest[..] {
            [target, s] => {
                let (cols, rows) = size(s)?;
                Op::Resize { target: target.clone(), cols, rows }
            }
            _ => return Err(String::new()),
        },
        "place" => match &rest[..] {
            [target, w] => Op::Place {
                target: target.clone(),
                place: match w.as_str() {
                    "window" => Where::Window,
                    "shadow" => Where::Shadow,
                    "peek" => Where::Peek,
                    "unpeek" => Where::Unpeek,
                    _ => return Err(format!("{w:?}: one of window, shadow, peek, unpeek")),
                },
                force,
            },
            _ => return Err(String::new()),
        },
        "close" => match &rest[..] {
            [target] => Op::Close { target: target.clone() },
            _ => return Err(String::new()),
        },
        "audit" => Op::Audit { limit: number(take_value(&mut rest, "-n")?, "-n")?.unwrap_or(50) },
        _ => return Err(String::new()),
    };
    Ok(op)
}

fn read_op(rest: &mut Vec<String>) -> Result<Op, String> {
    let history_lines = number(take_value(rest, "-n")?, "-n")?.unwrap_or(0);
    let since = number(take_value(rest, "--since")?, "--since")?;
    let timeout_s = number(take_value(rest, "--timeout")?, "--timeout")?.unwrap_or(CLI_TIMEOUT_S);
    let mut waits = Vec::new();
    if let Some(t) = take_value(rest, "--wait-text")? {
        waits.push(Wait::Text(t));
    }
    if let Some(ms) = number(take_value(rest, "--wait-quiet")?, "--wait-quiet")? {
        waits.push(Wait::Quiet(ms));
    }
    for (flag, w) in [
        ("--wait-command-end", Wait::CommandEnd),
        ("--wait-prompt", Wait::Prompt),
        ("--wait-change", Wait::Change),
        ("--wait-exit", Wait::Exit),
    ] {
        if take_flag(rest, flag) {
            waits.push(w);
        }
    }
    if waits.len() > 1 {
        return Err("one --wait-… at a time".into());
    }
    let [target] = &rest[..] else { return Err(String::new()) };
    Ok(Op::Read(ReadArgs { target: target.clone(), history_lines, wait: waits.pop(), since, timeout_s }))
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod pane_control_tests;

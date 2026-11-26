//! `marspot-peer pane …`: the control socket from a shell.
//!
//! Exit codes: 0 done (`pane run`: the command's own status), 1 the
//! request was refused or its outcome is unknown (the reason on
//! stderr), 2 a usage mistake, 3 no app to ask.

use std::io::Read;

use super::client::{self, CallError};
use super::cli_args::{self, Args, Spec, Stop};
use super::{Input, Op, OpenArgs, ReadArgs, Request, Response, RunArgs, SendArgs, Wait, Where, cli_out};

pub const USAGE: &str = "usage:
  marspot-peer pane list
  marspot-peer pane open [--visible] [--parent PANE] [--cwd DIR] [--size 120x40] [-- COMMAND...]
  marspot-peer pane send PANE [--no-enter] [--force] <text... | - | -- TEXT...>
  marspot-peer pane keys PANE [--force] KEY...
  marspot-peer pane run PANE [--timeout S] [--force] [--] COMMAND...   (options end at the command)
  marspot-peer pane read PANE [-n LINES] [--wait-text S | --wait-quiet MS | --wait-command-end
                              | --wait-prompt | --wait-change | --wait-exit] [--since POS] [--timeout S]
  marspot-peer pane resize PANE COLSxROWS
  marspot-peer pane place PANE [--force] window|shadow|peek|unpeek   (peek: shown in its parent's place)
  marspot-peer pane close PANE
  marspot-peer pane audit [-n N]
any of them takes --json for the answer as the app sent it, and -h / --help for this;
words after the first word of a text, task or command are taken as they are;
a first word that starts with - goes after --";

/// What a shell waiting on a pane gets by default: inside a Claude Bash
/// tool call's two minutes, with room to print.
const CLI_TIMEOUT_S: u32 = 110;

/// Run `pane <args>`; the process exit code.
pub fn main(args: &[String]) -> u8 {
    match parse_cmd(args) {
        Ok((op, json)) => ask(op, json),
        Err(stop) => cli_args::report(stop, "marspot-peer pane", USAGE, 2),
    }
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

pub(super) fn number<T: std::str::FromStr>(v: Option<String>, what: &str) -> Result<Option<T>, String> {
    v.map(|s| s.parse().map_err(|_| format!("{what}: {s:?} is not a number"))).transpose()
}

fn size(s: &str) -> Result<(u16, u16), String> {
    let (c, r) = s.split_once('x').ok_or_else(|| format!("{s:?}: write the size as COLSxROWS"))?;
    Ok((c.parse().map_err(|_| format!("{c:?} is not a number"))?, r.parse().map_err(|_| format!("{r:?} is not a number"))?))
}

/// The text the words make; a lone `-` reads it from stdin.
pub(super) fn text_or_stdin(a: &Args) -> Result<String, String> {
    if a.stdin_sign() {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).map_err(|_| "stdin is not UTF-8 text".to_string())?;
        return Ok(s.trim_end_matches('\n').to_string());
    }
    Ok(a.words().join(" "))
}

/// The op the arguments ask for.
pub fn parse(args: &[String]) -> Result<Op, Stop> {
    parse_cmd(args).map(|(op, _)| op)
}

/// The op the arguments ask for, and whether `--json` was given.
pub fn parse_cmd(args: &[String]) -> Result<(Op, bool), Stop> {
    let Some((sub, rest)) = args.split_first() else { return Err(Stop::Usage(String::new())) };
    let spec = match sub.as_str() {
        "list" | "resize" | "close" => Spec::flags(&["--json"]),
        "open" => Spec { flags: &["--json", "--visible"], values: &["--parent", "--cwd", "--size"], whole_after: None, text_after: None },
        // options stop at the text's first word: agents send any text
        "send" => Spec { flags: &["--json", "--no-enter", "--force"], values: &[], whole_after: Some(2), text_after: None },
        "keys" | "place" => Spec::flags(&["--json", "--force"]),
        // options stop at the command's first word: the rest is the command's own
        "run" => Spec { flags: &["--json", "--force"], values: &["--timeout"], whole_after: Some(2), text_after: None },
        "read" => Spec {
            flags: &["--json", "--wait-command-end", "--wait-prompt", "--wait-change", "--wait-exit"],
            values: &["-n", "--since", "--timeout", "--wait-text", "--wait-quiet"],
            whole_after: None,
            text_after: None,
        },
        "audit" => Spec { flags: &["--json"], values: &["-n"], whole_after: None, text_after: None },
        other => return Err(cli_args::unknown(other, rest)),
    };
    let a = cli_args::split(rest, &spec)?;
    Ok((op(sub, &a)?, a.flag("--json")))
}

fn op(sub: &str, a: &Args) -> Result<Op, Stop> {
    let force = a.flag("--force");
    let words = a.words();
    let op = match sub {
        "list" if words.is_empty() => Op::List,
        "open" => {
            if !a.pos.is_empty() {
                return Err(format!("{:?}: the command goes after --", a.pos[0]).into());
            }
            let (cols, rows) = a.value("--size").map_or(Ok((120, 40)), |s| size(&s))?;
            let command = a.tail.as_ref().map(|t| t.join(" ")).filter(|c| !c.is_empty());
            Op::Open(OpenArgs { cwd: a.value("--cwd"), shadow: !a.flag("--visible"), parent: a.value("--parent"), cols, rows, command })
        }
        "send" if words.len() >= 2 => {
            let text = text_or_stdin(&a.after_first())?;
            Op::Send(SendArgs { target: words[0].clone(), input: Input::Text { text, enter: !a.flag("--no-enter") }, force })
        }
        "keys" if words.len() >= 2 => Op::Send(SendArgs { target: words[0].clone(), input: Input::Keys(words[1..].to_vec()), force }),
        "run" if words.len() >= 2 => {
            let timeout_s = number(a.value("--timeout"), "--timeout")?.unwrap_or(CLI_TIMEOUT_S);
            Op::Run(RunArgs { target: words[0].clone(), command: words[1..].join(" "), timeout_s, force })
        }
        "read" => read_op(a)?,
        "resize" => match &words[..] {
            [target, s] => {
                let (cols, rows) = size(s)?;
                Op::Resize { target: target.clone(), cols, rows }
            }
            _ => return Err(Stop::Usage(String::new())),
        },
        "place" => match &words[..] {
            [target, w] => Op::Place {
                target: target.clone(),
                place: match w.as_str() {
                    "window" => Where::Window,
                    "shadow" => Where::Shadow,
                    "peek" => Where::Peek,
                    "unpeek" => Where::Unpeek,
                    _ => return Err(format!("{w:?}: one of window, shadow, peek, unpeek").into()),
                },
                force,
            },
            _ => return Err(Stop::Usage(String::new())),
        },
        "close" => match &words[..] {
            [target] => Op::Close { target: target.clone() },
            _ => return Err(Stop::Usage(String::new())),
        },
        "audit" if words.is_empty() => Op::Audit { limit: number(a.value("-n"), "-n")?.unwrap_or(50) },
        _ => return Err(Stop::Usage(String::new())),
    };
    Ok(op)
}

fn read_op(a: &Args) -> Result<Op, Stop> {
    let history_lines = number(a.value("-n"), "-n")?.unwrap_or(0);
    let since = number(a.value("--since"), "--since")?;
    let timeout_s = number(a.value("--timeout"), "--timeout")?.unwrap_or(CLI_TIMEOUT_S);
    let mut waits = Vec::new();
    if let Some(t) = a.value("--wait-text") {
        waits.push(Wait::Text(t));
    }
    if let Some(ms) = number(a.value("--wait-quiet"), "--wait-quiet")? {
        waits.push(Wait::Quiet(ms));
    }
    for (flag, w) in [
        ("--wait-command-end", Wait::CommandEnd),
        ("--wait-prompt", Wait::Prompt),
        ("--wait-change", Wait::Change),
        ("--wait-exit", Wait::Exit),
    ] {
        if a.flag(flag) {
            waits.push(w);
        }
    }
    if waits.len() > 1 {
        return Err("one --wait-… at a time".into());
    }
    let [target] = &a.words()[..] else { return Err(Stop::Usage(String::new())) };
    Ok(Op::Read(ReadArgs { target: target.clone(), history_lines, wait: waits.pop(), since, timeout_s }))
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod pane_control_tests;

//! `marspot-peer procs …`: end processes of the caller's own pane.
//! Exit codes as `pane`; a request that went through but left one of
//! its processes running (refused, not the caller's, not permitted)
//! exits 1 as well.

use super::cli::ask;
use super::cli_args::{self, Spec, Stop};
use super::request::proc::ProcStopArgs;
use super::{Op, Response};
use crate::json::Value;

pub const USAGE: &str = "usage:
  marspot-peer procs stop <pid@start_us>... [--no-tree] [--force]
    ends each process and everything under it (--no-tree: only it); --force: SIGKILL
    what still runs 5 s after SIGTERM.  Only processes of your own pane.
any of them takes --json for the answer as the app sent it, and -h / --help for this";

pub fn main(args: &[String]) -> u8 {
    match parse_cmd(args) {
        Ok((op, json)) => ask(op, json),
        Err(stop) => cli_args::report(stop, "marspot-peer procs", USAGE, 2),
    }
}

/// The op the arguments ask for.
pub fn parse(args: &[String]) -> Result<Op, Stop> {
    parse_cmd(args).map(|(op, _)| op)
}

/// The op the arguments ask for, and whether `--json` was given.
pub fn parse_cmd(args: &[String]) -> Result<(Op, bool), Stop> {
    let Some((sub, rest)) = args.split_first() else { return Err(Stop::Usage(String::new())) };
    match sub.as_str() {
        "stop" => {
            let a = cli_args::split(rest, &Spec::flags(&["--json", "--no-tree", "--force"]))?;
            let ids = a.words();
            if ids.is_empty() {
                return Err(Stop::Usage(String::new()));
            }
            Ok((Op::ProcStop(ProcStopArgs { ids, tree: !a.flag("--no-tree"), force: a.flag("--force") }), a.flag("--json")))
        }
        other => Err(cli_args::unknown(other, rest)),
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.str_at(k).unwrap_or("")
}

/// Whether every process named ended or was already gone.
pub(super) fn all_ended(v: &Value) -> bool {
    v.get("results").map(Value::arr).unwrap_or(&[]).iter().all(|r| matches!(s(r, "result"), "stopped" | "gone"))
}

pub(super) fn exit_code(resp: &Response) -> u8 {
    match resp {
        Response::Ok(v) if all_ended(v) => 0,
        _ => 1,
    }
}

pub(super) fn print(v: &Value) {
    for r in v.get("results").map(Value::arr).unwrap_or(&[]) {
        let more = r.get("signalled").map(Value::arr).map_or(0, |a| a.len().saturating_sub(1));
        let tree = if more > 0 { format!(" and {more} under it") } else { String::new() };
        let msg = s(r, "message");
        let msg = if msg.is_empty() { String::new() } else { format!(": {msg}") };
        println!("{}  {}  {}{tree}{msg}", s(r, "id"), s(r, "result"), s(r, "comm"));
    }
    if v.get("force").and_then(Value::bool) == Some(true) {
        println!("what still runs in 5 s gets SIGKILL");
    }
}

#[cfg(test)]
mod pane_control_proc_cli_tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn pane_control_procs_stop_parses() {
        let Op::ProcStop(a) = parse(&words("stop 12@34 56@78 --force")).unwrap() else { panic!() };
        assert_eq!((a.ids, a.tree, a.force), (vec!["12@34".to_string(), "56@78".to_string()], true, true));
        let Op::ProcStop(a) = parse(&words("stop --no-tree 12@34")).unwrap() else { panic!() };
        assert_eq!((a.tree, a.force), (false, false));
        assert!(parse(&words("stop")).is_err());
        assert!(parse(&words("stop 1@2 --kill")).is_err());
        assert!(parse(&words("list")).is_err());
    }

    #[test]
    fn pane_control_procs_stop_exits_one_unless_everything_ended() {
        let ok = crate::json::parse(r#"{"results":[{"result":"stopped"},{"result":"gone"}]}"#).unwrap();
        let not = crate::json::parse(r#"{"results":[{"result":"stopped"},{"result":"not_yours"}]}"#).unwrap();
        assert_eq!(exit_code(&Response::Ok(ok)), 0);
        assert_eq!(exit_code(&Response::Ok(not)), 1);
    }
}

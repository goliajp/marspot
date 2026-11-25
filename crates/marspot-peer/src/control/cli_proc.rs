//! `marspot-peer procs …`: end processes of the caller's own pane.
//! Exit codes as `pane`; a request that went through but left one of
//! its processes running (refused, not the caller's, not permitted)
//! exits 1 as well.

use super::cli::{ask, take_flag};
use super::request::proc::ProcStopArgs;
use super::{Op, Response};
use crate::json::Value;

pub const USAGE: &str = "usage:
  marspot-peer procs stop <pid@start_us>... [--no-tree] [--force]
    ends each process and everything under it (--no-tree: only it); --force: SIGKILL
    what still runs 5 s after SIGTERM.  Only processes of your own pane.
any of them takes --json for the answer as the app sent it";

pub fn main(args: &[String]) -> u8 {
    let json = args.iter().any(|a| a == "--json");
    let args: Vec<String> = args.iter().filter(|a| *a != "--json").cloned().collect();
    match parse(&args) {
        Ok(op) => ask(op, json),
        Err(e) => {
            if !e.is_empty() {
                eprintln!("marspot-peer procs: {e}");
            }
            eprintln!("{USAGE}");
            2
        }
    }
}

/// The op the arguments ask for; `Err("")` for a plain usage mistake.
pub fn parse(args: &[String]) -> Result<Op, String> {
    let Some((sub, rest)) = args.split_first() else { return Err(String::new()) };
    let mut rest = rest.to_vec();
    match sub.as_str() {
        "stop" => {
            let tree = !take_flag(&mut rest, "--no-tree");
            let force = take_flag(&mut rest, "--force");
            if rest.is_empty() {
                return Err(String::new());
            }
            if let Some(f) = rest.iter().find(|a| a.starts_with("--")) {
                return Err(format!("{f}: not an option of procs stop"));
            }
            Ok(Op::ProcStop(ProcStopArgs { ids: rest, tree, force }))
        }
        _ => Err(String::new()),
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

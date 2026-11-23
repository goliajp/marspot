//! `marspot-peer helper …`: start an agent in a shadow pane under a
//! pane, list the helpers, stop them.  Exit codes as `pane`.

use super::cli::{ask, number, take_flag, take_value, text_or_stdin};
use super::request::helper::{AgentKind, DEFAULT_BUDGET_MIN, HelperStartArgs, HelperStop, Isolation};
use super::Op;
use crate::json::Value;

pub const USAGE: &str = "usage:
  marspot-peer helper start --agent claude|codex|gemini|kimi [--parent PANE] [--model M] [--cwd DIR]
                            [--profile N] [--mode auto|ask|FLAGS] [--resume] [--budget MINUTES]
                            [--name LABEL] [--container] <task... | ->
  marspot-peer helper list [--all]
  marspot-peer helper stop <NAME | --all>
any of them takes --json for the answer as the app sent it";

pub fn main(args: &[String]) -> u8 {
    let json = args.iter().any(|a| a == "--json");
    let args: Vec<String> = args.iter().filter(|a| *a != "--json").cloned().collect();
    match parse(&args) {
        Ok(op) => ask(op, json),
        Err(e) => {
            if !e.is_empty() {
                eprintln!("marspot-peer helper: {e}");
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
        "list" => {
            let all = take_flag(&mut rest, "--all");
            if !rest.is_empty() {
                return Err(String::new());
            }
            Ok(Op::HelperList { all })
        }
        "stop" => {
            if take_flag(&mut rest, "--all") {
                return if rest.is_empty() { Ok(Op::HelperStop(HelperStop::All)) } else { Err(String::new()) };
            }
            match &rest[..] {
                [name] => Ok(Op::HelperStop(HelperStop::Name(name.clone()))),
                _ => Err(String::new()),
            }
        }
        "start" => start(&mut rest).map(Op::HelperStart),
        _ => Err(String::new()),
    }
}

fn start(rest: &mut Vec<String>) -> Result<HelperStartArgs, String> {
    let agent = take_value(rest, "--agent")?.ok_or("--agent is required")?;
    let agent = AgentKind::parse(&agent).ok_or_else(|| format!("{agent:?}: one of claude, codex, gemini, kimi"))?;
    let parent = take_value(rest, "--parent")?;
    let model = take_value(rest, "--model")?;
    let cwd = take_value(rest, "--cwd")?;
    let mode = take_value(rest, "--mode")?;
    let name = take_value(rest, "--name")?;
    let profile = number(take_value(rest, "--profile")?, "--profile")?;
    let budget_minutes = number(take_value(rest, "--budget")?, "--budget")?.unwrap_or(DEFAULT_BUDGET_MIN);
    let isolation = if take_flag(rest, "--container") { Isolation::Container } else { Isolation::Host };
    let resume_last = take_flag(rest, "--resume");
    if rest.is_empty() {
        return Err("the task is missing".into());
    }
    let task = text_or_stdin(rest)?;
    Ok(HelperStartArgs { agent, task, cwd, parent, model, profile, isolation, mode, resume_last, budget_minutes, name })
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.str_at(k).unwrap_or("-")
}

fn span(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::i64).map_or_else(|| "-".into(), |s| if s < 60 { format!("{s}s") } else { format!("{}m", s / 60) })
}

pub(super) fn print(op: &Op, v: &Value) {
    match op {
        Op::HelperStart(_) => {
            println!(
                "started {} (id {}): {} on {}, model {}, mode {}, {}",
                s(v, "name"),
                v.get("id").and_then(Value::i64).unwrap_or(0),
                s(v, "agent"),
                s(v, "account"),
                s(v, "model"),
                s(v, "mode"),
                s(v, "state")
            );
            if let Some(n) = v.str_at("note") {
                println!("note: {n}");
            }
            let others: Vec<&str> = v.get("others_in_cwd").map(Value::arr).unwrap_or(&[]).iter().filter_map(Value::str).collect();
            if !others.is_empty() {
                println!("already working in that directory: {}", others.join(", "));
            }
        }
        Op::HelperList { .. } => {
            println!("{:<20}  {:<7}  {:<9}  {:<8}  {:>5}  {:>5}  {:>5}  task", "name", "agent", "state", "account", "up", "left", "quiet");
            for h in v.get("helpers").map(Value::arr).unwrap_or(&[]) {
                println!(
                    "{:<20}  {:<7}  {:<9}  {:<8}  {:>5}  {:>5}  {:>5}  {}",
                    s(h, "name"),
                    s(h, "agent"),
                    s(h, "state"),
                    s(h, "account"),
                    span(h, "running_s"),
                    span(h, "budget_left_s"),
                    span(h, "quiet_s"),
                    s(h, "task")
                );
            }
        }
        _ => {
            let stopped: Vec<&str> = v.get("stopped").map(Value::arr).unwrap_or(&[]).iter().filter_map(Value::str).collect();
            println!("stopped: {}", if stopped.is_empty() { "none".to_string() } else { stopped.join(", ") });
            for f in v.get("failed").map(Value::arr).unwrap_or(&[]) {
                println!("not stopped: {} ({}: {})", s(f, "name"), s(f, "code"), s(f, "message"));
            }
            let closing: Vec<&str> = v.get("closing").map(Value::arr).unwrap_or(&[]).iter().filter_map(Value::str).collect();
            if !closing.is_empty() {
                println!("still closing: {}", closing.join(", "));
            }
        }
    }
}

#[cfg(test)]
mod helpers_cli_tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn helpers_cli_shows_seconds_under_a_minute() {
        let v = crate::json::parse(r#"{"a":0,"b":59,"c":60,"d":1799,"e":null}"#).unwrap();
        assert_eq!(["a", "b", "c", "d", "e", "f"].map(|k| span(&v, k)), ["0s", "59s", "1m", "29m", "-", "-"]);
    }

    #[test]
    fn helpers_cli_parses_the_three_commands() {
        let Op::HelperStart(a) = parse(&words("start --agent codex --budget 5 --resume --profile 2 fix the bug")).unwrap() else { panic!() };
        assert_eq!((a.agent, a.budget_minutes, a.resume_last, a.profile, a.task.as_str()), (AgentKind::Codex, 5, true, Some(2), "fix the bug"));
        assert_eq!(parse(&words("list --all")).unwrap(), Op::HelperList { all: true });
        assert_eq!(parse(&words("stop --all")).unwrap(), Op::HelperStop(HelperStop::All));
        assert_eq!(parse(&words("stop spg#1")).unwrap(), Op::HelperStop(HelperStop::Name("spg#1".into())));
        assert!(parse(&words("start fix")).is_err());
        assert!(parse(&words("start --agent codex")).is_err());
        assert!(parse(&words("stop")).is_err());
    }
}

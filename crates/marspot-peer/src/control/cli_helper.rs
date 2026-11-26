//! `marspot-peer helper …`: start an agent in a shadow pane under a
//! pane, list the helpers, stop them.  Exit codes as `pane`.

use super::cli::{ask, number, text_or_stdin};
use super::cli_args::{self, Args, Spec, Stop};
use super::request::helper::{AgentKind, DEFAULT_BUDGET_MIN, HelperStartArgs, HelperStop, Isolation};
use super::Op;
use crate::json::Value;

pub const USAGE: &str = "usage:
  marspot-peer helper start --agent claude|codex|gemini|kimi [--parent PANE] [--model M] [--cwd DIR]
                            [--profile N] [--mode auto|ask|FLAGS] [--resume] [--budget MINUTES]
                            [--name LABEL] [--title WORDS] [--container] <task... | - | -- TASK...>
  marspot-peer helper list [--all]
  marspot-peer helper stop <NAME | --all>
any of them takes --json for the answer as the app sent it, and -h / --help for this;
words after the first word of a text, task or command are taken as they are;
a first word that starts with - goes after --";

pub fn main(args: &[String]) -> u8 {
    match parse_cmd(args) {
        Ok((op, json)) => ask(op, json),
        Err(stop) => cli_args::report(stop, "marspot-peer helper", USAGE, 2),
    }
}

/// The op the arguments ask for.
pub fn parse(args: &[String]) -> Result<Op, Stop> {
    parse_cmd(args).map(|(op, _)| op)
}

/// The op the arguments ask for, and whether `--json` was given.
pub fn parse_cmd(args: &[String]) -> Result<(Op, bool), Stop> {
    let Some((sub, rest)) = args.split_first() else { return Err(Stop::Usage(String::new())) };
    let spec = match sub.as_str() {
        "list" | "stop" => Spec::flags(&["--json", "--all"]),
        "start" => Spec {
            flags: &["--json", "--container", "--resume"],
            values: &["--agent", "--parent", "--model", "--cwd", "--mode", "--name", "--title", "--profile", "--budget"],
            // options stop at the task's first word
            whole_after: Some(1),
            text_after: None,
        },
        other => return Err(cli_args::unknown(other, rest)),
    };
    let a = cli_args::split(rest, &spec)?;
    let words = a.words();
    let all = a.flag("--all");
    let op = match (sub.as_str(), &words[..]) {
        ("list", []) => Op::HelperList { all },
        ("stop", []) if all => Op::HelperStop(HelperStop::All),
        ("stop", [name]) if !all => Op::HelperStop(HelperStop::Name(name.clone())),
        ("start", _) => Op::HelperStart(start(&a)?),
        _ => return Err(Stop::Usage(String::new())),
    };
    Ok((op, a.flag("--json")))
}

fn start(a: &Args) -> Result<HelperStartArgs, Stop> {
    let agent = a.value("--agent").ok_or("--agent is required")?;
    let agent = AgentKind::parse(&agent).ok_or_else(|| format!("{agent:?}: one of claude, codex, gemini, kimi"))?;
    let profile = number(a.value("--profile"), "--profile")?;
    let budget_minutes = number(a.value("--budget"), "--budget")?.unwrap_or(DEFAULT_BUDGET_MIN);
    let isolation = if a.flag("--container") { Isolation::Container } else { Isolation::Host };
    if a.words().is_empty() {
        return Err("the task is missing".into());
    }
    let task = text_or_stdin(a)?;
    Ok(HelperStartArgs {
        agent,
        task,
        cwd: a.value("--cwd"),
        parent: a.value("--parent"),
        model: a.value("--model"),
        profile,
        isolation,
        mode: a.value("--mode"),
        resume_last: a.flag("--resume"),
        budget_minutes,
        name: a.value("--name"),
        title: a.value("--title"),
    })
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
        let Op::HelperStart(a) = parse(&words("start --agent claude --title fix-parser --name p fix it")).unwrap() else { panic!() };
        assert_eq!((a.title.as_deref(), a.name.as_deref(), a.task.as_str()), (Some("fix-parser"), Some("p"), "fix it"));
        assert_eq!(parse(&words("list --all")).unwrap(), Op::HelperList { all: true });
        assert_eq!(parse(&words("stop --all")).unwrap(), Op::HelperStop(HelperStop::All));
        assert_eq!(parse(&words("stop spg#1")).unwrap(), Op::HelperStop(HelperStop::Name("spg#1".into())));
        assert!(parse(&words("start fix")).is_err());
        assert!(parse(&words("start --agent codex")).is_err());
        assert!(parse(&words("stop")).is_err());
    }
}

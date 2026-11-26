//! `marspot-peer reset …`: claim a stored reset of an agent account, or
//! list what is known of every account's resets.
//!
//! `claim` always ends with one JSON line on stdout, whatever happened,
//! so a caller over ssh reads the outcome from the last line: `result`,
//! `request_id` (the one sent), `left_after`, `observed_after`, `detail`.
//! Exit codes: 0 the reset was used, 1 any other outcome, 2 a usage
//! mistake, 3 no app to ask.

use super::client::{self, CallError};
use super::cli_args::{self, Spec, Stop};
use super::{Op, Request, ResetClaimArgs, Response};
use crate::json::{Value, obj};

pub const USAGE: &str = "usage:
  marspot-peer reset claim --provider claude|codex --profile N --kind KIND --grant-ref REF
                           --request-id UUID --confirm TOKEN [--reason TEXT]
    uses one stored reset of that account once the confirmation checks out;
    the same request id asked again gives the same answer and claims nothing more
  marspot-peer reset status [--json]
    what is known of every account's stored resets
-h / --help prints this and does nothing else";

/// How long a claim may take in the app, and some: waiting for the
/// account to be idle, then the claim itself.
pub const CLAIM_WITHIN_S: u32 = 240;

const VALUES: &[&str] = &["--provider", "--profile", "--kind", "--grant-ref", "--request-id", "--confirm", "--reason"];

pub fn main(args: &[String]) -> u8 {
    match parse(args) {
        Ok((op, json)) => run(op, json),
        Err(stop) => cli_args::report(stop, "marspot-peer reset", USAGE, 2),
    }
}

/// The op the arguments ask for, and whether `--json` was given.
pub fn parse(args: &[String]) -> Result<(Op, bool), Stop> {
    let Some((sub, rest)) = args.split_first() else { return Err(Stop::Usage(String::new())) };
    match sub.as_str() {
        "claim" => {
            let a = cli_args::split(rest, &Spec { flags: &[], values: VALUES, whole_after: None, text_after: None })?;
            if !a.words().is_empty() {
                return Err(Stop::Usage("claim takes no words besides its options".into()));
            }
            let need = |k: &str| a.value(k).ok_or_else(|| Stop::Usage(format!("{k} is required")));
            let profile = need("--profile")?.parse::<u8>().map_err(|_| Stop::Usage("--profile is a number from 0 to 255".into()))?;
            let c = ResetClaimArgs {
                provider: need("--provider")?,
                profile,
                kind: need("--kind")?,
                grant_ref: need("--grant-ref")?,
                request_id: need("--request-id")?,
                confirm: need("--confirm")?,
                reason: a.value("--reason").unwrap_or_default(),
            };
            Ok((Op::ResetClaim(c), true))
        }
        "status" => {
            let a = cli_args::split(rest, &Spec::flags(&["--json"]))?;
            if !a.words().is_empty() {
                return Err(Stop::Usage(String::new()));
            }
            Ok((Op::ResetStatus, a.flag("--json")))
        }
        other => Err(cli_args::unknown(other, rest)),
    }
}

/// The last line a claim prints when the app gave no answer of its own.
pub fn failed(request_id: &str, result: &str, detail: &str) -> Value {
    obj([
        ("result", result.into()),
        ("request_id", request_id.into()),
        ("left_after", Value::Null),
        ("observed_after", Value::Null),
        ("detail", detail.into()),
    ])
}

fn run(op: Op, json: bool) -> u8 {
    let id = match &op {
        Op::ResetClaim(c) => Some(c.request_id.clone()),
        _ => None,
    };
    let req = Request { op, caller_thread: std::env::var("CODEX_THREAD_ID").ok() };
    let answer = client::call(&req);
    let Some(id) = id else { return status(answer, json) };
    let (line, code) = match answer {
        Err(CallError::Unreachable(e)) => (failed(&id, "error", &format!("cannot reach the app ({e})")), 3),
        Err(CallError::Lost) => (failed(&id, "error", "the connection was lost; ask again with the same request id"), 1),
        Ok(Response::Err(r)) => (failed(&id, "error", &format!("{}: {}", r.code.as_str(), r.message)), 1),
        Ok(Response::Ok(v)) => {
            let code = if v.str_at("result") == Some("reset") { 0 } else { 1 };
            (v, code)
        }
    };
    println!("{}", line.to_json());
    code
}

fn status(answer: Result<Response, CallError>, json: bool) -> u8 {
    let v = match answer {
        Err(CallError::Unreachable(e)) => {
            eprintln!("marspot-peer: cannot reach the app ({e})");
            return 3;
        }
        Err(CallError::Lost) => {
            eprintln!("marspot-peer: the connection was lost before the answer");
            return 1;
        }
        Ok(Response::Err(r)) => {
            eprintln!("{}: {}", r.code.as_str(), r.message);
            return 1;
        }
        Ok(Response::Ok(v)) => v,
    };
    if json {
        println!("{}", v.to_json());
        return 0;
    }
    for a in v.get("accounts").map(Value::arr).unwrap_or(&[]) {
        let r = a.get("resets");
        let left: i64 = r.and_then(|r| r.get("grants")).map(Value::arr).unwrap_or(&[]).iter().filter_map(|g| g.get("left")?.i64()).sum();
        let source = r.and_then(|r| r.str_at("source")).unwrap_or("-");
        let last = r.and_then(|r| r.get("last_reset")).and_then(|l| Some(format!("{} by {}", l.str_at("result")?, l.str_at("by")?)));
        println!("{} {}  {source}  left={left}  last={}", a.str_at("provider").unwrap_or("?"), a.get("profile").and_then(Value::i64).unwrap_or(0), last.unwrap_or_else(|| "-".into()));
    }
    0
}

#[cfg(test)]
mod pane_control_reset_cli_tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    const CLAIM: &str = "claim --provider codex --profile 1 --kind codex_full --grant-ref codex:1:g2 --request-id 0b6a7c1e-3f1d --confirm tok --reason because";

    #[test]
    fn pane_control_reset_claim_parses_every_option() {
        let (Op::ResetClaim(c), true) = parse(&words(CLAIM)).unwrap() else { panic!() };
        assert_eq!((c.provider.as_str(), c.profile, c.kind.as_str(), c.grant_ref.as_str()), ("codex", 1, "codex_full", "codex:1:g2"));
        assert_eq!((c.request_id.as_str(), c.confirm.as_str(), c.reason.as_str()), ("0b6a7c1e-3f1d", "tok", "because"));
        assert!(parse(&words(&CLAIM.replace("--confirm tok ", ""))).is_err(), "no token, no claim");
        assert!(parse(&words(&CLAIM.replace("--profile 1", "--profile x"))).is_err());
        assert!(parse(&words(&format!("{CLAIM} extra"))).is_err());
    }

    #[test]
    fn pane_control_reset_help_anywhere_asks_nothing() {
        for line in [format!("{CLAIM} --help"), format!("-h {CLAIM}"), "status --help".into(), "--help".into(), "claim -h".into()] {
            assert_eq!(parse(&words(&line)).unwrap_err(), Stop::Help, "{line}");
        }
    }

    #[test]
    fn pane_control_reset_failure_line_carries_the_request_id() {
        let v = failed("abc-1", "error", "cannot reach the app");
        assert_eq!(v.to_json(), r#"{"result":"error","request_id":"abc-1","left_after":null,"observed_after":null,"detail":"cannot reach the app"}"#);
    }
}

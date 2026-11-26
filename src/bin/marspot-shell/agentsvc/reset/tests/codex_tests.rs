//! The consume step against the app-server's own field names (Codex
//! 0.172.0: `outcome` is `reset`, `nothingToReset`, `noCredit` or
//! `alreadyRedeemed`), and a whole claim against a stand-in app-server
//! process that answers from recorded limits.  No real credit is used.

use std::time::{Duration, Instant};

use marspot_peer::json;

use super::super::codex::{below_usable, claim, consumed, pick};
use super::super::{Grant, Res};

const RL_1: &str = include_str!("../../../plugins/usage/fixtures/codex-1-rate-limits.json");
const ANSWERS: &str = include_str!("../fixtures/codex-consume-answers.jsonl");

fn second_credit() -> Grant {
    Grant { kind: "codex_full".into(), grant_ref: "codex:1:g2".into(), starts_at: Some(1_793_573_597), ends_at: Some(1_796_165_597), left: Some(1) }
}

fn with_week_used(percent: u32) -> String {
    RL_1.replace(r#""usedPercent":2,"#, &format!(r#""usedPercent":{percent},"#))
}

fn redeem_second(rl: &str) -> String {
    rl.replacen(r#""status":"available","grantedAt":1793573597"#, r#""status":"redeemed","grantedAt":1793573597"#, 1)
}

#[test]
fn reset_codex_the_credit_is_found_by_its_start_and_end() {
    let limits = json::parse(RL_1).unwrap();
    assert_eq!(pick(&limits, &second_credit()).unwrap(), "RateLimitResetCredit_redacted2");
    let only_end = Grant { starts_at: None, ..second_credit() };
    assert_eq!(pick(&limits, &only_end).unwrap(), "RateLimitResetCredit_redacted2");
    let unknown = Grant { ends_at: Some(1), ..second_credit() };
    assert_eq!(pick(&limits, &unknown).unwrap_err().result, Res::Unavailable);
    let blind = Grant { starts_at: None, ends_at: None, ..second_credit() };
    assert_eq!(pick(&limits, &blind).unwrap_err().result, Res::Unavailable, "no credit is guessed");
    let redeemed = json::parse(&redeem_second(RL_1)).unwrap();
    assert_eq!(pick(&redeemed, &second_credit()).unwrap_err().result, Res::AlreadyUsed);
}

#[test]
fn reset_codex_a_limit_with_80_percent_left_cannot_be_brought_back() {
    assert!(!below_usable(&json::parse(RL_1).unwrap()), "the week is 2% used");
    assert!(!below_usable(&json::parse(&with_week_used(20)).unwrap()), "exactly 80% left");
    assert!(below_usable(&json::parse(&with_week_used(21)).unwrap()));
}

#[test]
fn reset_codex_each_consume_outcome_maps_to_its_result() {
    let got: Vec<Res> = ANSWERS.lines().map(|l| consumed(&json::parse(l).unwrap()).result).collect();
    assert_eq!(got, [Res::Reset, Res::NotLimited, Res::NoCredit, Res::AlreadyUsed, Res::Error]);
    let odd = consumed(&json::parse(r#"{"id":"consume","result":{"outcome":"later"}}"#).unwrap());
    assert_eq!(odd.result, Res::Error);
}

/// A stand-in `codex app-server`: answers each request by its id from
/// files beside it and writes the consume request it got down.
fn stand_in(dir: &std::path::Path, before: &str, after: &str, consume: &str) -> std::path::PathBuf {
    std::fs::write(dir.join("rl.json"), before.trim()).unwrap();
    std::fs::write(dir.join("rl2.json"), after.trim().replace(r#""id":"rl""#, r#""id":"rl2""#)).unwrap();
    std::fs::write(dir.join("consume.json"), consume).unwrap();
    let script = format!(
        "#!/bin/sh\nD='{}'\nwhile IFS= read -r line; do\n  case \"$line\" in\n    *'\"id\":\"rl2\"'*) cat \"$D/rl2.json\"; echo;;\n    *'\"id\":\"rl\"'*) cat \"$D/rl.json\"; echo;;\n    *'\"id\":\"consume\"'*) printf '%s\\n' \"$line\" >> \"$D/asked\"; cat \"$D/consume.json\"; echo;;\n  esac\ndone\n",
        dir.display()
    );
    let bin = dir.join("codex");
    std::fs::write(&bin, script).unwrap();
    let mut p = std::fs::metadata(&bin).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut p, 0o755);
    std::fs::set_permissions(&bin, p).unwrap();
    bin
}

#[test]
fn reset_codex_a_claim_consumes_the_confirmed_credit_once_with_the_request_id() {
    let dir = marspot_term::test_env::TempDir::new("reset-codex-claim");
    let before = with_week_used(64);
    let after = redeem_second(&before).replace(r#""availableCount":4"#, r#""availableCount":3"#);
    let bin = stand_in(dir.path(), &before, &after, ANSWERS.lines().next().unwrap());
    let deadline = Instant::now() + Duration::from_secs(20);
    let o = claim(&bin, dir.path(), "codex:1", &second_credit(), "ee-1", deadline);
    assert_eq!((o.result, o.detail.as_str(), o.left_after), (Res::Reset, "reset", Some(3)), "{o:?}");
    let block = o.observed_after.expect("the limits read afterwards");
    assert_eq!(block.str_at("source"), Some("app_server"));
    assert!(!block.to_json().contains("redacted"), "no credit id leaves the claim");
    let asked = std::fs::read_to_string(dir.join("asked")).unwrap();
    assert_eq!(asked.lines().count(), 1);
    let req = json::parse(asked.trim()).unwrap();
    assert_eq!(req.str_at("method"), Some("account/rateLimitResetCredit/consume"));
    assert_eq!(req.at(&["params", "creditId"]).and_then(json::Value::str), Some("RateLimitResetCredit_redacted2"));
    assert_eq!(req.at(&["params", "idempotencyKey"]).and_then(json::Value::str), Some("ee-1"));

    // a week with 80% left is never consumed
    let dir = marspot_term::test_env::TempDir::new("reset-codex-full");
    let bin = stand_in(dir.path(), RL_1, RL_1, ANSWERS.lines().next().unwrap());
    let o = claim(&bin, dir.path(), "codex:1", &second_credit(), "ee-2", deadline);
    assert_eq!(o.result, Res::NotLimited);
    assert!(!dir.join("asked").exists(), "nothing was asked to consume");
}

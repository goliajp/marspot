//! A confirmed request whose consume may have gone through keeps its
//! confirmation through every later attempt that sent nothing, and for
//! ten minutes at most.  Nothing here is claimed: the app-servers are
//! stand-ins that never consume.

use std::time::{Duration, Instant};

use crate::plugins::codex::reset_claim as codex;
use super::super::ledger::{AGAIN_FOR_S, Ledger};
use super::*;

/// A request whose consume got no answer, written down `ago` seconds
/// before now, on Codex account `profile`.
fn lost_consume(l: &Arc<Mutex<Ledger>>, profile: u8, id: &str, ago: i64) -> Claim {
    let mut c = claim(Provider::Codex, profile, id);
    c.grant.ends_at = Some(1_796_165_597);
    let lost = Outcome::again(Res::Error, "the app-server did not answer the consume request");
    l.lock().unwrap().record(&c, &lost, unix_now() - ago).unwrap();
    c
}

fn still_confirmed(l: &Arc<Mutex<Ledger>>, c: &Claim) -> bool {
    l.lock().unwrap().confirmed_before(c, unix_now()).is_some() && l.lock().unwrap().answered(&c.request_id).is_none()
}

fn never_checked(_: &Claim) -> Result<Grant, Outcome> {
    panic!("a request confirmed before is not checked again")
}

/// An executable at `dir/name` running `body` under `sh`.
fn script(dir: &marspot_term::test_env::TempDir, name: &str, body: &str) -> std::path::PathBuf {
    let bin = dir.join(name);
    std::fs::write(&bin, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut p = std::fs::metadata(&bin).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut p, 0o755);
    std::fs::set_permissions(&bin, p).unwrap();
    bin
}

#[test]
fn reset_a_busy_attempt_leaves_a_retry_s_confirmation_alone() {
    let dir = marspot_term::test_env::TempDir::new("reset-again-busy");
    let l = ledger(&dir);
    let c = lost_consume(&l, 11, "ra-1", 0);
    let held = Ledger::hold(&l, &claim(Provider::Codex, 11, "ra-other")).expect("free");
    let o = run(&l, c.clone(), never_checked);
    assert_eq!(o.result, Res::Busy);
    drop(held);
    assert!(still_confirmed(&l, &c), "busy never ran, so the retry is still owed");
}

#[test]
fn reset_a_retry_that_waits_out_a_working_account_may_be_retried() {
    let dir = marspot_term::test_env::TempDir::new("reset-again-account-busy");
    let l = ledger(&dir);
    super::super::idle::publish("codex", [(12, false)]);
    let c = lost_consume(&l, 12, "rb-1", 0);
    let o = run_within(&l, c.clone(), CLI_WITHIN + Duration::from_millis(50), never_checked);
    assert_eq!((o.result, o.again), (Res::AccountBusy, true), "{o:?}");
    assert!(still_confirmed(&l, &c));

    // a first attempt that waited it out is answered: nothing is owed
    let fresh = run_within(&l, claim(Provider::Codex, 12, "rb-2"), CLI_WITHIN + Duration::from_millis(50), |c| Ok(c.grant.clone()));
    assert_eq!((fresh.result, fresh.again), (Res::AccountBusy, false));
    assert!(l.lock().unwrap().answered("rb-2").is_some());
}

#[test]
fn reset_a_retry_whose_time_ran_out_before_anything_was_tried_may_be_retried() {
    let dir = marspot_term::test_env::TempDir::new("reset-again-late");
    let l = ledger(&dir);
    let c = lost_consume(&l, 13, "rc-1", 0);
    let o = run_within(&l, c.clone(), Duration::ZERO, never_checked);
    assert_eq!((o.result, o.detail.as_str(), o.again), (Res::Error, "the claim's time ran out before anything was tried", true));
    assert!(still_confirmed(&l, &c));
}

#[test]
fn reset_a_retry_the_app_server_never_answered_may_be_retried() {
    let dir = marspot_term::test_env::TempDir::new("reset-again-silent");
    let l = ledger(&dir);
    let c = lost_consume(&l, 14, "rd-1", 0);
    let silent = script(&dir, "codex-silent", "exec cat > /dev/null");
    let deadline = Instant::now() + Duration::from_millis(300);
    let act = |c: &Claim, _: Instant| codex::claim(&silent, dir.path(), "codex:14", &c.grant, &c.request_id, deadline);
    let o = run_with(&l, c.clone(), Duration::from_secs(5), never_checked, act);
    assert_eq!((o.result, o.detail.as_str(), o.again), (Res::Error, "the app-server did not answer the limits request", true));
    assert!(still_confirmed(&l, &c));

    let missing = dir.join("no-such-codex");
    let act = |c: &Claim, _: Instant| codex::claim(&missing, dir.path(), "codex:14", &c.grant, &c.request_id, deadline);
    let o = run_with(&l, c.clone(), Duration::from_secs(5), never_checked, act);
    assert_eq!((o.result, o.again), (Res::Error, true), "{o:?}");
    assert!(still_confirmed(&l, &c));

    // an attempt that got an answer ends it
    let act = |_: &Claim, _: Instant| Outcome::of(Res::AlreadyUsed, "alreadyRedeemed");
    assert_eq!(run_with(&l, c.clone(), Duration::from_secs(5), never_checked, act).result, Res::AlreadyUsed);
    assert!(l.lock().unwrap().confirmed_before(&c, unix_now()).is_none());
    assert_eq!(l.lock().unwrap().answered("rd-1").map(|r| r.outcome.result), Some(Res::AlreadyUsed));
}

#[test]
fn reset_a_confirmation_to_retry_lasts_ten_minutes_from_the_first_attempt() {
    let dir = marspot_term::test_env::TempDir::new("reset-again-expiry");
    let l = ledger(&dir);
    let old = lost_consume(&l, 15, "re-1", AGAIN_FOR_S + 1);
    assert!(l.lock().unwrap().confirmed_before(&old, unix_now()).is_none(), "past ten minutes it must be confirmed anew");
    let recent = lost_consume(&l, 16, "re-2", AGAIN_FOR_S - 30);
    assert!(still_confirmed(&l, &recent));

    // marked again later, it still counts from the first confirmation
    let lost = Outcome::again(Res::Error, "no answer");
    l.lock().unwrap().record(&recent, &lost, unix_now()).unwrap();
    assert!(l.lock().unwrap().confirmed_before(&recent, unix_now() + 31).is_none());
    assert!(l.lock().unwrap().confirmed_before(&recent, unix_now() + 20).is_some());
}

#[test]
fn reset_a_claude_kind_without_a_claim_path_is_refused_before_the_account_is_held() {
    let dir = marspot_term::test_env::TempDir::new("reset-no-path");
    let l = ledger(&dir);
    let held = Ledger::hold(&l, &claim(Provider::Claude, 17, "rf-other")).expect("free");
    let mut c = claim(Provider::Claude, 17, "rf-1");
    c.grant.kind = "claude_full".into();
    let t = Instant::now();
    let o = run(&l, c, |_| panic!("not checked: there is nothing to claim it with"));
    assert_eq!(o.result, Res::Unavailable, "not busy and not a wait for the account: {o:?}");
    assert!(t.elapsed() < Duration::from_secs(5));
    drop(held);
}

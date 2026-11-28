//! The credits facts read off the rollouts: a spend for each drop of the
//! balance once the week is used up, a drop nothing explains otherwise,
//! none for a balance that does not read, and none twice.
//!
//! `codex-credits-cross.jsonl` is the real record of one pane's account
//! going from 99% to 100% of its week on 2026-11-27 and then spending:
//! its `token_count` lines unchanged, a cut-down `turn_context` first.

use super::alloc::{CodexAlloc, PaneIn, Snapshot};
use super::alloc_tests::{H, accounts, append, iso, rows, world};
use crate::agentsvc::accounts::wallet;

const CROSS: &str = include_str!("fixtures/codex-credits-cross.jsonl");
const RENEWS: i64 = 1_796_051_701;
/// Just after the fixture's last line.
const AFTER: i64 = 1_795_820_063;

fn pane(rollout: &std::path::Path, started_at: i64) -> PaneIn {
    PaneIn { sid: 487, profile: 2, started_at, rollout: Some(rollout.to_path_buf()), idle: false, helper: false, restricted: false, peer: Default::default() }
}

fn snap(rollout: &std::path::Path, now: i64, started_at: i64) -> Snapshot {
    Snapshot { now, feed_at: Some(now), accounts: accounts(now), panes: vec![pane(rollout, started_at)] }
}

fn reading(at: i64, used_percent: f64, balance: &str) -> String {
    format!(
        r#"{{"timestamp":"{}","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":1000,"cached_input_tokens":0,"output_tokens":10}}}},"rate_limits":{{"primary":{{"used_percent":{used_percent},"window_minutes":10080,"resets_at":{}}},"secondary":null,"credits":{{"has_credits":true,"unlimited":false,"balance":{balance}}},"individual_limit":null,"spend_control_reached":null,"rate_limit_reached_type":null}}}}}}"#,
        iso(at),
        // a second either way, as the real records jitter
        RENEWS + at % 2
    )
}

fn amounts(dir: &std::path::Path, kind: &str) -> Vec<String> {
    rows(dir, kind).iter().map(|f| f[6].split(' ').next().unwrap().to_string()).collect()
}

#[test]
fn a_week_running_out_and_the_balance_falling_makes_one_spend_per_drop() {
    let w = world("codex-wallet-cross");
    let mut a = CodexAlloc::new(w.dir.clone());
    a.observe(&snap(&w.rollout, AFTER - 600, 0));
    let lines: Vec<String> = CROSS.lines().map(str::to_string).collect();
    append(&w.rollout, &lines);
    a.observe(&snap(&w.rollout, AFTER, 0));

    let want = ["amount=1.731320", "amount=1.118640", "amount=1.203760", "amount=1.274960", "amount=0.542160", "amount=0.675320"];
    assert_eq!(amounts(&w.dir, "spend"), want);
    assert!(rows(&w.dir, "wallet_drop").is_empty(), "the week was used up at every drop");
    let spends = rows(&w.dir, "spend");
    let sum: f64 = spends.iter().map(|f| f[6].split(' ').next().unwrap()["amount=".len()..].parse::<f64>().unwrap()).sum();
    assert!((sum - (59_775.634_925 - 59_769.088_765)).abs() < 1e-6, "the spends add up to the balance's drop: {sum}");
    let first = &spends[0];
    assert_eq!((first[2].as_str(), first[4].as_str()), ("487", "2"));
    assert!(first[6].contains("balance=59773.903605") && first[6].contains("model=gpt-6.2-astra") && first[6].contains(" in="), "{}", first[6]);

    let (_, view) = wallet::all("codex").into_iter().find(|(p, _)| *p == 2).expect("published");
    assert!((view.balance - 59_769.088_765).abs() < 1e-6 && view.week_full && view.spending());
    assert_eq!(view.spends.len(), 6);

    // a restart reads the tail again and writes none of it twice
    let mut again = CodexAlloc::new(w.dir.clone());
    again.observe(&snap(&w.rollout, AFTER + 60, 0));
    assert_eq!(rows(&w.dir, "spend").len(), 6);
    append(&w.rollout, &[reading(AFTER + 100, 100.0, r#""59768.0887650000""#)]);
    again.observe(&snap(&w.rollout, AFTER + 120, 0));
    assert_eq!(amounts(&w.dir, "spend").last().map(String::as_str), Some("amount=1.000000"), "it goes on from the last balance");
}

#[test]
fn a_drop_with_the_week_not_used_up_is_a_wallet_drop_and_no_spend() {
    let w = world("codex-wallet-drop");
    let now = 1_795_900_000;
    let mut a = CodexAlloc::new(w.dir.clone());
    a.observe(&snap(&w.rollout, now - 600, now - H));
    append(&w.rollout, &[reading(now - 300, 37.0, r#""100.0""#), reading(now - 200, 38.0, r#""98.5""#), reading(now - 100, 38.0, r#""98.5""#)]);
    a.observe(&snap(&w.rollout, now, now - H));
    assert!(rows(&w.dir, "spend").is_empty());
    assert_eq!(amounts(&w.dir, "wallet_drop"), ["amount=1.500000"]);
    assert!(rows(&w.dir, "wallet_drop")[0][6].ends_with("week=0.38"));
}

#[test]
fn a_balance_that_does_not_read_is_no_reading() {
    let w = world("codex-wallet-null");
    let now = 1_795_900_000;
    let mut a = CodexAlloc::new(w.dir.clone());
    a.observe(&snap(&w.rollout, now - 600, now - H));
    append(&w.rollout, &[reading(now - 300, 100.0, r#""100.0""#), reading(now - 200, 100.0, "null"), reading(now - 100, 100.0, r#""99.0""#)]);
    a.observe(&snap(&w.rollout, now, now - H));
    assert_eq!(amounts(&w.dir, "spend"), ["amount=1.000000"], "the drop is counted from the last balance that read");
    let (_, view) = wallet::all("codex").into_iter().find(|(p, _)| *p == 2).expect("published");
    assert_eq!(view.balance, 99.0);
}

#[test]
fn a_week_that_renews_is_measured_once() {
    let w = world("codex-wallet-period");
    let now = 1_795_900_000;
    let mut a = CodexAlloc::new(w.dir.clone());
    a.observe(&snap(&w.rollout, now - 600, now - 10 * H));
    let mut lines = vec![reading(now - 500, 10.0, r#""500.0""#), reading(now - 400, 60.0, r#""500.0""#), reading(now - 300, 100.0, r#""300.0""#)];
    // the same account a week on: its renewal has moved
    lines.push(reading(now - 200, 1.0, r#""300.0""#).replace(&format!("{RENEWS}"), &format!("{}", RENEWS + 7 * 24 * H)));
    append(&w.rollout, &lines);
    a.observe(&snap(&w.rollout, now, now - 10 * H));
    let text = std::fs::read_to_string(w.dir.parent().unwrap().join("per_point.tsv")).expect("the week that ended is written");
    let f: Vec<&str> = text.lines().next().unwrap().split('\t').collect();
    assert_eq!(text.lines().count(), 1);
    // 90 points measured before (10% to 100%), 200 credits after, a "-" model for want of a turn_context
    assert_eq!((f[1], f[3], f[4], f[6]), ("2", "-", "90.00", "200.000000"));
    assert_eq!(rows(&w.dir, "spend").len(), 1);
}

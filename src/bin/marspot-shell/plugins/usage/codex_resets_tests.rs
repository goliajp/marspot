//! The fixtures are what `codex app-server` 0.172.0 answered for two
//! accounts on 2026-11-26 around 18:57Z, and the devops Codex feed
//! written at 18:55Z, with ids and addresses replaced.

use super::*;

const RL_1: &str = include_str!("fixtures/codex-1-rate-limits.json");
const RL_2: &str = include_str!("fixtures/codex-2-rate-limits.json");
const FEED: &str = include_str!("fixtures/devops-codex-feed.json");
/// 2026-11-26T18:57:09Z, when the answers were read.
const NOW: i64 = 1_795_719_429;

fn replies(rl: &str) -> Replies {
    let mut r = Replies::default();
    r.take(r#"{"id":"acct","result":{"account":{"type":"chatgpt","email":"ada@example.jp","planType":"pro"}}}"#);
    r.take(rl.trim());
    r
}

fn own(name: &str, rl: &str) -> AccountResets {
    let profile = name.rsplit(' ').next().unwrap().parse().unwrap();
    AccountResets { agent: "codex".into(), profile, name: name.into(), resets: of_replies(&replies(rl), NOW).expect("a credit list") }
}

#[test]
fn every_available_credit_is_a_grant_ending_when_the_answer_says() {
    let r = of_replies(&replies(RL_1), NOW).expect("a credit list");
    assert_eq!((r.source.as_str(), r.left_total(), r.next_expiry()), ("app_server", 4, Some(1_796_070_363)));
    let ends: Vec<_> = r.grants.iter().map(|g| g.ends_at).collect();
    assert_eq!(ends, [Some(1_796_070_363), Some(1_796_165_597), Some(1_797_115_642), Some(1_798_266_047)]);
    let g = &r.grants[0];
    assert_eq!((g.kind.as_str(), g.restores.as_slice()), ("codex_full", ["seven_day".to_string()].as_slice()), "a Pro credit brings the week back");
    assert!(g.moves_weekly_reset && g.use_requires_limit == Some(false));
    assert_eq!((g.left, g.total, g.starts_at), (Some(1), Some(1), Some(1_793_478_363)));
    let body = marspot::agent_resets::render(&[own("Codex 1", RL_1)], NOW);
    assert!(!body.contains("redacted") && !body.contains('@'), "no credit id, no address: {body}");
}

#[test]
fn a_plus_plan_restores_both_windows_and_used_or_spent_credits_count_nothing() {
    let rl = RL_2.replace(r#""status":"available","grantedAt":1793572310"#, r#""status":"redeemed","grantedAt":1793572310"#);
    let rl = rl.replace(r#""status":"available","grantedAt":1794522412"#, r#""status":"redeeming","grantedAt":1794522412"#);
    let rl = rl.replace(r#""availableCount":3"#, r#""availableCount":1"#);
    let mut rp = replies(&rl);
    rp.account = None;
    let rl_plus = rl.replace(r#""planType":"pro""#, r#""planType":"plus""#);
    rp.take(rl_plus.trim());
    let r = of_replies(&rp, NOW).expect("a list");
    assert_eq!(r.grants.len(), 2, "the redeemed one is gone");
    assert_eq!((r.grants[0].left, r.grants[0].unavailable_reason.as_deref()), (Some(0), Some("redeeming")));
    assert_eq!(r.left_total(), 1);
    assert_eq!(r.grants[1].restores, ["five_hour", "seven_day"]);
}

#[test]
fn no_credit_list_is_no_reading_and_a_bare_count_is_credits_with_no_end() {
    let mut r = Replies::default();
    r.take(r#"{"id":"rl","result":{"rateLimits":{"planType":"pro"}}}"#);
    assert_eq!(of_replies(&r, NOW), None);
    let mut r = Replies::default();
    r.take(r#"{"id":"rl","result":{"rateLimits":{"planType":"pro"},"rateLimitResetCredits":{"availableCount":2,"credits":[]}}}"#);
    let got = of_replies(&r, NOW).unwrap();
    assert_eq!((got.left_total(), got.next_expiry()), (2, None));
}

#[test]
fn the_feeds_counts_and_earliest_ends_are_read_per_account() {
    let feed = of_feed(FEED);
    let names: Vec<_> = feed.iter().map(|(n, r)| (n.as_str(), r.left_total(), r.next_expiry(), r.source.as_str())).collect();
    assert_eq!(names, [("Codex 1", 4, Some(1_796_070_363), "devops_feed"), ("Codex 2", 3, Some(1_796_164_310), "devops_feed")]);
    assert_eq!(feed[0].1.observed_at, marspot::cc_usage::parse_iso_utc("2026-11-26T18:55:36Z".into()).unwrap());
    assert!(feed[0].1.grants.iter().all(|g| g.ends_at == Some(1_796_070_363)), "one end for every credit");

    // a resets block, once devops writes one, is taken over the credit fields
    let block = r#""resets":{"observed_at":1795719400,"source":"app_server","grants":[{"ref":"codex:1:g1","kind":"codex_full","left":1,"total":1,"ends_at":1796070363}]},"tier""#;
    let with_block = FEED.replacen(r#""tier""#, block, 1);
    let feed = of_feed(&with_block);
    assert_eq!((feed[0].1.left_total(), feed[0].1.source.as_str()), (1, "app_server"));
    assert_eq!(feed[1].1.left_total(), 3);
    assert!(of_feed("{}").is_empty());
}

#[test]
fn the_recorded_answers_agree_with_the_feed_written_two_minutes_before() {
    let feed = of_feed(FEED);
    let mut mine = vec![own("Codex 1", RL_1), own("Codex 2", RL_2)];
    let said = settle(&mut mine, &feed, NOW);
    let verdicts: Vec<_> = said.iter().map(|(n, a, ..)| (n.as_str(), *a)).collect();
    assert_eq!(verdicts, [("Codex 1", Agreement::Same), ("Codex 2", Agreement::Same)]);

    // one credit used since the feed was written: they differ
    let used = RL_1.replacen(r#""status":"available""#, r#""status":"redeemed""#, 1).replace(r#""availableCount":4"#, r#""availableCount":3"#);
    let mut mine = vec![own("Codex 1", &used)];
    let said = settle(&mut mine, &feed, NOW);
    assert_eq!((said[0].1, said[0].2.left, said[0].3.left), (Agreement::Differs, 3, 4));

    // read an hour after the feed: not compared
    let mut late = vec![own("Codex 1", RL_1)];
    late[0].resets.observed_at = NOW + 3_600;
    assert_eq!(settle(&mut late, &feed, NOW + 3_600)[0].1, Agreement::Apart);
}

#[test]
fn a_credit_with_no_end_of_its_own_takes_the_feeds_while_it_is_ahead() {
    let feed = of_feed(FEED);
    let mut r = Replies::default();
    r.take(r#"{"id":"rl","result":{"rateLimits":{"planType":"pro"},"rateLimitResetCredits":{"availableCount":4,"credits":[]}}}"#);
    let bare = AccountResets { agent: "codex".into(), profile: 1, name: "Codex 1".into(), resets: of_replies(&r, NOW).unwrap() };
    let mut mine = vec![bare.clone()];
    settle(&mut mine, &feed, NOW);
    assert!(mine[0].resets.grants.iter().all(|g| g.ends_at == Some(1_796_070_363)));
    let mut past = vec![bare];
    settle(&mut past, &feed, 1_796_070_363);
    assert!(past[0].resets.grants.iter().all(|g| g.ends_at.is_none()), "an end already behind is not taken");
}

#[test]
fn a_block_nobody_read_is_no_reading_not_an_empty_list() {
    // devops's rows for accounts it has not read: source not_read, no grants
    let not_read = r#""resets":{"observed_at":"2026-11-26T18:55:36Z","source":"not_read","grants":[]},"tier""#;
    let feed = of_feed(&FEED.replacen(r#""tier""#, not_read, 1));
    assert_eq!((feed[0].1.left_total(), feed[0].1.source.as_str()), (4, "devops_feed"), "the credit fields are read instead");
    let claude = r#"{"accounts":[{"name":"Claude 1","resets":{"observed_at":1795719400,"source":"not_read","grants":[]}}]}"#;
    assert!(of_feed(claude).is_empty(), "nothing is known of it");
    let block = json::parse(r#"{"observed_at":1795719400,"source":"not_read","grants":[]}"#).unwrap();
    assert_eq!(Resets::from_value(&block), None);
}

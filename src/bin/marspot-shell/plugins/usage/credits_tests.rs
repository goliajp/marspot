//! The Codex balances: marspot's own reading is the newer of its two,
//! agrees with the devops feed's within what is spent between the two
//! readings plus one credit, is not compared with a stale or missing
//! feed, and a balance that did not read gives no sample and a `null`.

use super::super::feed::{self, Account, Credits};
use super::super::feed_load::load;
use super::super::samples::{Balance, Samples};
use super::*;
use crate::plugins::claudecode::allocation_events::ymd;

const NOW: i64 = 1_795_830_000;

fn account(name: &str, balance: Option<f64>, at: i64) -> Account {
    let credits = Credits { enabled: Some(true), balance, unit: Some("credits".into()), spend: Some("auto".into()), observed_at: Some(at), ..Credits::default() };
    Account { name: name.into(), email: "a@b".into(), credits: Some(credits), credits_seen: at, last_read: at, ..Account::default() }
}

/// Spending 688 credits an hour, the last spend at `at`.
fn spending(at: i64, balance: f64) -> View {
    View { at, balance, week_full: true, spent_24h: 688.0, spent_1h: 688.0, last_spend: Some(at), ..View::default() }
}

fn feed_body(generated: i64, rows: &[(&str, f64, i64)]) -> String {
    let rows: Vec<String> = rows
        .iter()
        .map(|(n, b, at)| format!(r#"{{"name":"{n}","status":"rejected","collected_at":"{}","credits":{{"enabled":true,"balance":{b}}}}}"#, feed::iso_utc(*at)))
        .collect();
    format!(r#"{{"generated_at":"{}","accounts":[{}]}}"#, feed::iso_utc(generated), rows.join(","))
}

#[test]
fn own_reading_is_the_newer_of_the_app_server_and_the_rollouts() {
    let accounts = [account("Codex 1", Some(60_460.5), NOW - 900), account("Codex 2", Some(47_000.0), NOW - 900), account("Codex 3", None, NOW)];
    let views = vec![(2, spending(NOW - 60, 46_434.0)), (1, View { at: NOW - 1800, balance: 1.0, ..View::default() }), (4, View { at: NOW, balance: 7.0, ..View::default() })];
    let own = own_balances(&accounts, &views);
    let got: Vec<(u8, i64, f64)> = own.iter().map(|b| (b.profile, b.at, b.balance)).collect();
    assert_eq!(got, [(1, NOW - 900, 60_460.5), (2, NOW - 60, 46_434.0), (4, NOW, 7.0)], "Codex 3's balance did not read");
}

#[test]
fn readings_within_the_spend_between_them_agree_and_others_disagree() {
    let views = vec![(2, spending(NOW, 46_434.384_485))];
    let own = vec![
        Balance { name: "Codex 1".into(), profile: 1, at: NOW, balance: 60_460.536_725, unit: "credits" },
        Balance { name: "Codex 2".into(), profile: 2, at: NOW, balance: 46_434.384_485, unit: "credits" },
    ];
    // as on 2026-11-29: devops read Codex 2 a minute earlier, 1.17 apart
    let body = feed_body(NOW - 30, &[("Codex 1", 60_460.536_725, NOW - 40), ("Codex 2", 46_433.218_085, NOW - 60)]);
    let mut checks = Checks::default();
    let v = checks.check(Some(&body), &own, &views);
    assert_eq!(v.iter().map(|v| (v.name.as_str(), v.agree)).collect::<Vec<_>>(), [("Codex 1", true), ("Codex 2", true)]);
    assert!(v[1].line.contains("tolerance=12.467"), "688/h for 60 s plus one: {}", v[1].line);

    // Codex 1 reads 2 credits apart with nothing being spent: beyond the one allowed
    let body = feed_body(NOW - 30, &[("Codex 1", 60_458.5, NOW - 40)]);
    let v = checks.check(Some(&body), &own, &views);
    assert_eq!((v.len(), v[0].agree), (1, false));
    assert!(v[0].line.contains("own=60460.536725"), "marspot's reading is the one shown first: {}", v[0].line);
}

#[test]
fn a_stale_or_missing_feed_is_not_compared_with() {
    let own = vec![Balance { name: "Codex 1".into(), profile: 1, at: NOW, balance: 5.0, unit: "credits" }];
    let mut checks = Checks::default();
    assert!(checks.check(None, &own, &[]).is_empty(), "the collector hands no feed that is stale or missing");
    assert!(compare("not json", &own, &[]).is_empty());
    let unread = r#"{"generated_at":"2026-11-29T00:00:00+00:00","accounts":[{"name":"Codex 1","credits":{"balance":null}}]}"#;
    assert!(compare(unread, &own, &[]).is_empty(), "a feed without a balance has nothing to compare");
}

#[test]
fn an_unreadable_balance_gives_no_wallet_row_and_a_null_in_the_feed() {
    let dir = marspot_term::test_env::TempDir::new("usage-credits-unread");
    let mut s = Samples::new(dir.join("s"), [("claude", &[]), ("codex", &[]), ("kimi", &[])]);
    let accounts = [account("Codex 1", None, NOW - 60), account("Codex 2", Some(46_434.384_485), NOW - 60)];
    let own = own_balances(&accounts, &[]);
    assert_eq!(s.record_wallet("codex", &own, NOW).unwrap(), 1);
    assert_eq!(s.record_wallet("codex", &own, NOW + 300).unwrap(), 0, "the same reading again");
    let text = std::fs::read_to_string(dir.join(format!("s/{}.tsv", ymd(NOW)))).unwrap();
    assert_eq!(text.lines().nth(1), Some(format!("{}\tcodex\t2\twallet\t46434.384485\tcredits\t-", NOW - 60).as_str()));
    assert_eq!(text.lines().count(), 2, "no row for Codex 1");
    // a restart does not write it again
    let mut again = Samples::new(dir.join("s"), [("claude", &[]), ("codex", &[]), ("kimi", &[])]);
    assert_eq!(again.record_wallet("codex", &own, NOW + 600).unwrap(), 0);

    let body = feed::render(&accounts, NOW);
    assert!(body.contains(r#""balance":null,"#), "{body}");
    assert!(!body.contains(r#""balance":0"#));
    assert_eq!(load(&body), accounts, "and it reads back the same");
}

#[test]
fn the_spend_fields_come_from_the_rollouts_and_unknown_ones_stay_null() {
    let mut accounts = vec![account("Codex 2", Some(47_000.0), NOW - 900), account("Codex 1", Some(60_460.5), NOW - 900)];
    let mut v = spending(NOW - 60, 46_434.0);
    v.drops = vec![(NOW - 5000, 3.5)];
    enrich(&mut accounts, &[(2, v.clone())], NOW);
    let c = accounts[0].credits.clone().unwrap();
    assert_eq!((c.balance, c.observed_at, c.spending), (Some(46_434.0), Some(NOW - 60), Some(true)));
    assert!((c.rate_per_h.unwrap() - 688.0).abs() < 1e-6 && (c.spent_24h.unwrap() - 688.0).abs() < 1e-6);
    assert_eq!((c.lots.as_ref(), c.expiry_source.as_ref()), (None, None), "no lot recorded yet: the expiry is unknown, not none");
    let body = feed::render(&accounts, NOW);
    assert!(body.contains(r#""unexplained_drops":[{"at":"#) && body.contains(r#""lots":null,"expiry_source":null"#), "{body}");
    let other = accounts[1].credits.as_ref().unwrap();
    assert_eq!((other.spending, other.spent_24h, other.drops.as_ref()), (None, None, None), "no rollout read: unknown");
    assert_eq!(load(&body), accounts);

    // a recorded lot is the inferred expiry; spending is judged now, long after the last spend
    v.lot = Some(Lot { amount: 62_000.0, expires_at: NOW + 180 * 86_400, bound: "latest".into(), seen_by: Some("rollout".into()) });
    enrich(&mut accounts, &[(2, v)], NOW + 3600);
    let c = accounts[0].credits.clone().unwrap();
    assert_eq!((c.spending, c.expiry_source.as_deref()), (Some(false), Some("inferred")));
    let body = feed::render(&accounts, NOW);
    assert!(body.contains(&format!(r#""lots":[{{"amount":62000,"expires_at":{},"bound":"latest","seen_by":"rollout"}}],"expiry_source":"inferred""#, NOW + 180 * 86_400)), "{body}");
    assert_eq!(load(&body), accounts);
}

#[test]
fn the_newer_reading_sets_the_cap_and_a_cap_gone_is_cleared() {
    let cap = Cap { reached: Some(true), ..Cap::default() };
    let mut accounts = vec![account("Codex 2", Some(47_000.0), NOW - 900)];
    enrich(&mut accounts, &[(2, View { cap: Some(cap.clone()), ..spending(NOW - 600, 46_900.0) })], NOW);
    assert_eq!(accounts[0].credits.as_ref().unwrap().cap, Some(cap));
    enrich(&mut accounts, &[(2, spending(NOW - 60, 46_800.0))], NOW);
    assert_eq!(accounts[0].credits.as_ref().unwrap().cap, None, "the newer reading has no cap");
}

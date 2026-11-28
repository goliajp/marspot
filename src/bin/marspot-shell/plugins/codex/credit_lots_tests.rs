//! The lot log: the end by the rule in force, the line chosen per
//! account, and the lines written once each.

use std::fs;

use super::*;
use marspot_term::test_env::TempDir;

/// 2026-09-30 11:05:21 UTC and a year on.
const SEPT_30: i64 = 1_790_766_321;
const SEPT_30_NEXT: i64 = 1_822_302_321;

fn line(first_seen: i64, profile: u8, amount: f64, by: &str, recorded_at: i64) -> LotLine {
    LotLine { first_seen, profile, amount, seen_by: by.into(), recorded_at }
}

#[test]
fn a_grant_seen_before_the_thirteenth_of_november_lasts_a_year_after_it_180_days() {
    assert_eq!(expiry(SEPT_30), SEPT_30_NEXT);
    assert_eq!(expiry(TWELVE_MONTHS_BEFORE - 1), plus_a_year(TWELVE_MONTHS_BEFORE - 1));
    assert_eq!(expiry(TWELVE_MONTHS_BEFORE), TWELVE_MONTHS_BEFORE + 180 * 86_400, "from that day on, Japan's 180 days");
    // 2028-02-29 12:00 to 2029-02-28 12:00
    assert_eq!(plus_a_year(1_835_438_400), 1_866_974_400);
    for days in [-800, 0, 19_000, 20_726, 60_000] {
        let (y, m, d) = civil(days);
        assert_eq!(days_of(y, m, d), days);
    }
}

#[test]
fn both_spellings_of_a_line_read_and_the_end_is_worked_out_again() {
    // marspot's line from before seen_by was written: its stored 180 days do not count
    let old = parse("1795800000\t2\t59775.634925\t1811352000\tlatest").unwrap();
    assert_eq!(old, line(1_795_800_000, 2, 59_775.634_925, MARSPOT, 1_795_800_000));
    let new = parse(line(SEPT_30, 2, 62_500.0, ROLLOUT, 1_795_900_000).line().trim_end()).unwrap();
    assert_eq!(new, line(SEPT_30, 2, 62_500.0, ROLLOUT, 1_795_900_000));
    assert_eq!(parse("1\t2\t3"), None);
    let lot = pick(&[old], 2).unwrap();
    assert_eq!((lot.expires_at, lot.seen_by.as_deref()), (1_795_800_000 + 180 * 86_400, Some(MARSPOT)));
}

#[test]
fn the_line_seen_first_is_the_lot_whatever_the_order_of_the_file() {
    let lines = [
        line(1_795_800_000, 2, 59_775.6, MARSPOT, 1_795_800_000),
        line(SEPT_30, 2, 62_500.0, ROLLOUT, 1_795_900_000),
        line(SEPT_30 + 86_400, 2, 62_500.0, "devops", 1_795_900_100),
        line(1_795_000_000, 1, 60_460.5, MARSPOT, 1_795_000_000),
    ];
    let want = Lot { amount: 62_500.0, expires_at: SEPT_30_NEXT, bound: "latest".into(), seen_by: Some(ROLLOUT.into()) };
    assert_eq!(pick(&lines, 2), Some(want.clone()));
    let mut rev = lines.clone();
    rev.reverse();
    assert_eq!(pick(&rev, 2), Some(want));
    assert_eq!(pick(&lines, 1).map(|l| l.amount), Some(60_460.5), "each account its own");
    assert_eq!(pick(&lines, 3), None);
    // the same moment from two sources: the rollout's, then the one written first
    let tie = [line(SEPT_30, 2, 1.0, MARSPOT, 5), line(SEPT_30, 2, 2.0, ROLLOUT, 9), line(SEPT_30, 2, 3.0, ROLLOUT, 7)];
    for order in [[0, 1, 2], [2, 1, 0], [1, 2, 0]] {
        let l: Vec<LotLine> = order.iter().map(|i| tie[*i].clone()).collect();
        assert_eq!(pick(&l, 2).map(|l| l.amount), Some(3.0));
    }
}

#[test]
fn marspots_own_sighting_is_written_once_and_kept_beside_an_earlier_one() {
    let t = TempDir::new("codex-lots-log");
    let path = t.join("credit_lots.tsv");
    fs::write(&path, line(SEPT_30, 2, 62_500.0, ROLLOUT, 1_795_900_000).line()).unwrap();
    let mut log = Vec::new();
    let mut lots = LotLog::new(path.clone(), t.join("home"));
    lots.sighted(2, 1_795_950_000, 46_434.0, 1_795_950_010, &mut log);
    lots.sighted(2, 1_795_960_000, 46_400.0, 1_795_960_010, &mut log);
    let text = fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 2, "{text}");
    assert!(text.ends_with("1795950000\t2\t46434.000000\t1811502000\tlatest\tmarspot\t1795950010\n"), "{text}");
    assert_eq!(lots.lot(2).map(|l| (l.expires_at, l.seen_by)), Some((SEPT_30_NEXT, Some(ROLLOUT.into()))));
    // a rollout already dated it: no scan, and none started for an account without a login
    lots.scan([2, 1], 1_795_960_020, &mut log);
    assert!(!lots.scanning());
    assert!(log.iter().any(|(tag, l)| *tag == "codex.wallet.lot_scan" && l.starts_with("P1 ")), "{log:?}");
    let again = LotLog::new(path, t.join("home"));
    assert_eq!(again.lot(2), lots.lot(2), "a restart reads the same lot back");
}

fn login(homes: &std::path::Path, profile: u8, account: &str) {
    let home = if profile == 0 { homes.join(".codex") } else { homes.join(format!(".codex-profile-{profile}")) };
    fs::create_dir_all(home.join("sessions")).unwrap();
    fs::write(home.join("auth.json"), format!(r#"{{"tokens":{{"account_id":"{account}"}}}}"#)).unwrap();
}

fn wait(lots: &LotLog) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while lots.scanning() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn a_finished_scan_that_found_nothing_is_not_read_again_from_the_start() {
    let t = TempDir::new("codex-lots-progress");
    let homes = t.join("home");
    login(&homes, 1, "acct-test-1");
    let day = homes.join(".codex-profile-1/sessions/2026/10/01");
    fs::create_dir_all(&day).unwrap();
    fs::write(day.join("rollout-a.jsonl"), "{\"type\":\"session_meta\",\"payload\":{\"creator_account_id\":\"acct-test-1\"}}\n").unwrap();
    let mut log = Vec::new();
    let mut lots = LotLog::new(t.join("credit_lots.tsv"), homes.clone());
    lots.scan([1], 10, &mut log);
    wait(&lots);
    lots.scan([1], 20, &mut log);
    let digest = marspot_peer::control::sha256::hex_prefix(b"acct-test-1", 16);
    let progress = fs::read_to_string(t.join("credit_scan.tsv")).unwrap();
    assert_eq!(progress, format!("P\t1\t{digest}\t20261001\t1\n"));
    assert!(!progress.contains("acct-test-1"), "the id is never written");
    assert!(log.iter().any(|(_, l)| l.contains("complete=true files=1 ") && l.contains("background=true")), "{log:?}");
    // a restart reads that day again and the newer ones, not the older
    let old = homes.join(".codex-profile-1/sessions/2026/09/20");
    fs::create_dir_all(&old).unwrap();
    fs::write(old.join("rollout-old.jsonl"), "{}\n").unwrap();
    let mut log = Vec::new();
    let mut again = LotLog::new(t.join("credit_lots.tsv"), homes);
    again.scan([1], 30, &mut log);
    wait(&again);
    again.scan([1], 40, &mut log);
    assert!(log.iter().any(|(_, l)| l.contains("complete=true files=1 ")), "{log:?}");
}

#[test]
fn two_logins_of_one_account_are_neither_scanned() {
    let t = TempDir::new("codex-lots-twins");
    let homes = t.join("home");
    login(&homes, 1, "acct-team");
    login(&homes, 3, "acct-team");
    let mut log = Vec::new();
    let mut lots = LotLog::new(t.join("credit_lots.tsv"), homes);
    lots.scan([1, 3], 10, &mut log);
    assert!(!lots.scanning());
    let lines: Vec<&String> = log.iter().filter(|(tag, _)| *tag == "codex.wallet.lot_scan").map(|(_, l)| l).collect();
    assert_eq!(lines.len(), 2, "{log:?}");
    assert!(lines.iter().all(|l| l.starts_with("ambiguous ") && !l.contains("acct-team")), "{log:?}");
}

#[test]
fn a_reading_from_a_scan_that_did_not_settle_its_account_is_not_written() {
    let t = TempDir::new("codex-lots-unsettled");
    let mut log = Vec::new();
    let mut lots = LotLog::new(t.join("credit_lots.tsv"), t.join("home"));
    let w = |profile: u8| Want { profile, account: format!("acct-{profile}"), from: i64::MIN };
    let mut done = credit_scan::Scanned { wants: vec![w(1), w(2)], ..Default::default() };
    done.found.insert(1, Found { at: SEPT_30, balance: 62_500.0 });
    done.found.insert(2, Found { at: SEPT_30, balance: 62_500.0 });
    done.settled.insert(2, Some(SEPT_30));
    lots.took(done, 50, &mut log);
    assert_eq!(lots.lot(1), None, "its earliest days may not have been read");
    assert_eq!(lots.lot(2).and_then(|l| l.seen_by).as_deref(), Some(ROLLOUT));
    assert!(!t.join("credit_scan.tsv").exists(), "nothing found unsettled is kept");
}

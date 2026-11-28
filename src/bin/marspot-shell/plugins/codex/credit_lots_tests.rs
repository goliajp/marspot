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

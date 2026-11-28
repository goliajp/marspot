//! The scan for each account's first balance in the shared rollouts.
//!
//! `codex-credits-grant.jsonl` is a real rollout's last two readings
//! with no balance and first two with 62500 on 2026-09-30, unchanged,
//! under a made-up first line naming its maker `acct-test-1`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::*;
use crate::plugins::codex::credit_lots::days_of;
use marspot_term::test_env::TempDir;

const GRANT: &str = include_str!("fixtures/codex-credits-grant.jsonl");
/// 2026-09-30 11:05:21 UTC, the fixture's first reading with 62500.
const GRANTED_AT: i64 = 1_790_766_321;
const WEEK: &str = "1791050252";

fn put(sessions: &Path, day: &str, name: &str, text: &str) -> PathBuf {
    let dir = sessions.join(&day[..4]).join(&day[4..6]).join(&day[6..]);
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join(format!("rollout-{name}.jsonl"));
    fs::write(&p, text).unwrap();
    p
}

fn want(profile: u8, account: &str) -> Want {
    Want { profile, account: account.to_string(), from: i64::MIN }
}

fn run(sessions: &Path, wants: &[Want]) -> Scanned {
    let mut out = Scanned::default();
    assert!(scan(sessions, wants, Instant::now() + Duration::from_secs(60), &mut out));
    out
}

/// The fixture as made by `maker`.
fn made_by(maker: &str) -> String {
    GRANT.replacen("acct-test-1", maker, 1)
}

#[test]
fn the_rise_from_nothing_is_found_with_its_maker() {
    let t = TempDir::new("codex-scan-rise");
    put(&t.join("s"), "20260927", "a", GRANT);
    let out = run(&t.join("s"), &[want(1, "acct-test-1"), want(2, "acct-test-2")]);
    assert_eq!(out.found.get(&1), Some(&Found { at: GRANTED_AT, balance: 62_500.0 }));
    assert_eq!(out.found.get(&2), None, "another account's rollout says nothing of this one");
}

#[test]
fn the_earliest_of_several_files_wins_and_unattributable_ones_are_skipped() {
    let t = TempDir::new("codex-scan-earliest");
    // a file of no known maker, as codex wrote them before it named one, with an earlier balance
    let nameless = GRANT.lines().skip(1).collect::<Vec<_>>().join("\n").replace("2026-09-30T", "2026-09-29T");
    put(&t.join("s"), "20260920", "nameless", &format!("{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"/w\"}}}}\n{nameless}\n"));
    // the same account seen later in a newer file
    let later = made_by("acct-test-1").replace("2026-09-30T11:0", "2026-10-01T11:0");
    put(&t.join("s"), "20261001", "later", &later);
    put(&t.join("s"), "20260927", "a", GRANT);
    let out = run(&t.join("s"), &[want(1, "acct-test-1")]);
    assert_eq!(out.found.get(&1).map(|f| f.at), Some(GRANTED_AT));
}

#[test]
fn a_file_is_left_once_its_week_is_another_accounts() {
    let t = TempDir::new("codex-scan-resumed");
    // made by account 2, resumed under account 1 before any balance showed: another week's renewal
    let lines: Vec<String> = made_by("acct-test-2")
        .lines()
        .enumerate()
        .map(|(i, l)| if i >= 3 { l.replace(WEEK, "1791999999") } else { l.to_string() })
        .collect();
    put(&t.join("s"), "20260927", "a", &(lines.join("\n") + "\n"));
    let out = run(&t.join("s"), &[want(2, "acct-test-2")]);
    assert_eq!(out.found.get(&2), None, "the balance came with account 1's week: not account 2's");
    // the jitter of a second in the renewal is the same week
    let t = TempDir::new("codex-scan-jitter");
    put(&t.join("s"), "20260927", "a", &GRANT.replace(&format!("{WEEK}}},\"secondary\":null,\"credits\":{{\"has_credits\":true"), "1791050253},\"secondary\":null,\"credits\":{\"has_credits\":true"));
    assert_eq!(run(&t.join("s"), &[want(1, "acct-test-1")]).found.get(&1).map(|f| f.at), Some(GRANTED_AT));
}

#[test]
fn days_after_every_account_was_found_are_not_opened() {
    let t = TempDir::new("codex-scan-stop");
    put(&t.join("s"), "20260927", "a", GRANT);
    let late = put(&t.join("s"), "20261020", "late", &made_by("acct-test-1"));
    let out = run(&t.join("s"), &[want(1, "acct-test-1")]);
    assert_eq!(out.files, 1, "{late:?} is past the reading found");
}

#[test]
fn the_account_id_comes_from_the_login_and_only_a_plain_one() {
    let t = TempDir::new("codex-scan-login");
    fs::write(t.join("auth.json"), r#"{"auth_mode":"chatgpt","tokens":{"id_token":"x","access_token":"y","account_id":"6dc92e3e-2437"}}"#).unwrap();
    assert_eq!(account_of(&t.join("")).as_deref(), Some("6dc92e3e-2437"));
    fs::write(t.join("auth.json"), r#"{"tokens":{"account_id":"a\"b"}}"#).unwrap();
    assert_eq!(account_of(&t.join("")), None);
    assert_eq!(account_of(&t.join("missing")), None);
}

#[test]
fn a_scan_of_two_homes_sharing_one_directory_reads_it_once() {
    let t = TempDir::new("codex-scan-shared");
    let shared = t.join("shared");
    put(&shared, "20260927", "a", GRANT);
    put(&shared, "20260927", "b", &made_by("acct-test-2"));
    for p in ["h1", "h2"] {
        fs::create_dir_all(t.join(p)).unwrap();
        std::os::unix::fs::symlink(&shared, t.join(p).join("sessions")).unwrap();
    }
    let out = scan_all(vec![(t.join("h1/sessions"), want(1, "acct-test-1")), (t.join("h2/sessions"), want(2, "acct-test-2"))]);
    assert!(out.complete && out.found.len() == 2 && out.files == 2, "{out:?}");
}

#[test]
fn an_account_searched_from_a_later_day_leaves_the_older_days_unread() {
    let t = TempDir::new("codex-scan-from");
    put(&t.join("s"), "20260927", "a", GRANT);
    let later = made_by("acct-test-1").replace("2026-09-30T11:0", "2026-10-02T11:0");
    put(&t.join("s"), "20261002", "later", &later);
    let oct_1 = days_of(2026, 10, 1) * DAY;
    let out = run(&t.join("s"), &[Want { from: oct_1, ..want(1, "acct-test-1") }]);
    assert_eq!(out.files, 1, "{out:?}");
    assert_eq!(out.found.get(&1).map(|f| f.at), Some(GRANTED_AT + 2 * DAY));
    assert_eq!(out.settled.get(&1), Some(&Some(oct_1 + DAY)), "the newest day opened");
}

#[test]
fn a_scan_stopped_by_its_deadline_settles_nothing() {
    let t = TempDir::new("codex-scan-deadline");
    put(&t.join("s"), "20260927", "a", GRANT);
    let mut out = Scanned::default();
    assert!(!scan(&t.join("s"), &[want(1, "acct-test-1")], Instant::now() - Duration::from_secs(1), &mut out));
    assert!(out.settled.is_empty(), "{out:?}");
    // an empty directory read to its end settles its accounts with no day opened
    let out = run(&t.join("none"), &[want(1, "acct-test-1")]);
    assert_eq!(out.settled.get(&1), Some(&None));
}

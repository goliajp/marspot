use super::feed::{self, Account, Credits, ModelWindow, Reading, Window};
use super::samples::{Samples, HEADER};
use super::{Collector, Sources};
use crate::plugins::claudecode::allocation_events::{ymd, DAY};

const NOW: i64 = 1_795_530_644;

fn account(name: &str, seen: i64) -> Account {
    Account {
        name: name.into(),
        email: "ada@example.jp".into(),
        five_hour: Some(Window::new(0.25, Some(seen + 3_600), seen)),
        seven_day: Some(Window::new(0.5, Some(seen + 86_400), seen)),
        models: vec![ModelWindow { label: "Fable".into(), minutes: feed::WEEK_MINUTES, window: Window::new(0.125, None, seen) }],
        credits: Some(Credits { used: Some(3.5), limit: Some(50.0), ..Credits::default() }),
        last_read: seen,
        credits_seen: seen,
        ..Account::default()
    }
}

fn lines(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path).unwrap_or_default().lines().map(str::to_string).collect()
}

#[test]
fn usage_samples_are_written_once_per_collected_at() {
    let dir = marspot_term::test_env::TempDir::new("usage-samples-once");
    let mut s = Samples::new(dir.join("s"), [("claude", &[]), ("codex", &[]), ("kimi", &[])]);
    let file = dir.join(format!("s/{}.tsv", ymd(NOW)));
    let two = [account("Claude 3", NOW - 60), account("Claude", NOW - 60)];
    assert_eq!(s.record("claude", &two, NOW).unwrap(), 8, "four windows each");
    assert_eq!(s.record("claude", &two, NOW + 300).unwrap(), 0, "the same collected_at again");
    let got = lines(&file);
    assert_eq!(got.len(), 1 + 8);
    assert_eq!(format!("{}\n", got[0]), HEADER);
    let at = NOW - 60;
    assert_eq!(got[1], format!("{at}\tclaude\t3\t5h\t25.00\tpct\t{}", at + 3_600));
    assert_eq!(got[3], format!("{at}\tclaude\t3\tFable/10080m\t12.50\tpct\t-"));
    assert_eq!(got[4], format!("{at}\tclaude\t3\tcredits\t3.5\t50\t-"));
    assert_eq!(got[5], format!("{at}\tclaude\t0\t5h\t25.00\tpct\t{}", at + 3_600));
    assert!(!got.iter().any(|l| l.contains('@')), "account numbers only, never an email");

    let newer = [account("Claude 3", NOW + 240), account("Claude", NOW - 60)];
    assert_eq!(s.record("claude", &newer, NOW + 300).unwrap(), 4, "only the account read again");
    assert_eq!(lines(&file).len(), 1 + 8 + 4, "a second set, no second header");
    assert_eq!(s.record("codex", &[account("Codex 1", NOW + 240)], NOW + 300).unwrap(), 4, "each agent on its own");
}

#[test]
fn usage_samples_keep_fifteen_days() {
    let dir = marspot_term::test_env::TempDir::new("usage-samples-days");
    let root = dir.join("s");
    std::fs::create_dir_all(&root).unwrap();
    let (old, kept) = (root.join(format!("{}.tsv", ymd(NOW - 16 * DAY))), root.join(format!("{}.tsv", ymd(NOW - 14 * DAY))));
    std::fs::write(&old, HEADER).unwrap();
    std::fs::write(&kept, HEADER).unwrap();
    let mut s = Samples::new(root.clone(), [("claude", &[]), ("codex", &[]), ("kimi", &[])]);
    assert_eq!(s.record("kimi", &[account("Kimi 2", NOW)], NOW).unwrap(), 4);
    assert!(!old.exists(), "sixteen days old: deleted whole");
    assert_eq!(lines(&kept).len(), 1, "fourteen days old: kept as it was");
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
}

#[test]
fn usage_samples_held_readings_are_not_new_after_a_restart() {
    let dir = marspot_term::test_env::TempDir::new("usage-samples-restart");
    let held = [account("Codex 2", NOW - 60)];
    let mut s = Samples::new(dir.join("s"), [("claude", &[]), ("codex", &held), ("kimi", &[])]);
    assert_eq!(s.record("codex", &held, NOW).unwrap(), 0);
    assert!(!dir.join("s").exists());
}

/// While the devops feed is fresh the collector neither writes nor
/// grows a samples file; once it goes stale the same pass samples.
#[test]
fn usage_samples_stay_still_while_devops_collects() {
    let dir = marspot_term::test_env::TempDir::new("usage-samples-devops");
    let home = dir.join("home");
    // a codex home with no CLI: the pass keeps its numbers and asks nothing
    std::fs::create_dir_all(home.join(".codex-profile-1")).unwrap();
    std::fs::create_dir_all(dir.join("devops")).unwrap();
    let sources = Sources { claude: dir.join("devops/claude.json"), codex: dir.join("devops/codex.json") };
    let feed_at = |at: i64| format!(r#"{{"generated_at":"{}","accounts":[]}}"#, feed::iso_utc(at));
    std::fs::write(&sources.claude, feed_at(NOW - 60)).unwrap();
    std::fs::write(&sources.codex, feed_at(NOW - 60)).unwrap();
    let own_claude = dir.join("usage/claude-usage.json");
    let samples = dir.join("usage/usage_samples");
    let file = samples.join(format!("{}.tsv", ymd(NOW)));

    let mut c = Collector::new(sources, own_claude, dir.join("usage/codex-usage.json"));
    c.codex = vec![account("Codex 1", NOW - 30)];
    c.pass(&home, NOW);
    assert!(!samples.exists(), "devops collects: no file");
    assert_eq!(c.by_devops, [Some(true), Some(true)]);

    std::fs::write(&c.sources.codex, feed_at(NOW - 900)).unwrap();
    c.pass(&home, NOW);
    assert_eq!(lines(&file).len(), 1 + 4, "marspot writes the codex feed: one set");
    assert_eq!(c.by_devops, [Some(true), Some(false)]);

    std::fs::write(&c.sources.codex, feed_at(NOW + 200)).unwrap();
    c.codex = vec![account("Codex 1", NOW + 230)];
    let before = std::fs::metadata(&file).unwrap().len();
    c.pass(&home, NOW + 300);
    assert_eq!(std::fs::metadata(&file).unwrap().len(), before, "devops is back: the file does not grow");
}

#[test]
fn usage_samples_a_failed_write_is_tried_again() {
    let dir = marspot_term::test_env::TempDir::new("usage-samples-retry");
    let root = dir.join("s");
    std::fs::write(&root, "not a directory").unwrap();
    let mut s = Samples::new(root.clone(), [("claude", &[]), ("codex", &[]), ("kimi", &[])]);
    let one = [account("Claude 2", NOW - 60)];
    assert!(s.record("claude", &one, NOW).is_err(), "the target is a regular file");
    std::fs::remove_file(&root).unwrap();
    assert_eq!(s.record("claude", &one, NOW + 300).unwrap(), 4, "the rows were not lost");
    assert_eq!(lines(&root.join(format!("{}.tsv", ymd(NOW - 60)))).len(), 1 + 4);
}

#[test]
fn usage_samples_a_read_without_credits_writes_no_credits_row() {
    let dir = marspot_term::test_env::TempDir::new("usage-samples-credits");
    let mut s = Samples::new(dir.join("s"), [("claude", &[]), ("codex", &[]), ("kimi", &[])]);
    let mut a = Account { name: "Claude 4".into(), ..Account::default() };
    let spend = Some(Credits { used: Some(3.5), limit: Some(50.0), ..Credits::default() });
    a.absorb(Reading { seven_day: Some(Window::new(0.5, None, NOW - 60)), credits: spend, ..Reading::default() }, NOW - 60);
    assert_eq!(s.record("claude", std::slice::from_ref(&a), NOW).unwrap(), 2, "7d and credits");
    a.absorb(Reading { seven_day: Some(Window::new(0.6, None, NOW + 240)), ..Reading::default() }, NOW + 240);
    assert_eq!(s.record("claude", std::slice::from_ref(&a), NOW + 300).unwrap(), 1, "7d only");
    let got = lines(&dir.join(format!("s/{}.tsv", ymd(NOW))));
    assert_eq!(got.len(), 1 + 3);
    assert_eq!(got.iter().filter(|l| l.contains("\tcredits\t")).count(), 1);
}

#[test]
fn usage_samples_a_day_past_retention_is_not_written() {
    let dir = marspot_term::test_env::TempDir::new("usage-samples-expired");
    let mut s = Samples::new(dir.join("s"), [("claude", &[]), ("codex", &[]), ("kimi", &[])]);
    assert_eq!(s.record("codex", &[account("Codex 1", NOW - 16 * DAY)], NOW).unwrap(), 0);
    assert!(!dir.join("s").exists(), "no file comes back for an expired day");
}

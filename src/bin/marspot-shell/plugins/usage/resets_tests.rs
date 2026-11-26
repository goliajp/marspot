use super::super::resets_store::{rebuild, HEADER};
use super::*;
use marspot::agent_resets::{Grant, Outcome};

const FEED: &str = include_str!("fixtures/devops-codex-feed.json");
/// Two minutes after the feed was written.
const NOW: i64 = 1_795_719_456;

fn codex(left: u32, at: i64) -> AccountResets {
    let grant = Grant { kind: "codex_full".into(), left: Some(1), total: Some(1), ends_at: Some(1_796_070_363), ..Grant::default() };
    let resets = Reading { grants: vec![grant; left as usize], ..Reading::none(at, "app_server") };
    AccountResets { agent: "codex".into(), profile: 1, name: "Codex 1".into(), resets }
}

fn kimi() -> Account {
    Account { name: "Kimi 2".into(), last_read: NOW, ..Account::default() }
}

#[test]
fn a_reading_is_a_fact_only_when_it_says_something_new() {
    let dir = marspot_term::test_env::TempDir::new("usage-resets-facts");
    let mut r = Resets::new(dir.join("usage"), NOW);
    let feed = dir.join("devops-codex.json");
    r.pass(false, &feed, &feed, vec![codex(4, NOW)], &[kimi()], NOW);
    r.pass(false, &feed, &feed, vec![codex(4, NOW + 300)], &[kimi()], NOW + 300);
    r.pass(false, &feed, &feed, vec![codex(3, NOW + 600)], &[], NOW + 600);
    let day = std::fs::read_to_string(dir.join(format!("usage/resets/{}.tsv", crate::plugins::claudecode::allocation_events::ymd(NOW)))).unwrap();
    let rows: Vec<&str> = day.lines().collect();
    assert_eq!(rows[0], HEADER.trim_end());
    assert_eq!(rows.len(), 4, "codex 4, kimi none, codex 3: {day}");
    assert!(rows[1].starts_with(&format!("{NOW}\tcodex\t1\tCodex 1\tapp_server\tread\t{{")), "{}", rows[1]);
    assert!(rows[2].contains("\tkimi\t2\tKimi 2\tnone\tread\t"));

    let file = marspot::agent_resets::parse(&std::fs::read_to_string(dir.join("usage/resets.json")).unwrap());
    let got: Vec<_> = file.iter().map(|a| (a.agent.as_str(), a.profile, a.resets.left_total(), a.resets.observed_at)).collect();
    assert_eq!(got, [("codex", 1, 3, NOW + 600), ("kimi", 2, 0, NOW + 300)], "the last of each account");

    // the derived file, lost, is the last fact of each account again
    let mut rebuilt = rebuild(&dir.join("usage/resets"));
    rebuilt.sort_by_key(|a| a.agent.clone());
    assert_eq!(rebuilt.len(), file.len());
    for (a, b) in rebuilt.iter().zip(&file) {
        assert_eq!((&a.agent, a.profile, &a.name), (&b.agent, b.profile, &b.name));
        assert!(a.resets.same_as(&b.resets), "{a:?} / {b:?}");
    }
    std::fs::remove_file(dir.join("usage/resets.json")).unwrap();
    let again = Resets::new(dir.join("usage"), NOW);
    assert_eq!(again.store().last().len(), 2, "a restart without the file starts from the facts");
}

#[test]
fn a_restart_does_not_write_again_what_the_last_run_wrote() {
    let dir = marspot_term::test_env::TempDir::new("usage-resets-restart");
    let feed = dir.join("none.json");
    Resets::new(dir.join("usage"), NOW).pass(false, &feed, &feed, vec![codex(4, NOW)], &[], NOW);
    Resets::new(dir.join("usage"), NOW).pass(false, &feed, &feed, vec![codex(4, NOW + 300)], &[], NOW + 300);
    let day = std::fs::read_to_string(dir.join(format!("usage/resets/{}.tsv", crate::plugins::claudecode::allocation_events::ymd(NOW)))).unwrap();
    assert_eq!(day.lines().count(), 2, "{day}");
}

#[test]
fn while_devops_collects_its_feed_is_the_reading() {
    let dir = marspot_term::test_env::TempDir::new("usage-resets-devops");
    let feed = dir.join("codex-usage.json");
    std::fs::write(&feed, FEED).unwrap();
    let mut r = Resets::new(dir.join("usage"), NOW);
    r.pass(true, &feed, &feed, Vec::new(), &[], NOW);
    let got: Vec<_> = r.store().last().iter().map(|a| (a.name.as_str(), a.profile, a.resets.left_total(), a.resets.source.as_str())).collect();
    assert_eq!(got, [("Codex 1", 1, 4, "devops_feed"), ("Codex 2", 2, 3, "devops_feed")]);
    assert!(r.store().last().iter().all(|a| a.resets.outcome == Outcome::Read));

    // marspot collects again: its own reading replaces the feed's and is compared to it
    r.pass(false, &feed, &feed, vec![codex(4, NOW)], &[], NOW);
    assert_eq!(r.agreed.get("Codex 1"), Some(&Agreement::Same));
    r.pass(false, &feed, &feed, vec![codex(2, NOW + 60)], &[], NOW + 60);
    assert_eq!(r.agreed.get("Codex 1"), Some(&Agreement::Differs));
    assert_eq!(r.store().last()[0].resets.left_total(), 2);
}

fn day_rows(dir: &marspot_term::test_env::TempDir) -> usize {
    let files = std::fs::read_dir(dir.join("usage/resets")).unwrap();
    files.flatten().map(|f| std::fs::read_to_string(f.path()).unwrap().lines().skip(1).count()).sum()
}

#[test]
fn a_restart_after_the_grants_ended_does_not_write_the_same_reading_again() {
    let dir = marspot_term::test_env::TempDir::new("usage-resets-ended");
    let feed = dir.join("none.json");
    Resets::new(dir.join("usage"), NOW).pass(false, &feed, &feed, vec![codex(4, NOW)], &[], NOW);
    // resets.json leaves out the ended grants; the facts still hold them
    let later = 1_796_070_363 + 60;
    Resets::new(dir.join("usage"), later).pass(false, &feed, &feed, vec![codex(4, later)], &[], later);
    assert_eq!(day_rows(&dir), 1);
}

#[test]
fn a_reading_that_cannot_be_recorded_does_not_reach_the_derived_file() {
    let dir = marspot_term::test_env::TempDir::new("usage-resets-unrecorded");
    std::fs::create_dir_all(dir.join("usage")).unwrap();
    // the day files' directory cannot be made
    std::fs::write(dir.join("usage/resets"), "").unwrap();
    Resets::new(dir.join("usage"), NOW).pass(false, &dir.join("none.json"), &dir.join("none.json"), vec![codex(4, NOW)], &[], NOW);
    assert!(!dir.join("usage/resets.json").exists());
}

#[test]
fn day_files_past_fifteen_days_go_at_start_and_on_a_later_day_with_nothing_new() {
    let dir = marspot_term::test_env::TempDir::new("usage-resets-prune");
    let days = dir.join("usage/resets");
    std::fs::create_dir_all(&days).unwrap();
    let ymd = crate::plugins::claudecode::allocation_events::ymd;
    let old = days.join(format!("{}.tsv", ymd(NOW - 15 * 86_400)));
    let kept = days.join(format!("{}.tsv", ymd(NOW - 14 * 86_400)));
    std::fs::write(&old, HEADER).unwrap();
    std::fs::write(&kept, HEADER).unwrap();
    let feed = dir.join("none.json");
    let mut r = Resets::new(dir.join("usage"), NOW);
    assert!(!old.exists() && kept.exists(), "at start");
    r.pass(false, &feed, &feed, vec![codex(4, NOW)], &[], NOW);

    // a day later the same reading writes nothing, and the files still age out
    let next = NOW + 86_400;
    r.pass(false, &feed, &feed, vec![codex(4, next)], &[], next);
    assert!(!kept.exists(), "fifteen days old by then");
    assert_eq!(day_rows(&dir), 1);
}

#[test]
fn a_feed_that_only_counts_credits_gives_way_to_marspots_own_reading() {
    assert!(codex_resets::feed_lacks_grants(FEED), "a count and one end, not each credit");
    let listed = FEED.replacen(r#""tier": "pro","#, r#""tier": "pro", "resets": {"observed_at": 1795719336, "source": "app_server", "grants": []},"#, 2);
    assert!(!codex_resets::feed_lacks_grants(&listed));
    assert!(!codex_resets::feed_lacks_grants("not json"));

    let dir = marspot_term::test_env::TempDir::new("usage-resets-flat");
    let feed = dir.join("codex-usage.json");
    std::fs::write(&feed, FEED).unwrap();
    let mut own = codex(2, NOW);
    own.resets.grants[1].starts_at = Some(1_793_573_597);
    own.resets.grants[1].ends_at = Some(1_796_165_597);
    let mut r = Resets::new(dir.join("usage"), NOW);
    r.pass(true, &feed, &feed, vec![own], &[], NOW);
    let last = r.store().last();
    assert_eq!(last.len(), 1, "only the account read: {last:?}");
    let ends: Vec<_> = last[0].resets.grants.iter().map(|g| g.ends_at).collect();
    assert_eq!(ends, [Some(1_796_070_363), Some(1_796_165_597)], "each credit keeps its own end");
    assert_eq!(last[0].resets.source, "app_server");
}

#[test]
fn claude_resets_from_the_web_usage_page_are_readings_and_not_read_is_none() {
    let body = r#"{"accounts":[
        {"name":"Claude 3","resets":{"observed_at":"2026-11-27T01:00:00Z","source":"web_usage","eligible":true,
         "grants":[{"ref":"claude:3:g1","kind":"fable","restores":["seven_day_overage_included"],"left":1,"total":1,"ends_at":1796500000}]}},
        {"name":"Claude 4","resets":{"observed_at":"2026-11-27T01:00:00Z","source":"not_read","grants":[]}},
        {"name":"Claude 5"}]}"#;
    let got = claude_of_feed(body);
    assert_eq!(got.len(), 1, "{got:?}");
    let a = &got[0];
    assert_eq!((a.agent.as_str(), a.profile, a.resets.source.as_str(), a.resets.left_total()), ("claude", 3, "web_usage", 1));
    assert_eq!(a.resets.grants[0].kind, "fable");

    let dir = marspot_term::test_env::TempDir::new("usage-resets-web");
    let claude = dir.join("claude-usage.json");
    std::fs::write(&claude, body).unwrap();
    let none = dir.join("none.json");
    let mut r = Resets::new(dir.join("usage"), NOW);
    r.pass(false, &none, &claude, Vec::new(), &[], NOW);
    let names: Vec<_> = r.store().last().iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["Claude 3"]);
}

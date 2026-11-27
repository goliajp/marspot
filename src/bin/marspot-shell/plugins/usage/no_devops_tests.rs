//! The collector on a machine without devops, and on one where devops
//! stopped: marspot's own feeds are written and read, and nothing is
//! taken from, compared with or logged about devops.

use super::*;
use marspot_term::test_env::{EnvGuard, TempDir};

/// 2026-11-27 17:20 UTC.
const NOW: i64 = 1_795_800_000;

const CODEX_FEED: &str = include_str!("fixtures/devops-codex-feed.json");

fn claude_feed(generated: i64) -> String {
    format!(
        r#"{{"generated_at":"{}","accounts":[{{"name":"Claude 3","resets":{{"observed_at":"2026-11-27T01:00:00Z","source":"web_usage",
           "grants":[{{"ref":"claude:3:g1","kind":"fable","left":1,"total":1,"ends_at":1796500000}}]}}}}]}}"#,
        iso_utc(generated)
    )
}

#[test]
fn without_devops_marspot_collects_reads_its_own_feeds_and_says_so_once() {
    let dir = TempDir::new("usage-no-devops");
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut env = EnvGuard::new();
    env.set("HOME", &home).set("MARSPOT_STATE_DIR", dir.join("state"));
    let log = crate::test_log::open();

    // the sources the plugin itself starts with
    let sources = Sources { claude: cc_usage::feed_path(), codex: cc_usage::codex_feed_path() };
    assert!(sources.claude.starts_with(&home) && !sources.claude.exists() && !sources.codex.exists());
    let mut c = Collector::new(sources, cc_usage::own_feed_path(), cc_usage::own_codex_feed_path());
    for n in 0..3 {
        c.pass(&home, NOW + n * 300);
    }

    // the body `cc_usage::read` and `read_codex` parse: no account here, so the parse itself says none
    for (devops, own) in [
        (cc_usage::feed_path(), cc_usage::own_feed_path()),
        (cc_usage::codex_feed_path(), cc_usage::own_codex_feed_path()),
    ] {
        let body = std::fs::read_to_string(&own).expect("marspot wrote its own feed");
        assert!(cc_usage::feed_is_fresh(&body, NOW + 600), "{body}");
        assert_eq!(cc_usage::authoritative_body(&devops, &own, NOW + 600), Some(body), "the readers take marspot's own feed");
    }
    assert!(!home.join(".local/state/devops").exists(), "nothing is made where devops would write");

    let text = std::fs::read_to_string(&log).unwrap();
    let said: Vec<&str> = text.lines().filter(|l| l.contains("\tusage.collector\t")).collect();
    assert_eq!(said.len(), 2, "one line per provider, not one per pass: {said:#?}");
    assert!(said.iter().all(|l| l.ends_with("by=marspot")), "{said:#?}");
    let warned: Vec<&str> = text.lines().filter(|l| matches!(l.split('\t').nth(2), Some("WARN" | "ERROR"))).collect();
    assert!(warned.is_empty(), "{warned:#?}");
    assert!(crate::test_log::devops_lines(&text).is_empty(), "{text}");
}

#[test]
fn a_stale_devops_feed_is_no_source_of_resets_and_a_fresh_one_is() {
    let dir = TempDir::new("usage-stale-devops");
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut env = EnvGuard::new();
    env.set("HOME", &home).set("MARSPOT_STATE_DIR", dir.join("state"));
    let log = crate::test_log::open();
    let sources = Sources { claude: dir.join("devops/claude.json"), codex: dir.join("devops/codex.json") };
    std::fs::create_dir_all(dir.join("devops")).unwrap();
    // left behind by a devops that stopped a day ago
    std::fs::write(&sources.codex, CODEX_FEED).unwrap();
    std::fs::write(&sources.claude, claude_feed(NOW - 86_400)).unwrap();
    let mut c = Collector::new(sources, dir.join("usage/claude-usage.json"), dir.join("usage/codex-usage.json"));

    c.pass(&home, NOW);
    assert!(c.resets.store().last().is_empty(), "{:?}", c.resets.store().last());
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(crate::test_log::devops_lines(&text).is_empty(), "{text}");

    // devops writes again: its Claude reading is taken
    std::fs::write(&c.sources.claude, claude_feed(NOW + 240)).unwrap();
    c.pass(&home, NOW + 300);
    let got: Vec<_> = c.resets.store().last().iter().map(|a| (a.name.clone(), a.resets.source.clone())).collect();
    assert_eq!(got, [("Claude 3".to_string(), "web_usage".to_string())]);
}

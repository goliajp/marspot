//! The profiles under `HOME` and the feed's accounts find each other.

use super::*;

/// A profile's `.claude.json` as claude writes it: pretty-printed,
/// the address one level inside `oauthAccount`.
fn claude_json(email: &str) -> String {
    format!(
        "{{\n  \"numStartups\": 12,\n  \"oauthAccount\": {{\n    \"accountUuid\": \"0b0b0b0b-1111-2222-3333-444444444444\",\n    \"emailAddress\": \"{email}\",\n    \"organizationRole\": \"admin\"\n  }},\n  \"userID\": \"x\"\n}}\n"
    )
}

/// The feed and the profiles have to agree about who is who, and
/// the only thing tying them together is the address each profile
/// keeps in its own config.  Built here as the collector and claude
/// lay them out under `HOME`, and read back through the same calls
/// the plugin makes.
#[test]
fn the_profiles_in_home_resolve_to_the_feeds_accounts() {
    let home = marspot_term::test_env::TempDir::new("quota-home");
    let mut env = marspot_term::test_env::EnvGuard::new();
    env.set("HOME", home.path()).set("MARSPOT_STATE_DIR", home.join("state"));
    for (n, email) in [(1u8, "ada@example.jp"), (2, "ben@example.com"), (3, "nobody@example.org")] {
        let dir = home.join(format!(".claude-profile-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".claude.json"), claude_json(email)).unwrap();
    }
    // where marspot writes the feed it collects: no devops on this machine
    let feed = marspot::cc_usage::own_feed_path();
    assert_eq!(feed, home.join("state/usage/claude-usage.json"));
    std::fs::create_dir_all(feed.parent().unwrap()).unwrap();
    std::fs::write(
        &feed,
        r#"{
  "generated_at": "2026-10-09T03:10:00+00:00",
  "accounts": [
{"name": "Claude 1", "email": "ada@example.jp", "status": "allowed",
 "utilization_5h": 0.1, "utilization_7d": 0.2, "reset_5h": 1, "reset_7d": 2,
 "model_limits": [{"label": "Fable", "utilization": 0.5, "reset": null}],
 "credits": {"enabled": false}, "collected_at": "2026-10-09T03:09:00+00:00"},
{"name": "Claude 2", "email": "ben@example.com", "status": "rejected",
 "utilization_5h": 1.0, "utilization_7d": 1.0, "reset_5h": 3, "reset_7d": 4,
 "model_limits": [], "credits": {"enabled": false},
 "collected_at": "2026-10-09T03:09:30+00:00"}
  ]
}"#,
    )
    .unwrap();

    let usage = marspot::cc_usage::read().expect("marspot's own feed parses");
    let profiles = super::super::discover_profiles();
    assert_eq!(profiles, vec![1, 2, 3]);
    let rooms = rooms_for(&profiles, &usage.accounts, |p| profile_email(home.path(), p), 0);
    let status = |n: u8| match &rooms.iter().find(|(p, _)| *p == n).unwrap().1 {
        Room::Known { status, .. } => Some(status.clone()),
        _ => None,
    };
    assert_eq!(status(1).as_deref(), Some("allowed"), "{rooms:?}");
    assert_eq!(status(2).as_deref(), Some("rejected"), "{rooms:?}");
    assert_eq!(status(3), None, "an address the feed does not have: {rooms:?}");
}

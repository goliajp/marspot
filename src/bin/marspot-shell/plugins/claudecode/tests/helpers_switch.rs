//! A helper refused by its account moves to the roomiest one, and its
//! move is reported for the helpers' record.

use super::*;
use super::super::with_helpers::{drain_account_changes, AccountChanged, HelperRoles};

/// P1 refuses; P2 renews first and is half used, so `best_profile`
/// sends every pane there; P3 has the most left.
fn three_accounts(tag: &str) -> TestHome {
    let home = quota_home(tag);
    fs::create_dir_all(home.join(".claude-profile-3/sessions")).unwrap();
    fs::write(
        home.join(".claude-profile-3/.claude.json"),
        format!("{{\n  \"oauthAccount\": {{\n    \"emailAddress\": \"{tag}3@example\"\n  }}\n}}"),
    )
    .unwrap();
    fs::write(
        home.join(".local/state/devops/claude-usage.json"),
        format!(
            r#"{{"generated_at": "2026-09-29T00:00:00+00:00", "accounts": [
                   {{"name":"A","email":"{tag}1@example","status":"rejected","utilization_5h":0.0,
                    "utilization_7d":1.0,"reset_5h":1,"reset_7d":4102444800}},
                   {{"name":"B","email":"{tag}2@example","status":"allowed","utilization_5h":0.0,
                    "utilization_7d":0.5,"reset_5h":1,"reset_7d":4102444900}},
                   {{"name":"C","email":"{tag}3@example","status":"allowed","utilization_5h":0.0,
                    "utilization_7d":0.1,"reset_5h":1,"reset_7d":4102999900}}]}}"#
        ),
    )
    .unwrap();
    home
}

fn target(host: &FakeHost, i: usize) -> String {
    host.sent.lock().unwrap()[i].1.clone()
}

#[test]
fn helpers_refused_on_their_account_move_to_the_roomiest_one() {
    let home = three_accounts("helpers");
    let host = FakeHost::new(home.join("state"));
    let mut plugin = stranded_plugin(pane_on_p1(&home, "u-77"), 77);
    plugin.last_meta.insert(78, pane_on_p1(&home, "u-78"));
    plugin.last_meta.insert(79, pane_on_p1(&home, "u-79"));
    let mut result = scan_of(77, CcActivity::AwaitingUser);
    for sid in [78, 79] {
        result.sessions_seen.push(sid);
        result.new_activity.insert(sid, CcActivity::AwaitingUser);
        result.new_vetoes.insert(sid, (false, false));
        result.new_models.insert(sid, "opus-5-5".into());
    }
    // 77 and 78 are helpers of 50; 79 is an ordinary pane
    result.helpers = HelperRoles::of([(50, 0), (77, 50), (78, 50), (79, 0)]);
    drain_account_changes();
    sweep(&mut plugin, &host, &mut result);

    let mut moved: Vec<u64> = host.submitted.lock().unwrap().iter().map(|(sid, _)| *sid).collect();
    moved.sort();
    assert_eq!(moved, [77, 78, 79]);
    let to = |sid: u64| {
        let i = host.sent.lock().unwrap().iter().position(|(s, _)| *s == sid).unwrap();
        target(&host, i)
    };
    assert!(to(79).contains(".claude-profile-2"), "an ordinary pane goes where quota lapses first: {}", to(79));
    let helpers = [to(77), to(78)];
    assert!(helpers.iter().any(|t| t.contains(".claude-profile-3")), "{helpers:?}");
    assert!(helpers.iter().any(|t| t.contains(".claude-profile-2")), "the second helper is spread: {helpers:?}");

    let mut changes = drain_account_changes();
    changes.sort_by_key(|c| c.sid);
    let got: Vec<(u64, &str, u8)> = changes.iter().map(|c: &AccountChanged| (c.sid, c.agent, c.from)).collect();
    assert_eq!(got, [(77, "claude", 1), (78, "claude", 1)], "only the helpers' moves are reported");
    assert!(changes.iter().all(|c| c.to == 2 || c.to == 3));
}

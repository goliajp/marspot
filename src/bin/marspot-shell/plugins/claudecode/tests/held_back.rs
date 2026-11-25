//! A refused plain pane held back for the Fable panes: its session is
//! sent nothing and its badge stays the binding's; a helper is held
//! back the same, and goes where the allocation's rule for helpers says.

use super::leaving::{rebalance_mode, write_feed};
use super::*;
use super::super::allocation_inputs::snapshot;
use super::super::allocation_view::{HELD_BACK, View};
use super::super::quota::Move;
use super::super::with_helpers::HelperRoles;

/// P1 out of its week and Fable; P2 with little of either left.  Three
/// Fable panes on P1 need what P2 has.
fn feed(home: &Path, tag: &str, p1: &str) {
    let now = rooms::unix_now();
    let row = |n: u8, status: &str, u7: f64, fable: f64, renews: i64| {
        format!(
            r#"{{"name":"{tag}{n}","email":"{tag}{n}@example","status":"{status}","utilization_5h":null,
                "utilization_7d":{u7},"reset_5h":null,"reset_7d":{renews},"tier":"default_claude_max_20x",
                "collected_at":"{}","model_limits":[{{"label":"Fable","utilization":{fable},"reset":{renews}}}]}}"#,
            crate::plugins::usage::iso_utc(now)
        )
    };
    let first = if p1 == "rejected" { row(1, "rejected", 1.0, 1.0, now + 50 * 3600) } else { row(1, "allowed", 0.5, 0.5, now + 50 * 3600) };
    write_feed(home, &[first, row(2, "allowed", 0.94, 0.9, now + 100 * 3600)]);
}

/// Pane `sid` (plain, resting) and three Fable panes on P1; the Fable
/// panes have no confirmed choice, so they count and never move.
fn scene(tag: &str, sid: u64) -> (TestHome, FakeHost, ClaudecodePlugin, ScanResult) {
    let home = quota_home(tag);
    feed(&home, tag, "rejected");
    let host = FakeHost::new(home.join("state"));
    let mut plugin = stranded_plugin(pane_on_p1(&home, &format!("u-{sid}")), sid);
    let mut result = scan_of(sid, CcActivity::AwaitingUser);
    for f in [90u64, 91, 92] {
        plugin.last_meta.insert(f, pane_on_p1(&home, &format!("u-hb-{f}")));
        result.sessions_seen.push(f);
        result.new_models.insert(f, "fable-5-1".into());
        result.new_activity.insert(f, CcActivity::AwaitingUser);
        result.new_vetoes.insert(f, (false, false));
    }
    (home, host, plugin, result)
}

/// What the allocation makes of the scene, as the sweep will see it.
fn view(plugin: &ClaudecodePlugin, r: &mut ScanResult) -> View {
    for (s, m) in &plugin.last_meta {
        r.new_meta.insert(*s, m.clone());
    }
    (r.usage, r.choices) = quota_inputs(&r.new_meta);
    View::of_defaults(&snapshot(r, &HashMap::new(), &HashMap::new(), rooms::unix_now()))
}

fn noted(host: &FakeHost) -> usize {
    host.logged.lock().unwrap().iter().filter(|(e, _)| e == "quota.held_back_noted").count()
}

/// The badge pane `sid` wore last.
fn badge(host: &FakeHost, sid: u64) -> String {
    host.pane_badges.lock().unwrap().iter().rev().find(|(s, _)| *s == sid).map(|(_, b)| b.clone()).unwrap_or_default()
}

/// A sweep, then the looks the same tick publishes.
fn tick(plugin: &mut ClaudecodePlugin, host: &FakeHost, result: &mut ScanResult) {
    sweep(plugin, host, result);
    plugin.publish_looks(host, result);
}

/// Held back, the pane's session is sent nothing: a line in its inbox
/// would start a turn on the account that refuses it.  Its badge stays
/// the binding's, with nothing beside it, and the hold is logged once
/// per episode.
#[test]
fn quota_a_pane_held_back_for_fable_sends_nothing_and_keeps_its_badge() {
    rebalance_mode(marspot::settings::Rebalance::DryRun);
    let (home, host, mut plugin, mut result) = scene("hbt", 79);
    result.new_mapping.insert(79, "P1@opus-5-5".into());
    let v = view(&plugin, &mut result);
    let rooms = rooms::rooms_for(&[1, 2], &marspot::cc_usage::read().unwrap().accounts, |p| Some(format!("hbt{p}@example")), rooms::unix_now());
    assert_eq!(v.must_move(&rooms, 79, 1, crate::plugins::allocation::Class::Plain).0, Move::Hold(HELD_BACK), "the scene holds it back");
    for _ in 0..3 {
        tick(&mut plugin, &host, &mut result);
    }
    assert!(host.submitted.lock().unwrap().is_empty(), "held, not moved");
    assert!(host.sent.lock().unwrap().is_empty(), "nothing typed into the pane");
    assert_eq!(noted(&host), 1, "{:?}", host.logged.lock().unwrap());
    assert_eq!(badge(&host, 79), "P1@opus-5-5");
    let logged = host.logged.lock().unwrap().clone();
    assert!(!logged.iter().any(|(e, _)| e.contains("heads_up")), "no message to the session: {logged:?}");
    assert!(plugin.heads_up.get(&79).is_none());

    // P1 answers again: the episode ends
    feed(&home, "hbt", "allowed");
    tick(&mut plugin, &host, &mut result);
    assert!(!plugin.stranded.contains_key(&79));
    assert_eq!(badge(&host, 79), "P1@opus-5-5");
    feed(&home, "hbt", "rejected");
    tick(&mut plugin, &host, &mut result);
    tick(&mut plugin, &host, &mut result);
    assert_eq!(noted(&host), 2, "a new episode: {:?}", host.logged.lock().unwrap());
    assert_eq!(badge(&host, 79), "P1@opus-5-5");
    assert!(host.submitted.lock().unwrap().is_empty());
    assert!(host.sent.lock().unwrap().is_empty());
    assert!(!host.logged.lock().unwrap().iter().any(|(e, _)| e.contains("heads_up")));
    assert!(plugin.heads_up.get(&79).is_none());
}

/// The allocation holds a plain pane back here; a helper in the same
/// place is held back too: it does not take what the Fable panes are
/// short of.
#[test]
fn helpers_switch_a_helper_is_held_back_for_fable_as_a_pane_is() {
    rebalance_mode(marspot::settings::Rebalance::DryRun);
    let (_home, host, mut plugin, mut result) = scene("hbh", 77);
    result.helpers = HelperRoles::of([(50, 0), (77, 50)]);
    let v = view(&plugin, &mut result);
    let rooms = rooms::rooms_for(&[1, 2], &marspot::cc_usage::read().unwrap().accounts, |p| Some(format!("hbh{p}@example")), rooms::unix_now());
    assert_eq!(v.must_move(&rooms, 77, 1, crate::plugins::allocation::Class::Plain).0, Move::Hold(HELD_BACK));
    sweep(&mut plugin, &host, &mut result);
    assert!(host.submitted.lock().unwrap().is_empty(), "held, not moved: {:?}", host.logged.lock().unwrap());
    assert!(host.sent.lock().unwrap().is_empty());
    assert_eq!(noted(&host), 1, "the helper is held back");
}

/// P1 refuses; P2 has the most room left but renews in a week, P3 less
/// and renews in ten hours.  The roomiest account was P2; the
/// allocation sends a plain pane to P3, whose room would be lost at its
/// renewal, and a helper goes there too.
#[test]
fn helpers_switch_a_helper_goes_where_the_allocation_sends_it() {
    rebalance_mode(marspot::settings::Rebalance::DryRun);
    let tag = "hb3";
    let home = quota_home(tag);
    fs::create_dir_all(home.join(".claude-profile-3/sessions")).unwrap();
    fs::write(home.join(".claude-profile-3/.claude.json"), format!(r#"{{"oauthAccount": {{"emailAddress": "{tag}3@example"}}}}"#)).unwrap();
    let now = rooms::unix_now();
    let row = |n: u8, status: &str, u7: f64, renews: i64| {
        format!(
            r#"{{"name":"{tag}{n}","email":"{tag}{n}@example","status":"{status}","utilization_5h":null,
                "utilization_7d":{u7},"reset_5h":null,"reset_7d":{renews},"tier":"default_claude_max_20x",
                "collected_at":"{}","model_limits":[]}}"#,
            crate::plugins::usage::iso_utc(now)
        )
    };
    write_feed(&home, &[row(1, "rejected", 1.0, now + 50 * 3600), row(2, "allowed", 0.2, now + 160 * 3600), row(3, "allowed", 0.6, now + 10 * 3600)]);
    let host = FakeHost::new(home.join("state"));
    let mut plugin = stranded_plugin(pane_on_p1(&home, "u-78"), 78);
    let mut result = scan_of(78, CcActivity::AwaitingUser);
    let v = view(&plugin, &mut result);
    let accounts = marspot::cc_usage::read().unwrap().accounts;
    let rooms = rooms::rooms_for(&[1, 2, 3], &accounts, |p| Some(format!("{tag}{p}@example")), now);
    assert_eq!(v.must_move(&rooms, 78, 1, crate::plugins::allocation::Class::Plain).0, Move::To(3), "the scene: the allocation sends a plain pane to P3");

    result.helpers = HelperRoles::of([(50, 0), (78, 50)]);
    sweep(&mut plugin, &host, &mut result);
    let sent = host.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "{:?}", host.logged.lock().unwrap());
    assert!(sent[0].1.contains(".claude-profile-3"), "the allocation's choice: {:?}", sent[0].1);
}

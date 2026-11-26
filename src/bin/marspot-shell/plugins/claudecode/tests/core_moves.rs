//! A resting pane moved by the allocation's objective: written down
//! with everything that chose it under `dryrun`, moved under `live`.

use super::*;
use crate::plugins::allocation::replay::replay;
use crate::plugins::claudecode::allocation_adapter::{Allocation, events_dir};
use crate::plugins::claudecode::allocation_inputs::snapshot;

/// 2026-11-05 00:00 UTC, when the feed below was written.
const FED: i64 = 1_793_836_800;

/// The busy pane on P2.
const BESIDE: u64 = 80;

/// P1 renews in 150 hours with 90% of its week left and nobody on it;
/// P2, where the pane is, renews in 30 with 10% left, which the pane and
/// a busy one beside it use up in under eight hours.  The busy one, the
/// lower numbered of the two, stays to use up P2; on P1 all the resting
/// one uses would lapse.
fn feed(home: &Path) {
    let renews = FED + 150 * 3600;
    let renews2 = FED + 30 * 3600;
    fs::write(
        home.join(".local/state/devops/claude-usage.json"),
        format!(
            r#"{{"generated_at": "2026-11-05T00:00:00+00:00", "accounts": [
               {{"name":"A","email":"plan1@example","status":"allowed","utilization_5h":null,
                "utilization_7d":0.10,"reset_5h":null,"reset_7d":{renews},"tier":"default_claude_max_20x",
                "collected_at":"2026-11-05T00:00:00+00:00",
                "model_limits":[{{"label":"Fable","utilization":0.0,"reset":{renews}}}]}},
               {{"name":"B","email":"plan2@example","status":"allowed","utilization_5h":null,
                "utilization_7d":0.90,"reset_5h":null,"reset_7d":{renews2},"tier":"default_claude_max_20x",
                "collected_at":"2026-11-05T00:00:00+00:00",
                "model_limits":[{{"label":"Fable","utilization":0.0,"reset":{renews2}}}]}}]}}"#
        ),
    )
    .unwrap();
}

/// The scan as the worker hands it over: the bindings, the feed, and
/// the allocation's view, the pane watched two and a half hours first so
/// it is past the cooldown that follows a binding.
fn scan(plugin: &ClaudecodePlugin, sid: u64, alloc: &mut Allocation) -> ScanResult {
    let mut result = scan_of(sid, CcActivity::AwaitingUser);
    result.sessions_seen.push(BESIDE);
    result.new_activity.insert(BESIDE, CcActivity::Working);
    result.new_vetoes.insert(BESIDE, (true, false));
    result.new_models.insert(BESIDE, "opus-5-5".into());
    for (s, m) in &plugin.last_meta {
        result.new_meta.insert(*s, m.clone());
    }
    (result.usage, result.choices) = quota_inputs(&result.new_meta);
    let none = HashMap::new();
    alloc.observe(&snapshot(&result, &HashMap::new(), &none, FED - 9000));
    alloc.observe(&snapshot(&result, &HashMap::new(), &none, FED + 60));
    result.alloc = alloc.view.take();
    result
}

#[test]
fn rebalance_dryrun_writes_the_plan_move_with_its_inputs_and_live_moves() {
    let home = quota_home("plan");
    feed(&home);
    let host = FakeHost::new(home.join("state"));
    let sid = 81u64;
    let mut on_p2 = pane_on_p1(&home, "u-81");
    on_p2.profile_num = 2;
    on_p2.config_dir = Some(home.join(".claude-profile-2").to_string_lossy().into_owned());
    let mut plugin = stranded_plugin(on_p2.clone(), sid);
    let mut beside = on_p2;
    beside.uuid = "u-80".into();
    plugin.last_meta.insert(BESIDE, beside);
    // the machine's load holds a resting move; this one is not about it
    plugin.host_load = || None;
    host.status.lock().unwrap().insert(
        sid,
        crate::plugins::PaneStatusView { status: marspot::pane_state::PaneStatus::AwaitingUser, held: Duration::from_secs(600), quiescent: true },
    );
    let mut alloc = Allocation::new(events_dir(&marspot::paths::state_root()));
    let result = scan(&plugin, sid, &mut alloc);
    let (to, why, _) = result.alloc.as_ref().unwrap().rest.get(&sid).cloned().expect("the objective moves it");
    assert_eq!((to, why.word()), (1, "waste"));
    let mode = |m| marspot::settings::set_for_test(marspot::settings::Settings { cc_rebalance: m, ..Default::default() });

    mode(marspot::settings::Rebalance::DryRun);
    plugin.sweep_unusable_profiles(&host, &result);
    assert!(host.submitted.lock().unwrap().is_empty(), "dryrun moves nothing");
    let would: Vec<String> = host.logged.lock().unwrap().iter().filter(|(e, _)| e == "rebalance.would_move").map(|(_, l)| l.clone()).collect();
    assert_eq!(would.len(), 1, "{:?}", host.logged.lock().unwrap());
    // the line carries every input: fed back, the same decision comes out
    let inputs = would[0].split_once(") ").unwrap().1;
    assert_eq!(replay(inputs).unwrap().map(|(t, w)| (t, w.word())), Some((1, "waste")), "{}", would[0]);
    plugin.sweep_unusable_profiles(&host, &result);
    assert_eq!(host.logged.lock().unwrap().iter().filter(|(e, _)| e == "rebalance.would_move").count(), 1, "said once");

    mode(marspot::settings::Rebalance::Off);
    plugin.sweep_unusable_profiles(&host, &result);
    assert!(host.submitted.lock().unwrap().is_empty());

    mode(marspot::settings::Rebalance::Live);
    plugin.focused_sid = Some(sid);
    plugin.sweep_unusable_profiles(&host, &result);
    assert!(host.submitted.lock().unwrap().is_empty(), "not under the user's cursor");
    plugin.focused_sid = None;

    plugin.sweep_unusable_profiles(&host, &result);
    assert_eq!(host.submitted.lock().unwrap().len(), 1, "moved");
    let sent = host.sent.lock().unwrap().clone();
    assert!(sent[0].1.contains(".claude-profile-1"), "to P1: {:?}", sent[0].1);
    assert!(sent[0].1.contains("--model 'claude-opus-5-5'"), "on the model it was on: {:?}", sent[0].1);
    assert!(!sent[0].1.contains(&resume_word()), "nothing to carry on with: {:?}", sent[0].1);
    assert_eq!(plugin.rebalanced.get(&sid).map(|(_, to)| *to), Some(1));
    // the move is a fact with what chose it
    let dir = events_dir(&marspot::paths::state_root());
    let text: String = fs::read_dir(&dir).unwrap().map(|e| fs::read_to_string(e.unwrap().path()).unwrap()).collect();
    let moved: Vec<&str> = text.lines().filter(|l| l.split('\t').nth(3) == Some("moved")).collect();
    assert_eq!(moved.len(), 1, "{text}");
    let detail = moved[0].split('\t').nth(6).unwrap();
    assert!(detail.starts_with("from=P2 why=waste "), "{detail}");
    assert_eq!(replay(detail).unwrap().map(|(t, _)| t), Some(1));

    plugin.sweep_unusable_profiles(&host, &result);
    assert_eq!(host.submitted.lock().unwrap().len(), 1, "not again while it lands");
}

/// The same target held first by the user, then by the machine's load:
/// two waiting lines, not one; the rest ends with one boundary line
/// naming both, and a third look with the same reason adds nothing.
#[test]
fn rebalance_waiting_line_follows_the_reason_and_the_rest_gets_one_boundary_line() {
    // the feed names the accounts by the emails of the "plan" home
    let home = quota_home("plan");
    feed(&home);
    let host = FakeHost::new(home.join("state"));
    let sid = 81u64;
    let mut on_p2 = pane_on_p1(&home, "u-81");
    on_p2.profile_num = 2;
    on_p2.config_dir = Some(home.join(".claude-profile-2").to_string_lossy().into_owned());
    let mut plugin = stranded_plugin(on_p2.clone(), sid);
    let mut beside = on_p2;
    beside.uuid = "u-80".into();
    plugin.last_meta.insert(BESIDE, beside);
    let status = |s| crate::plugins::PaneStatusView { status: s, held: Duration::from_secs(600), quiescent: true };
    host.status.lock().unwrap().insert(sid, status(marspot::pane_state::PaneStatus::AwaitingUser));
    let mut alloc = Allocation::new(events_dir(&marspot::paths::state_root()));
    let result = scan(&plugin, sid, &mut alloc);
    marspot::settings::set_for_test(marspot::settings::Settings { cc_rebalance: marspot::settings::Rebalance::Live, ..Default::default() });
    let lines = |tag: &str| -> Vec<String> { host.logged.lock().unwrap().iter().filter(|(e, _)| e == tag).map(|(_, l)| l.clone()).collect() };

    plugin.focused_sid = Some(sid);
    plugin.host_load = || None;
    plugin.sweep_unusable_profiles(&host, &result);
    plugin.focused_sid = None;
    plugin.host_load = || Some(99.0);
    plugin.sweep_unusable_profiles(&host, &result);
    plugin.sweep_unusable_profiles(&host, &result);
    let waiting = lines("rebalance.waiting");
    assert_eq!(waiting.len(), 2, "{waiting:?}");
    assert!(waiting[0].ends_with("P2 → P1 (waste) waits: the user is in it"), "{waiting:?}");
    assert!(waiting[1].ends_with("P2 → P1 (waste) waits: the machine is overloaded"), "{waiting:?}");
    assert!(host.submitted.lock().unwrap().is_empty());
    assert!(lines("rebalance.boundary").is_empty(), "the rest goes on");

    host.status.lock().unwrap().insert(sid, status(marspot::pane_state::PaneStatus::Unknown));
    plugin.sweep_unusable_profiles(&host, &result);
    let boundary = lines("rebalance.boundary");
    assert_eq!(boundary.len(), 1, "{boundary:?}");
    assert!(boundary[0].starts_with("sid=81 P2 → P1 (waste) rest_s="), "{boundary:?}");
    assert!(boundary[0].contains(" outcome=held why=overloaded seen=user_here@") && boundary[0].contains(",overloaded@"), "{boundary:?}");
}

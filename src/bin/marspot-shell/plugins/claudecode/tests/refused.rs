//! A refusal one pane meets is a fact about its account: the other
//! panes there on the same model leave too, and none is sent there.

use super::*;

/// 2026-11-07 17:39 UTC, after the feed's last measurement.
const AFTER: i64 = 1_794_073_148;
/// Before it.
const BEFORE: i64 = 1_790_000_000;

/// P1 one step from full by the feed, P2 with room, both measured on
/// 2026-11-06. P1's week renews a week earlier, so with nothing else known it is where a resting pane goes.
fn feed(home: &std::path::Path, tag: &str) {
    fs::write(
        home.join(".local/state/devops/claude-usage.json"),
        format!(
            r#"{{"generated_at": "2026-11-06T00:00:00+00:00", "accounts": [
               {{"name":"A","email":"{tag}1@example","status":"allowed","utilization_5h":null,
                "utilization_7d":0.99,"reset_5h":null,"reset_7d":4102444800,"tier":"default_claude_max_20x",
                "collected_at":"2026-11-06T00:00:00+00:00"}},
               {{"name":"B","email":"{tag}2@example","status":"allowed","utilization_5h":null,
                "utilization_7d":0.2,"reset_5h":null,"reset_7d":4103049600,"tier":"default_claude_max_20x",
                "collected_at":"2026-11-06T00:00:00+00:00"}}]}}"#
        ),
    )
    .unwrap();
}

fn rebalance(mode: marspot::settings::Rebalance) {
    marspot::settings::set_for_test(marspot::settings::Settings { cc_rebalance: mode, ..Default::default() });
}

fn confirmed(uuid: &str) {
    fs::write(model_push_dir().join(uuid), "opus-5-5\n/t/x.jsonl\nmedium\npids=1\nid=claude-opus-5-5\n").unwrap();
}

fn refused(home: &std::path::Path, uuid: &str, at: i64, model: &str) -> BindMeta {
    let mut m = pane_on_p1(home, uuid);
    m.refused_at = Some(quota::Refusal { at, model: model.into(), logged_out: false, window: quota::Window::Week });
    m
}

/// Pane 95 on P1 has been refused; pane 96 on P1 is resting and has
/// sent nothing.
fn two_on_p1(tag: &str, at: i64, model: &str) -> (TestHome, FakeHost, ClaudecodePlugin, ScanResult) {
    let home = quota_home(tag);
    feed(&home, tag);
    let host = FakeHost::new(home.join("state"));
    confirmed("u-95");
    confirmed("u-96");
    let mut plugin = stranded_plugin(refused(&home, "u-95", at, model), 95);
    plugin.last_meta.insert(96, pane_on_p1(&home, "u-96"));
    let mut result = scan_of(95, CcActivity::AwaitingUser);
    result.sessions_seen.push(96);
    result.new_activity.insert(96, CcActivity::AwaitingUser);
    result.new_vetoes.insert(96, (false, false));
    result.new_models.insert(96, "opus-5-5".into());
    (home, host, plugin, result)
}

fn moved(host: &FakeHost, sid: u64) -> Vec<String> {
    host.sent.lock().unwrap().iter().filter(|(s, _)| *s == sid).map(|(_, l)| l.clone()).collect()
}

#[test]
fn a_resting_pane_leaves_an_account_another_pane_was_refused_by() {
    rebalance(marspot::settings::Rebalance::Off);
    let (_home, host, mut plugin, mut result) = two_on_p1("rf", AFTER, "");
    sweep(&mut plugin, &host, &mut result);
    sweep(&mut plugin, &host, &mut result);
    let to = moved(&host, 96);
    assert_eq!(to.len(), 1, "the resting pane moves without being spoken to: {to:?}");
    assert!(to[0].contains(".claude-profile-2"), "{to:?}");
    assert_eq!(moved(&host, 95).len(), 1, "and so does the one refused");
}

#[test]
fn a_refusal_older_than_the_feed_moves_nobody() {
    rebalance(marspot::settings::Rebalance::Off);
    let (_home, host, mut plugin, mut result) = two_on_p1("ro", BEFORE, "");
    sweep(&mut plugin, &host, &mut result);
    assert!(host.sent.lock().unwrap().is_empty(), "the feed measured after it and is believed");
}

#[test]
fn a_refusal_naming_another_model_leaves_this_one_alone() {
    rebalance(marspot::settings::Rebalance::Off);
    let (_home, host, mut plugin, mut result) = two_on_p1("rm", AFTER, "fable");
    sweep(&mut plugin, &host, &mut result);
    assert!(moved(&host, 96).is_empty(), "a Fable cap says nothing about Opus");
}

#[test]
fn an_account_that_refused_a_pane_is_no_destination() {
    let room = |util_7d: f64, reset_7d: i64| quota::Room::Known {
        status: "allowed".into(),
        util_5h: 0.0,
        util_7d,
        reset_7d,
        models: Vec::new(),
        models_measured_at: 1_793_836_800,
        measured_at: 1_793_836_800,
    };
    let feed = vec![(1u8, room(0.99, 4102444800)), (2u8, room(0.2, 4103049600))];
    let opus = Some("opus-5-5");
    let target = |seen: &seen::Seen| {
        let mut rooms = feed.clone();
        seen.mark(&mut rooms, &[], AFTER);
        super::super::rebalance::better_home(&rooms, 2, opus).map(|(t, _)| t)
    };

    // with nothing known against it, P1 renews first and is where a resting pane goes
    let mut seen = seen::Seen::default();
    assert_eq!(target(&seen), Some(1));

    // a Fable cap spent there says nothing about Opus
    seen.out.insert((1, "fable".into()), (AFTER, quota::Window::Week));
    assert_eq!(target(&seen), Some(1));

    // once a pane there has been refused outright, it is no destination
    seen.out.insert((1, String::new()), (AFTER, quota::Window::Week));
    assert_eq!(target(&seen), None);

    // until the feed measures it again, after the refusal
    seen.out.clear();
    seen.out.insert((1, String::new()), (BEFORE, quota::Window::Week));
    assert_eq!(target(&seen), Some(1));
}

//! A new Claude helper as the shell starts one: the accounts from the
//! feed under `HOME`, the other panes from the view the last sweep
//! published, a line in `alloc_events` for every choice the core made,
//! and the reserve for the Fable panes.

use std::path::PathBuf;

use marspot_term::test_env::{EnvGuard, TempDir};

use super::super::super::allocation_events::{Event, EventLog};
use super::super::super::allocation_inputs::{PaneIn, Snapshot, accounts_of};
use super::super::super::rooms::unix_now;
use super::*;
use crate::plugins::allocation::choose::{HELPER_LAST_H, held_back};
use crate::plugins::handoff::Agent;
use crate::plugins::usage::for_helpers::{Pick, choose};

/// One feed row: status, week used, Fable used, hours to renewal.
type Row = (&'static str, f64, f64, f64);

/// Profiles 1 and 2 under a fresh `HOME`, with a feed of `rows`
/// written now.
fn home(tag: &str, rows: [Row; 2]) -> (TempDir, EnvGuard, Vec<(u8, PathBuf)>, i64) {
    let dir = TempDir::new(tag);
    let mut env = EnvGuard::new();
    env.set("HOME", dir.path()).set("MARSPOT_STATE_DIR", dir.join("state"));
    let now = unix_now();
    let at = crate::plugins::usage::iso_utc(now);
    let mut accounts = Vec::new();
    let mut homes = Vec::new();
    for (n, (status, u7, fable, h)) in (1u8..).zip(rows) {
        let profile = dir.join(format!(".claude-profile-{n}"));
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(profile.join(".claude.json"), format!("{{\n  \"oauthAccount\": {{\n    \"emailAddress\": \"{tag}{n}@example\"\n  }}\n}}")).unwrap();
        let renews = now + (h * 3600.0) as i64;
        accounts.push(format!(
            r#"{{"name":"{tag}{n}","email":"{tag}{n}@example","status":"{status}","utilization_5h":null,"utilization_7d":{u7},
               "reset_5h":null,"reset_7d":{renews},"collected_at":"{at}","model_limits":[{{"label":"Fable","utilization":{fable},"reset":{renews}}}]}}"#
        ));
        homes.push((n, profile));
    }
    let feed = dir.join(".local/state/devops/claude-usage.json");
    std::fs::create_dir_all(feed.parent().unwrap()).unwrap();
    std::fs::write(feed, format!(r#"{{"generated_at":"{at}","accounts":[{}]}}"#, accounts.join(","))).unwrap();
    (dir, env, homes, now)
}

fn pane(sid: u64, profile: u8, class: Class) -> PaneIn {
    PaneIn { sid, profile, class, marked_at: None, refused_at: None, resting: false, helper: false, out: false, transcript: None }
}

fn helper(sid: u64, profile: u8) -> PaneIn {
    PaneIn { helper: true, ..pane(sid, profile, Class::Plain) }
}

/// The sweep's view at `at` of `panes`, each at its speed (restricted,
/// plain), published for the helpers started after it.
fn publish(tag: &str, at: i64, panes: &[(PaneIn, (f64, f64))]) {
    let usage = marspot::cc_usage::read().expect("the feed under HOME");
    let mails: Vec<(u8, Option<String>)> = (1u8..=2).map(|n| (n, Some(format!("{tag}{n}@example")))).collect();
    let snap = Snapshot { now: at, accounts: accounts_of(&mails, &usage, at), panes: panes.iter().map(|p| p.0.clone()).collect(), ..Snapshot::default() };
    let rates: Vec<(f64, f64)> = panes.iter().map(|p| p.1).collect();
    View::from_rates(&snap, &rates, 0.0).publish();
}

/// A new plain helper started at `now`, as `pick_among` starts one.
fn start(homes: &[(u8, PathBuf)], placed: &mut Placed, now: i64, events: &Path) -> Pick {
    let (accounts, usage) = claude_accounts(&[1, 2], now);
    let scene = claude_scene(&[1, 2], &accounts.rooms, usage.as_ref(), now);
    choose(Agent::Claude, homes, &accounts, &scene, None, placed, now, Some(events))
}

fn number(p: &Pick) -> u8 {
    match p {
        Pick::Account(a) => a.number,
        Pick::NoAccount(_) => panic!("no account: {p:?}"),
    }
}

fn rows(events: &Path, now: i64) -> Vec<Event> {
    EventLog::new(events.to_path_buf()).read_since(now - 3600, now + 10)
}

/// P1 refuses; P2 has a little left, which the three Fable panes on P1
/// are short of.  A refused plain helper would wait; a new one is
/// started on P2 all the same, and its line says what it cost.
#[test]
fn helpers_a_new_helper_held_back_for_fable_starts_where_it_would_go() {
    let (dir, _env, homes, now) = home("hbs", [("rejected", 1.0, 1.0, 50.0), ("allowed", 0.94, 0.9, 100.0)]);
    let fable: Vec<(PaneIn, (f64, f64))> = (90..93).map(|s| (pane(s, 1, Class::Restricted), (RHO0, 0.0))).collect();
    publish("hbs", now - 60, &fable);
    let events = dir.join("events");
    let mut placed = Placed::default();
    assert_eq!(number(&start(&homes, &mut placed, now, &events)), 2);
    assert_eq!(placed.count(2), 1, "counted where it went");
    let got = rows(&events, now);
    assert_eq!(got.len(), 1, "{got:?}");
    let r = &got[0];
    assert_eq!((r.kind.as_str(), r.profile, r.class.as_str(), r.sid), ("held_back_start", Some(2), "plain", 0));
    let inputs = Inputs::decode(&r.detail).expect("the inputs read back");
    let plan = inputs.plan();
    assert_eq!(inputs.decide(&plan), None, "the core holds it back");
    assert_eq!(held_back(&inputs.accounts, &plan, &inputs.loads, inputs.mover, None, HELPER_LAST_H), Some(2));
    assert!(inputs.b_f > 0.0, "the Fable panes' demand is in what it read: {inputs:?}");
}

/// P1 renews first; a busy pane in the last view fills its gap, so the
/// helper goes to P2.  Once a view has that helper's pane in it, the
/// helper is not counted a second time from the tally.  Each choice is
/// one `placed` line whose inputs decide the same again.
#[test]
fn helpers_a_new_helper_avoids_a_busy_account_and_is_counted_once() {
    let (dir, _env, homes, now) = home("hbc", [("allowed", 0.7, 0.1, 30.0), ("allowed", 0.4, 0.1, 120.0)]);
    let (accounts, usage) = claude_accounts(&[1, 2], now);
    let bare = claude_scene(&[1, 2], &accounts.rooms, usage.as_ref(), now);
    assert_eq!(bare.shown, None, "no view yet: no helper pane is known either way");
    let Placement::To(first, _) = place(&bare, Mover { class: Class::Plain, rate: RHO0 }, None, &[]) else { panic!("{bare:?}") };
    assert_eq!(first, 1, "with no other pane known the account that renews first");

    let busy = (pane(5, 1, Class::Plain), (0.0, 0.01));
    publish("hbc", now - 100, std::slice::from_ref(&busy));
    let events = dir.join("events");
    let mut placed = Placed::default();
    assert_eq!(number(&start(&homes, &mut placed, now - 50, &events)), 2, "P1's gap is the busy pane's");

    publish("hbc", now - 10, &[busy, (helper(6, 2), (0.0, RHO0))]);
    let second = number(&start(&homes, &mut placed, now, &events));
    assert_eq!(placed.counts(), [(second, Class::Plain, 1)], "the first is the view's pane now, the second is not yet");

    let got = rows(&events, now);
    assert_eq!(got.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(), ["placed", "placed"], "{got:?}");
    assert_eq!(got.iter().map(|e| e.profile).collect::<Vec<_>>(), [Some(2), Some(second)]);
    for e in &got {
        let inputs = Inputs::decode(&e.detail).expect("the inputs read back");
        assert_eq!(inputs.decide(&inputs.plan()).map(|(to, _)| to), e.profile, "the line decides the same again: {}", e.detail);
    }
    let later = Inputs::decode(&got[1].detail).unwrap();
    let on2 = later.accounts.iter().position(|q| q.id == 2).unwrap();
    assert!((later.loads[on2].plain - RHO0).abs() < 1e-12, "the first helper once, from the view: {later:?}");
    assert!((later.b_n - (0.01 + 2.0 * RHO0)).abs() < 1e-12, "the busy pane, the first helper, the new one: {later:?}");
}

/// A helper's pane shows in a view only after its container, agent and
/// binding: a view taken after it was placed, in the same second or
/// later, that does not show it leaves it counted.  The view that shows
/// it takes it out once, and the helpers already shown before it was
/// placed take nothing.
#[test]
fn helpers_a_placed_helper_counts_until_a_view_shows_its_pane() {
    let (dir, _env, homes, now) = home("hsv", [("allowed", 0.5, 0.1, 100.0), ("allowed", 0.5, 0.1, 100.0)]);
    let events = dir.join("events");
    let old = (helper(3, 1), (0.0, RHO0));
    publish("hsv", now - 100, std::slice::from_ref(&old));
    let mut placed = Placed::default();
    let first = number(&start(&homes, &mut placed, now - 60, &events));
    assert_eq!(placed.counts(), [(first, Class::Plain, 1)], "the helper shown before takes nothing");

    // taken in the very second the first was placed, without its pane
    publish("hsv", now - 60, std::slice::from_ref(&old));
    let second = number(&start(&homes, &mut placed, now - 30, &events));
    let total = |p: &Placed| p.counts().iter().map(|c| c.2).sum::<u32>();
    assert_eq!(total(&placed), 2, "the first is still counted: {:?}", placed.counts());
    publish("hsv", now - 20, std::slice::from_ref(&old));
    let (accounts, usage) = claude_accounts(&[1, 2], now);
    let settle = |placed: &mut Placed| {
        let scene = claude_scene(&[1, 2], &accounts.rooms, usage.as_ref(), now);
        placed.settle(scene.shown.as_deref().unwrap(), now);
        scene
    };
    settle(&mut placed);
    assert_eq!(total(&placed), 2, "a newer view without their panes: {:?}", placed.counts());

    publish("hsv", now - 10, &[old.clone(), (helper(7, first), (0.0, RHO0))]);
    for _ in 0..2 {
        settle(&mut placed);
        assert_eq!(placed.counts(), [(second, Class::Plain, 1)], "the first helper's pane is counted once, from the view");
    }
    let scene = settle(&mut placed);
    let at = scene.accounts.iter().position(|q| q.id == first).unwrap();
    let want = RHO0 * if first == 1 { 2.0 } else { 1.0 };
    assert!((scene.loads[at].plain - want).abs() < 1e-12, "{scene:?}");
}

/// Helpers started before any sweep has published a view stay counted
/// when the first view comes: it holds helper panes older than them, and
/// nothing says which of its panes are theirs.
#[test]
fn helpers_started_before_the_first_view_stay_counted_through_it() {
    let (dir, _env, homes, now) = home("hfv", [("allowed", 0.5, 0.1, 100.0), ("allowed", 0.5, 0.1, 100.0)]);
    let events = dir.join("events");
    let mut placed = Placed::default();
    let a = number(&start(&homes, &mut placed, now - 60, &events));
    let b = number(&start(&homes, &mut placed, now - 50, &events));
    let before = placed.counts();
    assert_eq!(before.iter().map(|c| c.2).sum::<u32>(), 2, "{before:?}");

    publish("hfv", now - 10, &[(helper(3, a), (0.0, RHO0)), (helper(4, b), (0.0, RHO0))]);
    let (accounts, usage) = claude_accounts(&[1, 2], now);
    let scene = claude_scene(&[1, 2], &accounts.rooms, usage.as_ref(), now);
    placed.settle(scene.shown.as_deref().unwrap(), now);
    assert_eq!(placed.counts(), before, "the first view's helper panes take nothing");
}

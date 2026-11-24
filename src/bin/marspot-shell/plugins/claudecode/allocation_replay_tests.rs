//! The dry run's replay: every `would_move` line the shell writes,
//! fed back to the pure decision, comes out the same, and the view the
//! main thread acts on carries the same decision for each resting pane.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use super::*;
use crate::plugins::allocation::Class;
use crate::plugins::allocation::replay::replay;
use crate::plugins::claudecode::allocation_inputs::{AccountIn, PaneIn, Snapshot};
use crate::plugins::claudecode::allocation_view::View;

const NOW: i64 = 1_795_530_644;
const H: i64 = 3600;

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("marspot-alloc-replay-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    d
}

fn would_moves(d: &Path) -> Vec<(u64, String)> {
    let mut names: Vec<PathBuf> = fs::read_dir(d).map(|r| r.map(|e| e.unwrap().path()).collect()).unwrap_or_default();
    names.sort();
    names
        .iter()
        .flat_map(|p| fs::read_to_string(p).unwrap().lines().map(str::to_string).collect::<Vec<_>>())
        .filter(|l| l.split('\t').nth(3) == Some("would_move"))
        .map(|l| (l.split('\t').nth(2).unwrap().parse().unwrap(), l.split('\t').nth(6).unwrap().to_string()))
        .collect()
}

fn account(profile: u8, week_used: f64, fable_used: f64, renews_h: i64) -> AccountIn {
    let at = NOW + renews_h * H;
    AccountIn { profile, week_used, fable_used: Some(fable_used), fable_known: true, week_renews: at, fable_renews: at, open: true, measured_at: NOW }
}

fn pane(sid: u64, profile: u8, class: Class, resting: bool, out: bool) -> PaneIn {
    PaneIn { sid, profile, class, marked_at: None, refused_at: None, resting, helper: false, out, transcript: None }
}

/// Every account a pane might go to and every pane on them, watched
/// two and a half hours and then decided: past the cooldown after a
/// binding, short of the half-life that makes a speed measured.
fn run(name: &str, accounts: Vec<AccountIn>, panes: Vec<PaneIn>) -> (Vec<(u64, String)>, Option<View>) {
    let d = dir(name);
    let mut a = Allocation::new(d.clone());
    let live: Vec<u64> = panes.iter().map(|p| p.sid).collect();
    let at = |now: i64| {
        let mut accounts = accounts.clone();
        for ac in &mut accounts {
            ac.measured_at = now;
        }
        Snapshot { now, feed_at: Some(now), accounts, panes: panes.clone(), live: live.clone() }
    };
    a.observe(&at(NOW - 9000));
    a.observe(&at(NOW));
    let out = (would_moves(&d), a.view.take());
    fs::remove_dir_all(&d).unwrap();
    out
}

#[test]
fn allocation_replay_of_every_would_move_is_the_same_decision() {
    let plain = Class::Plain;
    let fable = Class::Restricted;
    let scenes = [
        // refused on a spent P1: rule (a)
        ("out", vec![account(1, 1.0, 1.0, 40), account(2, 0.3, 0.0, 60)], vec![pane(1, 1, plain, false, true)]),
        // more quota than use: the pane's use goes where it would lapse
        ("plan", vec![account(1, 0.1, 0.0, 50), account(2, 0.1, 0.0, 300)], vec![pane(1, 2, plain, true, false)]),
        // P1 runs out within the hour: the faster of its two panes leaves
        ("leave", vec![account(1, 0.995, 1.0, 60), account(2, 0.3, 1.0, 90)], vec![pane(1, 1, plain, true, false), pane(2, 1, plain, true, false)]),
        // what Fable will not take on P1 is gone: the plain pane leaves it
        (
            "u_out",
            vec![account(1, 0.94, 0.0, 100), account(2, 0.5, 1.0, 120)],
            vec![pane(1, 1, plain, true, false), pane(2, 1, fable, false, false), pane(3, 1, fable, false, false)],
        ),
    ];
    let mut whys = BTreeSet::new();
    for (name, accounts, panes) in scenes {
        let (lines, view) = run(name, accounts, panes);
        assert!(!lines.is_empty(), "{name}: a would-move: {view:?}");
        let view = view.expect("a feed, a view");
        for (sid, detail) in &lines {
            let to: u8 = detail.split(' ').find_map(|w| w.strip_prefix("to=P")).unwrap().parse().unwrap();
            let why = detail.split(' ').find_map(|w| w.strip_prefix("why=")).unwrap();
            assert_eq!(replay(detail).unwrap().map(|(t, w)| (t, w.word())), Some((to, why)), "{name}: {detail}");
            if why != "out" {
                let (t, w, inputs) = view.rest.get(sid).expect("the view carries it");
                assert_eq!((*t, w.word()), (to, why), "{name}");
                assert_eq!(&replay(&inputs.encode()).unwrap(), &Some((*t, *w)), "{name}");
            }
            whys.insert(why.to_string());
        }
    }
    assert_eq!(whys.into_iter().collect::<Vec<_>>(), ["leave_early", "out", "plan", "u_out"]);
}

/// The view's resting moves are this scan's only: what it decided and
/// nothing older, so it does not grow with the session.
#[test]
fn allocation_view_holds_this_scans_resting_moves_only() {
    let d = dir("bounded");
    let mut a = Allocation::new(d.clone());
    let panes = vec![pane(1, 2, Class::Plain, true, false)];
    let at = |now: i64, panes: Vec<PaneIn>| Snapshot {
        now,
        feed_at: Some(now),
        accounts: vec![account(1, 0.1, 0.0, 50), account(2, 0.1, 0.0, 300)].into_iter().map(|mut ac| { ac.measured_at = now; ac }).collect(),
        live: panes.iter().map(|p| p.sid).collect(),
        panes,
    };
    a.observe(&at(NOW - 9000, panes.clone()));
    a.observe(&at(NOW, panes.clone()));
    assert_eq!(a.view.take().map(|v| v.rest.len()), Some(1));
    let busy = vec![pane(1, 2, Class::Plain, false, false)];
    a.observe(&at(NOW + 60, busy));
    assert_eq!(a.view.take().map(|v| v.rest.len()), Some(0), "busy now: nothing to act on");
    fs::remove_dir_all(&d).unwrap();
}

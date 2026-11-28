//! The main thread's choices through the allocation core: a refused
//! pane, a badge click, a pane leaving early.  These replace the old
//! ranking's tests of 2026-11-05 and 11-20 (`better_home`,
//! `best_profile` for Claude).


use marspot::cc_usage::{CcAccount, CcModelLimit, CcUsage};

use super::*;
use crate::plugins::allocation::replay::replay;
use crate::plugins::claudecode::allocation_inputs::{PaneIn, accounts_of};
use crate::plugins::claudecode::rooms::rooms_for;

const NOW: i64 = 1_795_158_300;

fn row(n: u8, status: &str, u7: f64, reset: i64, fable: f64) -> CcAccount {
    CcAccount {
        name: format!("Claude {n}"),
        email: format!("p{n}"),
        status: status.into(),
        util_5h: None,
        util_7d: Some(u7),
        reset_5h: None,
        reset_7d: Some(reset),
        model_limits: vec![CcModelLimit { label: "Fable".into(), util: Some(fable), reset: Some(reset) }],
        collected_at: NOW - 5,
        limits_as_of: None,
        tier: "default_claude_max_20x".into(),
    }
}

fn pane(sid: u64, profile: u8, class: Class) -> PaneIn {
    PaneIn { sid, profile, class, marked_at: None, refused_at: None, resting: true, helper: false, out: false, transcript: None }
}

/// The view and the rooms of `feed`, with `panes` on it.
fn scene(feed: Vec<CcAccount>, panes: Vec<PaneIn>) -> (View, Vec<(u8, Room)>) {
    let ids: Vec<u8> = feed.iter().map(|a| a.email[1..].parse().unwrap()).collect();
    let mails: Vec<(u8, Option<String>)> = ids.iter().map(|p| (*p, Some(format!("p{p}")))).collect();
    let usage = CcUsage { generated_at: NOW, accounts: feed };
    let rooms = rooms_for(&ids, &usage.accounts, |p| Some(format!("p{p}")), NOW);
    let live = panes.iter().map(|p| p.sid).collect();
    let snap = Snapshot { now: NOW, feed_at: Some(NOW), accounts: accounts_of(&mails, &usage, NOW), panes, live };
    (View::of_defaults(&snap), rooms)
}

/// The feed at 2026-11-20 05:05 UTC, cut to three accounts: P3's Fable
/// is spent, its week at 99% and refusing.  A refused Opus pane on P8
/// goes to P5, never to P3, though P3 renews first.
#[test]
fn allocation_view_refused_pane_never_goes_to_an_account_that_is_out() {
    let feed = vec![
        row(3, "rejected", 0.99, 1_795_323_599, 1.00),
        row(5, "allowed", 0.00, 1_795_658_399, 0.00),
        row(8, "allowed", 0.61, 1_795_582_799, 0.67),
    ];
    let (view, rooms) = scene(feed, vec![pane(1, 8, Class::Plain)]);
    let (to, inputs) = view.must_move(&rooms, 1, 8, Class::Plain);
    assert_eq!(to, Move::To(5));
    // what chose it, fed back, chooses the same
    let line = inputs.unwrap().encode();
    assert_eq!(replay(&line).unwrap().map(|(t, _)| t), Some(5));
}

/// An account the main thread has seen refuse since the feed was
/// written is shut for the choice, though the view still reads it open.
#[test]
fn allocation_view_lays_the_main_threads_refusals_over_the_feed() {
    let feed = vec![row(1, "allowed", 0.30, NOW + 50 * 3600, 0.0), row(2, "allowed", 0.30, NOW + 60 * 3600, 0.0), row(3, "allowed", 0.30, NOW + 90 * 3600, 0.0)];
    let (view, mut rooms) = scene(feed, vec![pane(1, 1, Class::Plain)]);
    assert_eq!(view.must_move(&rooms, 1, 1, Class::Plain).0, Move::To(2), "the earliest renewal short of its target");
    if let Room::Known { status, .. } = &mut rooms[1].1 {
        *status = "rejected".into();
    }
    assert_eq!(view.must_move(&rooms, 1, 1, Class::Plain).0, Move::To(3));
    rooms[2].1 = Room::Unknown;
    assert_eq!(view.must_move(&rooms, 1, 1, Class::Plain).0, Move::Hold(ALL_OUT), "an unknown account is no target");
    assert_eq!(view.must_move(&rooms[..1], 1, 1, Class::Plain).0, Move::Hold(NOWHERE), "one account");
}

/// A Fable pane refused goes only where Fable is left; with Fable
/// spent everywhere else it stays, and is not given another model.
#[test]
fn allocation_view_fable_pane_goes_only_where_fable_is_left() {
    let feed = vec![row(1, "allowed", 0.30, NOW + 50 * 3600, 1.0), row(2, "allowed", 0.30, NOW + 40 * 3600, 1.0), row(3, "allowed", 0.50, NOW + 90 * 3600, 0.2)];
    let (view, rooms) = scene(feed, vec![pane(1, 1, Class::Restricted)]);
    assert_eq!(view.must_move(&rooms, 1, 1, Class::Restricted).0, Move::To(3), "P2 renews first but its Fable is spent");
    assert_eq!(view.must_move(&rooms[..2], 1, 1, Class::Restricted).0, Move::Hold(ALL_OUT));
}

/// A refused plain pane is not held back for the Fable panes: with only
/// P2 left it goes there, as a badge click sends it.
#[test]
fn allocation_view_refused_plain_pane_and_badge_click_go_where_there_is_room() {
    let feed = vec![row(1, "rejected", 1.0, NOW + 50 * 3600, 1.0), row(2, "allowed", 0.94, NOW + 100 * 3600, 0.9)];
    let panes = vec![pane(1, 1, Class::Plain), pane(2, 1, Class::Restricted), pane(3, 1, Class::Restricted), pane(4, 1, Class::Restricted)];
    let (view, rooms) = scene(feed, panes);
    assert_eq!(view.must_move(&rooms, 1, 1, Class::Plain).0, Move::To(2));
    assert_eq!(view.by_hand(&rooms, 1, 1, Class::Plain, None).map(|(t, _)| t), Some(2));
}

/// Leaving early ranks by the one objective among the accounts it is
/// handed, near ties to the earlier renewal,
/// and the panes sent this pass count where they went.
#[test]
fn allocation_view_leaving_early_ranks_among_the_lasting_and_counts_placed_panes() {
    let feed = vec![row(1, "allowed", 0.99, NOW + 50 * 3600, 1.0), row(2, "allowed", 0.30, NOW + 60 * 3600, 1.0), row(3, "allowed", 0.30, NOW + 90 * 3600, 1.0)];
    let (mut view, rooms) = scene(feed, vec![pane(1, 1, Class::Plain), pane(2, 1, Class::Plain)]);
    assert_eq!(view.rank_among(&rooms, 1, 1, Class::Plain, &[2, 3]).map(|(t, _)| t), Some(2));
    assert_eq!(view.rank_among(&rooms, 1, 1, Class::Plain, &[3]).map(|(t, _)| t), Some(3), "only what it is handed");
    assert_eq!(view.rank_among(&rooms, 1, 1, Class::Plain, &[]).map(|(t, _)| t), None);
    view.placed(1, 2);
    assert_eq!(view.panes.iter().find(|p| p.0 == 1).map(|p| p.1), Some(2));
}

/// A badge click between scans reads the accounts now and the panes as
/// the last scan had them.
#[test]
fn allocation_view_for_click_keeps_the_last_scans_panes() {
    let feed = vec![row(1, "allowed", 0.30, NOW + 50 * 3600, 0.0), row(2, "allowed", 0.30, NOW + 60 * 3600, 0.0)];
    let (last, _) = scene(feed.clone(), vec![pane(1, 1, Class::Plain), pane(2, 2, Class::Restricted)]);
    let mails: Vec<(u8, Option<String>)> = vec![(1, Some("p1".into())), (2, Some("p2".into()))];
    let usage = CcUsage { generated_at: NOW, accounts: feed };
    let v = View::for_click(NOW, accounts_of(&mails, &usage, NOW), Some(&last));
    assert_eq!(v.panes, last.panes);
    assert_eq!(v.accounts, last.accounts);
    let none = View::for_click(NOW, accounts_of(&mails, &usage, NOW), None);
    assert!(none.panes.is_empty());
}

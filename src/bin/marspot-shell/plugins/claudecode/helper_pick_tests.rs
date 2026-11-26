//! Which account a helper goes to, and when the tally starts again.

use super::placed_tally::SHOWN_WITHIN_S;
use super::*;
use crate::plugins::allocation::helper::{Placement, place};

fn room(week_used: f64, five_used: f64) -> Room {
    Room::Known {
        status: "allowed".into(),
        util_5h: Some(five_used),
        util_7d: Some(week_used),
        reset_7d: 2_000_000_000,
        models: vec![("fable".into(), Some(0.1))],
        models_measured_at: 1,
        measured_at: 1,
    }
}

fn fable_spent_room() -> Room {
    let mut r = room(0.1, 0.0);
    if let Room::Known { models, .. } = &mut r {
        *models = vec![("fable".into(), Some(1.0))];
    }
    r
}

fn unread_room() -> Room {
    let mut r = room(0.0, 0.0);
    if let Room::Known { util_7d, .. } = &mut r {
        *util_7d = None;
    }
    r
}

fn logged_out_room() -> Room {
    let mut r = room(0.1, 0.0);
    if let Room::Known { status, .. } = &mut r {
        *status = "auth_error".into();
    }
    r
}

#[test]
fn quota_a_new_feed_clears_what_was_placed() {
    let mut placed = Placed::default();
    placed.feed(100, 1_000);
    placed.note(1, Class::Plain, 1);
    placed.note(1, Class::Restricted, 1);
    assert_eq!(placed.count(1), 2);
    assert_eq!(placed.counts(), [(1, Class::Plain, 1), (1, Class::Restricted, 1)]);
    placed.feed(100, 1_000);
    assert_eq!(placed.count(1), 2, "the same feed read again changes nothing");
    placed.feed(40, 1_000);
    assert_eq!(placed.count(1), 2, "nor does an older one arriving late");
    placed.feed(160, 1_000);
    assert_eq!(placed.count(1), 0);
}

/// A helper pane seen for the first time, or on another account, takes
/// the oldest entry of its account and class, else of its account; one
/// seen before takes nothing; an entry no pane took in fifteen minutes
/// goes.
#[test]
fn helper_pick_settle_takes_one_entry_per_pane_shown() {
    let mut placed = Placed::default();
    placed.settle(&[(1, 1, Class::Plain)], 1_000);
    placed.note(1, Class::Plain, 1_000);
    placed.note(1, Class::Restricted, 1_001);
    placed.note(2, Class::Plain, 1_002);
    placed.settle(&[(1, 1, Class::Plain)], 1_010);
    assert_eq!(placed.counts().len(), 3, "the pane shown before takes nothing");
    placed.settle(&[(1, 1, Class::Plain), (5, 1, Class::Restricted)], 1_020);
    assert_eq!(placed.counts(), [(1, Class::Plain, 1), (2, Class::Plain, 1)]);
    placed.settle(&[(1, 2, Class::Restricted), (5, 1, Class::Restricted)], 1_030);
    assert_eq!(placed.counts(), [(1, Class::Plain, 1)], "a pane moved to P2 takes P2's entry of another class");
    placed.settle(&[], 1_000 + SHOWN_WITHIN_S);
    assert!(placed.counts().is_empty(), "a helper that never showed");
}

/// A helper pane a view leaves out, while its profile is being switched
/// or its agent restarted, is still the same pane when the next view
/// shows it again on the same account: it takes no other entry.  One
/// unseen for longer than a pane may take to show is forgotten.
#[test]
fn helper_pick_a_pane_left_out_of_a_view_is_not_new_when_it_comes_back() {
    let mut placed = Placed::default();
    placed.settle(&[], 1_000);
    placed.settle(&[(7, 1, Class::Plain)], 1_010);
    placed.settle(&[], 1_020);
    placed.note(1, Class::Plain, 1_025);
    placed.settle(&[(7, 1, Class::Plain)], 1_030);
    assert_eq!(placed.counts(), [(1, Class::Plain, 1)], "pane 7 came back, the new helper is not its");

    placed.settle(&[], 1_030 + SHOWN_WITHIN_S + 1);
    placed.note(1, Class::Plain, 1_030 + SHOWN_WITHIN_S + 1);
    placed.settle(&[(7, 1, Class::Plain)], 1_030 + SHOWN_WITHIN_S + 2);
    assert!(placed.counts().is_empty(), "unseen too long, pane 7 counts as new: {:?}", placed.counts());
}

const NOW: i64 = 1_795_530_644;

fn renewing(week_used: f64, hours: f64) -> Room {
    let mut r = room(week_used, 0.0);
    if let Room::Known { reset_7d, measured_at, .. } = &mut r {
        *reset_7d = NOW + (hours * 3600.0) as i64;
        *measured_at = NOW - 60;
    }
    r
}

#[test]
fn helpers_the_core_reads_only_the_accounts_the_feed_reads() {
    let rooms = vec![(1, renewing(0.16, 46.6)), (2, unread_room()), (3, Room::Unknown), (4, logged_out_room())];
    let s = rooms_scene(&rooms, &[], None, NOW);
    let ids: Vec<u8> = s.accounts.iter().map(|q| q.id).collect();
    assert_eq!(ids, [1, 4]);
    let q = &s.accounts[0];
    assert!((q.r - 0.84).abs() < 1e-9 && (q.t - 46.6).abs() < 0.01 && q.open && !q.stale);
    assert_eq!(q.f, q.r, "no reserve: the model cap only says whether it may go");
    assert!(!s.accounts[1].open, "logged out");
    assert_eq!(s.kappa, None);
    let spent = vec![(5, fable_spent_room())];
    assert_eq!(rooms_scene(&spent, &[], Some("fable-5"), NOW).accounts[0].f, 0.0, "its own cap is spent");
    assert!(rooms_scene(&spent, &[], None, NOW).accounts[0].f > 0.0);
}

#[test]
fn helpers_with_nothing_read_are_spread_unread_first_then_uncovered() {
    let rooms = vec![(1, Room::Unknown), (2, unread_room()), (3, unread_room()), (4, room(1.0, 0.0))];
    let mut placed = Placed::default();
    let mut got = Vec::new();
    for _ in 0..5 {
        let to = unread_pick(&rooms, None, None, &placed).unwrap();
        placed.note(to, Class::Plain, 1);
        got.push(to);
    }
    assert_eq!(got, [2, 3, 2, 3, 2], "an account the feed covers comes before one it does not");
    let none_read = vec![(1, Room::Unknown), (2, Room::Unknown)];
    assert_eq!(unread_pick(&none_read, None, None, &Placed::default()), Some(1), "no feed is not no account");
    assert_eq!(unread_pick(&none_read, Some(1), None, &Placed::default()), None, "a refused helper is never sent where the feed cannot speak");
    assert_eq!(unread_pick(&rooms, Some(2), None, &Placed::default()), Some(3), "nor back to the account that refused it");
}

#[test]
fn helpers_refused_one_after_another_are_moved_to_different_accounts() {
    // P2 renews first: rule (a) would send both there
    let rooms = vec![(1, renewing(1.0, 30.0)), (2, renewing(0.9, 20.0)), (3, renewing(0.5, 100.0))];
    let mut scene = rooms_scene(&rooms, &[], None, NOW);
    scene.b_n = RHO0;
    let mut placed = Placed::default();
    let mut got = Vec::new();
    for _ in 0..2 {
        let Placement::To(to, inputs) = place(&scene, Mover { class: Class::Plain, rate: RHO0, tokens: None }, Some(1), &placed.counts()) else { panic!() };
        assert_eq!(inputs.decide(&inputs.plan()).map(|(t, _)| t), Some(to), "the logged inputs give the same answer");
        placed.note(to, Class::Plain, 1);
        got.push(to);
    }
    assert_eq!(got, [2, 3]);
    let all_out = vec![(1, renewing(1.0, 30.0)), (2, renewing(1.0, 10.0))];
    assert_eq!(place(&rooms_scene(&all_out, &[], None, NOW), Mover { class: Class::Plain, rate: RHO0, tokens: None }, Some(1), &[]), Placement::Nowhere);
}

#[test]
fn helpers_on_a_model_with_a_cap_of_its_own_are_placed_as_restricted() {
    let rooms = vec![(1, room(0.2, 0.0))];
    assert_eq!(class_in(&rooms, Some("fable-5")), Class::Restricted);
    assert_eq!(class_in(&rooms, Some("opus-5-5")), Class::Plain);
    assert_eq!(class_in(&rooms, None), Class::Plain);
    assert_eq!(claude_class(Some("fable-5")), Class::Restricted);
    assert_eq!(claude_class(Some("opus-5-5")), Class::Plain);
}

#[test]
fn quota_a_feed_stamped_ahead_of_the_clock_does_not_freeze_the_tally() {
    let mut placed = Placed::default();
    placed.feed(100, 110);
    placed.note(1, Class::Plain, 1);
    placed.feed(9_000, 120);
    assert_eq!(placed.count(1), 0, "it is a new feed");
    placed.note(1, Class::Plain, 1);
    placed.feed(9_000, 130);
    assert_eq!(placed.count(1), 1, "the same feed read again");
    placed.feed(100, 140);
    assert_eq!(placed.count(1), 1, "an older one arriving late");
    placed.feed(180, 190);
    assert_eq!(placed.count(1), 0, "the next feed, stamped right");
    placed.note(1, Class::Plain, 1);
    placed.feed(240, 250);
    assert_eq!(placed.count(1), 0);
}

#[test]
fn quota_a_clock_set_back_does_not_freeze_the_tally() {
    let mut placed = Placed::default();
    placed.feed(9_000, 9_000);
    placed.note(1, Class::Plain, 1);
    placed.feed(9_000, 200);
    assert_eq!(placed.count(1), 1, "the same feed read again");
    placed.feed(190, 205);
    assert_eq!(placed.count(1), 1, "written before the clock was seen set back");
    placed.feed(250, 300);
    assert_eq!(placed.count(1), 0, "the next feed, read a while after it was written");
    placed.note(1, Class::Plain, 1);
    placed.feed(190, 310);
    assert_eq!(placed.count(1), 1, "an older one arriving late");
    placed.feed(310, 310);
    assert_eq!(placed.count(1), 0);
    placed.note(1, Class::Plain, 1);
    placed.feed(310, 310);
    placed.feed(309, 310);
    assert_eq!(placed.count(1), 1, "within the second it was read");
    placed.feed(240, 240);
    assert_eq!(placed.count(1), 0, "read as it is written, the clock just set back");
}

#[test]
fn quota_a_spent_account_says_when_it_comes_back() {
    let resets = Resets { five_hour: 500, week: 9_000, models: vec![("fable".into(), 7_000)] };
    assert_eq!(recovers_at(&room(1.0, 0.99), &resets, None, 100), Some(9_000));
    assert_eq!(recovers_at(&room(0.5, 0.99), &resets, None, 100), Some(500));
    let fable_spent = fable_spent_room();
    assert_eq!(recovers_at(&fable_spent, &resets, Some("fable-5"), 100), Some(7_000));
    assert_eq!(recovers_at(&room(0.5, 0.99), &resets, None, 600), None, "an end that has passed says nothing");
    let logged_out = logged_out_room();
    assert_eq!(recovers_at(&logged_out, &resets, None, 100), None);
}

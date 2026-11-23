//! Which account a helper goes to, and when the tally starts again.

use super::*;

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
fn helpers_go_to_the_account_with_the_most_room_whatever_renews_first() {
    // P1 renews first and is fuller: `best_profile` would pick it
    let mut fuller = room(0.6, 0.1);
    if let Room::Known { reset_7d, .. } = &mut fuller {
        *reset_7d = 1_000;
    }
    let rooms = vec![(1, fuller), (2, room(0.2, 0.1)), (3, room(0.4, 0.0))];
    assert_eq!(roomiest(&rooms, &Placed::default(), None), Some(2));
    assert_eq!(roomiest(&rooms, &Placed::default(), Some("opus-5-5")), Some(2));
}

#[test]
fn helpers_do_not_leave_out_the_callers_own_account() {
    let rooms = vec![(4, room(0.1, 0.0))];
    assert_eq!(roomiest(&rooms, &Placed::default(), None), Some(4));
}

#[test]
fn quota_spreads_a_batch_of_five_helpers_over_five_accounts() {
    let rooms: Vec<(u8, Room)> = (1..=8).map(|n| (n, room(0.1 + f64::from(n) * 0.05, 0.0))).collect();
    let mut placed = Placed::default();
    placed.feed(100, 1_000);
    let mut got = Vec::new();
    for _ in 0..5 {
        let to = roomiest(&rooms, &placed, None).unwrap();
        placed.note(to);
        got.push(to);
    }
    assert_eq!(got, [1, 2, 3, 4, 5], "each to the roomiest account nobody was just placed on");
}

#[test]
fn quota_a_batch_on_two_accounts_takes_turns() {
    let rooms = vec![(1, room(0.1, 0.0)), (2, room(0.3, 0.0))];
    let mut placed = Placed::default();
    let got: Vec<u8> = (0..4)
        .map(|_| {
            let to = roomiest(&rooms, &placed, None).unwrap();
            placed.note(to);
            to
        })
        .collect();
    assert_eq!(got, [1, 2, 1, 2]);
}

#[test]
fn quota_a_new_feed_clears_what_was_placed() {
    let rooms = vec![(1, room(0.1, 0.0)), (2, room(0.3, 0.0))];
    let mut placed = Placed::default();
    placed.feed(100, 1_000);
    placed.note(1);
    assert_eq!(roomiest(&rooms, &placed, None), Some(2));
    placed.feed(100, 1_000);
    assert_eq!(placed.count(1), 1, "the same feed read again changes nothing");
    placed.feed(40, 1_000);
    assert_eq!(placed.count(1), 1, "nor does an older one arriving late");
    placed.feed(160, 1_000);
    assert_eq!(placed.count(1), 0);
    assert_eq!(roomiest(&rooms, &placed, None), Some(1));
}

#[test]
fn quota_a_feed_stamped_ahead_of_the_clock_does_not_freeze_the_tally() {
    let mut placed = Placed::default();
    placed.feed(100, 110);
    placed.note(1);
    placed.feed(9_000, 120);
    assert_eq!(placed.count(1), 0, "it is a new feed");
    placed.note(1);
    placed.feed(9_000, 130);
    assert_eq!(placed.count(1), 1, "the same feed read again");
    placed.feed(100, 140);
    assert_eq!(placed.count(1), 1, "an older one arriving late");
    placed.feed(180, 190);
    assert_eq!(placed.count(1), 0, "the next feed, stamped right");
    placed.note(1);
    placed.feed(240, 250);
    assert_eq!(placed.count(1), 0);
}

#[test]
fn quota_a_clock_set_back_does_not_freeze_the_tally() {
    let mut placed = Placed::default();
    placed.feed(9_000, 9_000);
    placed.note(1);
    placed.feed(9_000, 200);
    assert_eq!(placed.count(1), 1, "the same feed read again");
    placed.feed(190, 205);
    assert_eq!(placed.count(1), 1, "written before the clock was seen set back");
    placed.feed(250, 300);
    assert_eq!(placed.count(1), 0, "the next feed, read a while after it was written");
    placed.note(1);
    placed.feed(190, 310);
    assert_eq!(placed.count(1), 1, "an older one arriving late");
    placed.feed(310, 310);
    assert_eq!(placed.count(1), 0);
    placed.note(1);
    placed.feed(310, 310);
    placed.feed(309, 310);
    assert_eq!(placed.count(1), 1, "within the second it was read");
    placed.feed(240, 240);
    assert_eq!(placed.count(1), 0, "read as it is written, the clock just set back");
}

#[test]
fn quota_a_refused_pick_is_no_account() {
    let rooms = vec![(1, room(1.0, 0.0)), (2, room(0.5, 0.99))];
    assert_eq!(roomiest(&rooms, &Placed::default(), None), None);
    let fable_spent = fable_spent_room();
    let rooms = vec![(3, fable_spent)];
    assert_eq!(roomiest(&rooms, &Placed::default(), Some("fable-5-1")), None);
    assert_eq!(roomiest(&rooms, &Placed::default(), Some("opus-5-5")), Some(3), "only Fable is spent there");
    assert_eq!(roomiest(&[], &Placed::default(), None), None);
}

#[test]
fn quota_an_account_nearly_out_or_unread_comes_after_one_known_to_have_room() {
    let unread = unread_room();
    let rooms = vec![(1, room(0.995, 0.0)), (2, unread.clone()), (3, Room::Unknown), (4, room(0.9, 0.0))];
    assert_eq!(roomiest(&rooms, &Placed::default(), None), Some(4));
    assert_eq!(roomiest(&rooms[..3], &Placed::default(), None), Some(2));
    assert_eq!(roomiest(&[(3, Room::Unknown)], &Placed::default(), None), Some(3), "no feed is not no account");
}

#[test]
fn helpers_refused_one_after_another_are_moved_to_different_accounts() {
    let rooms = vec![(1, room(1.0, 0.0)), (2, room(0.2, 0.0)), (3, room(0.3, 0.0)), (4, room(0.9, 0.0))];
    let mut placed = Placed::default();
    let first = for_helper(Move::To(4), &rooms, 1, None, &mut placed);
    let second = for_helper(Move::To(4), &rooms, 1, None, &mut placed);
    assert_eq!((first, second), (Move::To(2), Move::To(3)), "best_profile would send both to P4");
    assert_eq!(for_helper(Move::Hold("cooling down"), &rooms, 1, None, &mut placed), Move::Hold("cooling down"));
    let all_out = vec![(1, room(1.0, 0.0)), (2, room(1.0, 0.0))];
    assert_eq!(for_helper(Move::To(2), &all_out, 1, None, &mut placed), Move::Hold("every account is out"));
}

#[test]
fn quota_a_batch_on_accounts_the_feed_does_not_cover_is_spread() {
    let rooms = vec![(1, Room::Unknown), (2, Room::Unknown), (3, Room::Unknown)];
    let mut placed = Placed::default();
    let got: Vec<u8> = (0..3)
        .map(|_| {
            let to = roomiest(&rooms, &placed, None).unwrap();
            placed.note(to);
            to
        })
        .collect();
    assert_eq!(got, [1, 2, 3]);
}

#[test]
fn helpers_refused_are_never_moved_to_an_account_the_feed_does_not_cover() {
    let rooms = vec![(1, room(1.0, 0.0)), (2, Room::Unknown), (3, room(0.9, 0.0))];
    let mut placed = Placed::default();
    assert_eq!(for_helper(Move::To(3), &rooms, 1, None, &mut placed), Move::To(3));
    let only_unknown = vec![(1, room(1.0, 0.0)), (2, Room::Unknown)];
    assert_eq!(for_helper(Move::To(2), &only_unknown, 1, None, &mut placed), Move::Hold("every account is out"));
    assert_eq!(roomiest(&only_unknown, &Placed::default(), None), Some(2), "a start may still use it");
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

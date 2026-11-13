//! The five-hour window counts for an account exactly while the feed
//! gives a reading for it, whatever its plan.

use marspot::cc_usage::CcAccount;

use super::super::heads_up::nearly_out;
use super::*;

/// 2026-11-05 05:33 UTC.
const NOW: i64 = 1_793_856_780;

fn situation(rooms: &[(u8, Room)]) -> Situation<'_> {
    Situation {
        needs_model: None,
        rooms,
        current: 1,
        tool_executing: false,
        work_running: false,
        moves_this_episode: 0,
        since_last_move: None,
        last_target: None,
        stranded_for: None,
        warned: false,
        background_refused: None,
    }
}

/// A row of the 20x tier, with the 5h reading the feed gave or none.
fn ultra(email: &str, status: &str, five: Option<(f64, Option<i64>)>, u7: f64) -> CcAccount {
    CcAccount {
        tier: "default_claude_max_20x".into(),
        util_5h: five.map(|(u, _)| u),
        reset_5h: five.and_then(|(_, reset)| reset),
        name: email.into(),
        email: email.into(),
        status: status.into(),
        util_7d: u7,
        reset_7d: NOW + 86_400,
        model_limits: Vec::new(),
        collected_at: NOW - 60,
        limits_as_of: None,
    }
}

fn one_room(a: &CcAccount, now: i64) -> Room {
    rooms_for(&[1], std::slice::from_ref(a), |_| Some(a.email.clone()), now).remove(0).1
}

/// The reading decides, not the tier: a 20x account read at 100% of
/// its 5h is left, and is somewhere to go again once that window's
/// reset has passed.
#[test]
fn an_ultra_account_with_a_full_5h_reading_is_out_until_its_reset() {
    let reset = NOW + 3_600;
    let held = ultra("p1", "rejected", Some((1.0, Some(reset))), 0.40);
    let free = ultra("p2", "allowed", None, 0.40);
    let accounts = vec![held.clone(), free];
    let rooms = rooms_for(&[1, 2], &accounts, |p| Some(format!("p{p}")), NOW);
    assert!(rooms[0].1.refused_for(None), "full 5h");
    let s = Situation { current: 1, ..situation(&rooms) };
    assert_eq!(next_move(&s), Move::To(2));

    assert!(one_room(&held, reset - 1).refused_for(None), "the window is still running");
    let after = one_room(&held, reset);
    assert!(!after.refused_for(None), "the same row, read after its reset");
    assert!(matches!(&after, Room::Known { status, util_5h: Some(u), .. } if status == "allowed" && *u == 0.0));

    // the status word alone, as the feed's owner writes it at the limit
    let said = ultra("p1", "rejected", Some((0.97, Some(reset))), 0.40);
    assert!(one_room(&said, NOW).refused_for(None));
    // and the margin the 5h window has always had
    let nearly = ultra("p1", "allowed_warning", Some((0.99, Some(reset))), 0.40);
    assert!(one_room(&nearly, NOW).refused_for(None));
}

/// An account with no 5h reading has no 5h to be out on, to rank
/// by, or to be warned about.
#[test]
fn an_account_with_no_5h_reading_is_untouched_by_the_5h() {
        let free = ultra("p1", "allowed", None, 0.40);
    let room = one_room(&free, NOW);
    assert!(matches!(&room, Room::Known { util_5h: None, .. }), "none, not zero: {room:?}");
    assert!(!room.refused_for(None) && !nearly_out(&room, None));
    assert_eq!(room.headroom_for(None), (0.60, 1.0));

    // a reset with no reading beside it is not a window either
    let odd = CcAccount { reset_5h: Some(NOW - 60), ..free.clone() };
    assert_eq!(one_room(&odd, NOW), room);

    let week_spent = ultra("p3", "rejected", None, 1.0);
    assert!(one_room(&week_spent, NOW).refused_for(None), "its week still counts");
}

/// Zero is a reading: the account is held to a window it has not
/// started, usable now, and counted like any other 5h.
#[test]
fn a_5h_reading_of_zero_is_a_limit_with_nothing_used() {
        let fresh = ultra("p1", "allowed", Some((0.0, None)), 0.40);
    let room = one_room(&fresh, NOW);
    assert!(matches!(&room, Room::Known { util_5h: Some(u), .. } if *u == 0.0), "{room:?}");
    assert!(!room.refused_for(None) && !nearly_out(&room, None));
    assert_ne!(room, one_room(&ultra("p1", "allowed", None, 0.40), NOW), "kept apart from no reading");

    // one step short of spent, the session is told
    let last = ultra("p1", "allowed", Some((0.97, Some(NOW + 600))), 0.40);
    assert!(nearly_out(&one_room(&last, NOW), None));
    assert!(!nearly_out(&one_room(&last, NOW + 600), None), "that window has ended");
}

/// From the feed's text to the decision: `null`, `0` and `1` are
/// three different rows.
#[test]
fn null_zero_and_full_5h_stay_apart_from_the_feed_to_the_move() {
    let row = |n: u8, status: &str, five: &str, reset: &str| {
        format!(
            r#"{{"name":"Claude {n}","email":"p{n}","status":"{status}","utilization_5h":{five},
                "utilization_7d":0.4,"reset_5h":{reset},"reset_7d":4102444800,"model_limits":[],
                "tier":"default_claude_max_20x","collected_at":"2026-11-05T05:33:00+00:00"}}"#
        )
    };
    let feed = format!(
        r#"{{"generated_at":"2026-11-05T05:33:00+00:00","accounts":[{},{},{}]}}"#,
        row(1, "rejected", "1", "4102444800"),
        row(2, "allowed", "null", "null"),
        row(3, "allowed", "0", "null"),
    );
    let usage = marspot::cc_usage::parse(&feed).expect("the feed parses");
    let fives: Vec<Option<f64>> = usage.accounts.iter().map(|a| a.util_5h).collect();
    assert_eq!(fives, [Some(1.0), None, Some(0.0)]);
    let rooms = rooms_for(&[1, 2, 3], &usage.accounts, |p| Some(format!("p{p}")), NOW);
    let read: Vec<Option<f64>> = rooms
        .iter()
        .map(|(_, r)| match r {
            Room::Known { util_5h, .. } => *util_5h,
            Room::Unknown => unreachable!(),
        })
        .collect();
    assert_eq!(read, [Some(1.0), None, Some(0.0)]);
    let out: Vec<bool> = rooms.iter().map(|(_, r)| r.refused_for(None)).collect();
    assert_eq!(out, [true, false, false]);
    let s = Situation { current: 1, ..situation(&rooms) };
    assert!(matches!(next_move(&s), Move::To(2 | 3)), "the pane on the full one is moved");
    let s = Situation { current: 2, ..situation(&rooms) };
    assert!(!matches!(next_move(&s), Move::To(1)));
}

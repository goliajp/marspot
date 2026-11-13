//! A week the feed gives no reading for is not a week with nothing
//! used, and not a full one.

use super::super::heads_up::nearly_out;
use super::super::rebalance::{better_home, Why};
use super::*;

/// 2026-11-05 05:33 UTC.
const NOW: i64 = 1_793_856_780;
const DAY: i64 = 86_400;

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

/// Rows as the feed writes them: `(status, utilization_7d, reset_7d,
/// Fable's utilization)`, account `n` under the address `p<n>`.
fn rooms(rows: &[(&str, &str, &str, &str)]) -> Vec<(u8, Room)> {
    let rows: Vec<String> = rows
        .iter()
        .enumerate()
        .map(|(i, (status, week, reset, fable))| {
            let n = i + 1;
            format!(
                r#"{{"name":"Claude {n}","email":"p{n}","status":"{status}","utilization_5h":null,
                    "utilization_7d":{week},"reset_5h":null,"reset_7d":{reset},
                    "model_limits":[{{"label":"Fable","utilization":{fable},"reset":null}}],
                    "tier":"default_claude_max_20x","collected_at":"2026-11-05T05:33:00+00:00"}}"#
            )
        })
        .collect();
    let feed = format!(r#"{{"generated_at":"2026-11-05T05:33:00+00:00","accounts":[{}]}}"#, rows.join(","));
    let usage = marspot::cc_usage::parse(&feed).expect("the feed parses");
    let profiles: Vec<u8> = (1..=rows.len() as u8).collect();
    rooms_for(&profiles, &usage.accounts, |p| Some(format!("p{p}")), NOW)
}

fn soon() -> String {
    (NOW + DAY).to_string()
}

fn late() -> String {
    (NOW + 5 * DAY).to_string()
}

/// From the feed's text to the room: `null` stays apart from `0`.
#[test]
fn a_null_week_is_no_reading_in_the_room_and_zero_is_one() {
    let r = rooms(&[("allowed", "null", "null", "0"), ("allowed", "0", "null", "0")]);
    assert!(matches!(&r[0].1, Room::Known { util_7d: None, .. }), "{:?}", r[0].1);
    assert!(matches!(&r[1].1, Room::Known { util_7d: Some(u), .. } if *u == 0.0), "{:?}", r[1].1);
    assert_ne!(r[0].1, r[1].1);
    assert!(!r[0].1.week_known() && r[1].1.week_known());
    assert_eq!(r[0].1.headroom_for(None), (None, 1.0), "no figure, not all of it");
    assert_eq!(r[1].1.headroom_for(None), (Some(1.0), 1.0));
}

/// Not full: a pane on it stays, and is not told it is about to run
/// out.  Its own refusal still moves it.
#[test]
fn an_account_whose_week_is_not_read_is_neither_out_nor_nearly_out() {
    let r = rooms(&[("allowed", "null", "null", "0"), ("allowed", "0.2", &late(), "0")]);
    let here = &r[0].1;
    assert!(!here.refused_for(None) && !here.refused_for(Some("fable-5-1")));
    assert!(!nearly_out(here, None));
    let refused = Refusal { at: NOW + 60, model: String::new(), logged_out: false, window: Window::Week };
    assert!(here.refused_for_now(None, Some(&refused)), "what the pane was told is newer than the row");
    // the model's cap is a reading of its own and still counts
    let capped = rooms(&[("allowed", "null", "null", "0.99")]);
    assert!(nearly_out(&capped[0].1, Some("fable-5-1")));
    assert!(!nearly_out(&capped[0].1, Some("opus-5-5")));
    let spent = rooms(&[("rejected", "null", "null", "1")]);
    assert!(spent[0].1.refused_for(Some("fable-5-1")) && !spent[0].1.refused_for(Some("opus-5-5")));
}

/// Not empty: a refused pane goes to every account known to have room
/// first, whatever its renewal, and to the unread one before staying
/// on an account that is out.
#[test]
fn a_refused_pane_goes_to_an_unread_week_only_after_every_known_one() {
    let r = rooms(&[
        ("rejected", "1", &soon(), "0"),
        ("allowed", "null", "null", "0"),
        ("allowed", "0.3", "null", "0"),
    ]);
    assert_eq!(best_profile(&r, 1, None), Some(3), "a week read beats one not read");
    assert_eq!(next_move(&situation(&r)), Move::To(3));

    let only = rooms(&[("rejected", "1", &soon(), "0"), ("allowed", "null", "null", "0"), ("rejected", "1", &late(), "0")]);
    assert_eq!(next_move(&situation(&only)), Move::To(2), "ahead of the refused");

    // among accounts known to have room, an Opus pane prefers the one
    // whose Fable is spent; an unread week does not get that place
    let fable = rooms(&[
        ("rejected", "1", &soon(), "0"),
        ("rejected", "null", "null", "1"),
        ("allowed", "0.5", &late(), "0"),
    ]);
    assert_eq!(best_profile(&fable, 1, Some("opus-5-5")), Some(3));
}

/// A resting pane is moved for a difference worth a restart, and a
/// week that is not read is no figure to measure one by.
#[test]
fn no_resting_pane_is_moved_to_or_from_an_unread_week_by_its_renewal() {
    let to_unread = rooms(&[("allowed", "0.1", &late(), "0"), ("allowed", "null", "null", "0")]);
    assert_eq!(better_home(&to_unread, 1, None), None);
    let to_unread_fable = rooms(&[("allowed", "0.1", &late(), "0"), ("rejected", "null", "null", "1")]);
    assert_eq!(better_home(&to_unread_fable, 1, Some("opus-5-5")), None, "its week may have nothing left");

    // the unread week is ranked a whole week ahead; that is a place in
    // a list, not a renewal time to leave it for
    let from_unread = rooms(&[("allowed", "null", "null", "0"), ("allowed", "0.5", &soon(), "0")]);
    assert_eq!(better_home(&from_unread, 1, None), None);
    // a week read as not started has that renewal, and is left
    let from_fresh = rooms(&[("allowed", "0", "null", "0"), ("allowed", "0.5", &soon(), "0")]);
    assert_eq!(better_home(&from_fresh, 1, None), Some((2, Why::RenewsSooner { hours: 144 })));
    // leaving Fable to the panes that need it rests on readings alone
    let keeps = rooms(&[("allowed", "null", "null", "0"), ("rejected", "0.5", &late(), "1")]);
    assert_eq!(better_home(&keeps, 1, Some("opus-5-5")), Some((2, Why::KeepsFable)));
}

/// A reset that has passed says the week it was given for is over,
/// with or without a figure beside it.
#[test]
fn a_week_whose_reset_has_passed_is_a_new_one_with_nothing_used() {
    let passed = (NOW - 3_600).to_string();
    let r = rooms(&[("allowed", "null", &passed, "0"), ("rejected", "1", &passed, "0")]);
    for (_, room) in &r {
        assert!(matches!(room, Room::Known { util_7d: Some(u), .. } if *u == 0.0), "{room:?}");
        assert!(!room.refused_for(None));
    }
}

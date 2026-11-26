//! A week the feed gives no reading for is not a week with nothing
//! used, and not a full one.

use super::super::heads_up::nearly_out;
use super::super::allocation_inputs::{AccountIn, PaneIn, Snapshot};
use super::super::allocation_view::{self as view, View};
use super::*;
use crate::plugins::allocation::Class;

/// 2026-11-05 05:33 UTC.
const NOW: i64 = 1_793_856_780;
const DAY: i64 = 86_400;

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

/// Not empty: a refused Claude pane goes to every account known to have
/// room first, whatever its renewal, and to the unread one before
/// staying on an account that is out; a badge click the same.
#[test]
fn a_refused_pane_goes_to_an_unread_week_only_after_every_known_one() {
    let (v, r) = view_of(&[("rejected", "1", &soon(), "0"), ("allowed", "null", "null", "0"), ("allowed", "0.3", &late(), "0")], Class::Plain);
    let (to, inputs) = v.must_move(&r, 1, 1, Class::Plain);
    assert_eq!(to, Move::To(3), "a week read beats one not read");
    assert!(inputs.is_some(), "the core chose it");

    let only = [("rejected", "1", &soon() as &str, "0"), ("allowed", "null", "null", "0"), ("rejected", "1", &late(), "0")];
    let (v, r) = view_of(&only, Class::Plain);
    let (to, inputs) = v.must_move(&r, 1, 1, Class::Plain);
    assert_eq!(to, Move::To(2), "ahead of the refused");
    assert!(inputs.is_none(), "no reading chose it");
    assert_eq!(v.by_hand(&r, 1, 1, Class::Plain).map(|(t, i)| (t, i.is_none())), Some((2, true)));

    // an unread week that has refused is no place either
    let shut = [("rejected", "1", &soon() as &str, "0"), ("rejected", "null", "null", "0")];
    let (v, r) = view_of(&shut, Class::Plain);
    assert_eq!(v.must_move(&r, 1, 1, Class::Plain).0, Move::Hold(view::NOWHERE));
}

/// The allocation's view of the same rows, with pane 1 of `class` on P1.
fn view_of(rows: &[(&str, &str, &str, &str)], class: Class) -> (View, Vec<(u8, Room)>) {
    let r = rooms(rows);
    let accounts = r
        .iter()
        .filter_map(|(p, room)| {
            let Room::Known { util_7d: Some(w), reset_7d, models, measured_at, .. } = room else { return None };
            let fable_used = models.first().and_then(|(_, u)| *u);
            Some(AccountIn {
                profile: *p,
                week_used: *w,
                fable_used,
                fable_known: room.known_for(Some("fable")),
                week_renews: *reset_7d,
                fable_renews: *reset_7d,
                open: !room.refused_for(None),
                measured_at: (*measured_at).min(NOW),
                supply: Vec::new(),
            })
        })
        .collect();
    let pane = PaneIn { sid: 1, profile: 1, class, marked_at: None, refused_at: None, resting: true, helper: false, out: false, transcript: None };
    let snap = Snapshot { now: NOW, feed_at: Some(NOW), accounts, panes: vec![pane], live: vec![1] };
    (View::of_defaults(&snap), r)
}

/// A week that is not read is no account to the allocation: no resting
/// pane is sent there and nothing is measured by it.  A refused pane
/// with nowhere read to go goes there.
#[test]
fn an_unread_week_is_no_account_to_the_allocation() {
    let ids = |v: &View| v.accounts.iter().map(|q| q.id).collect::<Vec<_>>();
    let (to_unread, r) = view_of(&[("allowed", "0.1", &late(), "0"), ("allowed", "null", "null", "0")], Class::Plain);
    assert_eq!(ids(&to_unread), [1]);
    assert_eq!(to_unread.must_move(&r, 1, 1, Class::Plain).0, Move::To(2), "the last place for a refused pane");
    let (from_unread, _) = view_of(&[("allowed", "null", "null", "0"), ("allowed", "0.5", &soon(), "0")], Class::Plain);
    assert_eq!(ids(&from_unread), [2], "a pane on an unread week has no resting move");
    // a week read as not started is a reading
    let (from_fresh, _) = view_of(&[("allowed", "0", "null", "0"), ("allowed", "0.5", &soon(), "0")], Class::Plain);
    assert_eq!(ids(&from_fresh), [1, 2]);
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

/// A model's cap the feed reports with no reading is no more an empty
/// cap than an unread week is an empty week: not full, and not known
/// to have room for a pane on that model. Other panes are not bounded
/// by it.
#[test]
fn a_cap_with_no_reading_is_neither_full_nor_room_for_its_model() {
    let fable = Some("fable-5-2");
    let opus = Some("opus-5-5");
    let r = rooms(&[("rejected", "1", &soon(), "0"), ("allowed", "0.2", &soon(), "null"), ("allowed", "0.5", &late(), "0.5")]);
    assert!(matches!(&r[1].1, Room::Known { models, .. } if models[0].1.is_none()), "{:?}", r[1].1);
    assert!(!r[1].1.refused_for(fable));
    assert!(!r[1].1.known_for(fable) && r[1].1.known_for(opus));
    assert_eq!(best_profile(&r, 1, fable), Some(3), "a cap read beats one not read");
    assert_eq!(best_profile(&r, 1, opus), Some(2));

    // a Fable pane goes where the cap is not read before staying
    let only = [("rejected", "1", &soon() as &str, "0"), ("allowed", "0.2", &soon(), "null")];
    let (v, r) = view_of(&only, Class::Restricted);
    assert_eq!(v.must_move(&r, 1, 1, Class::Restricted).0, Move::To(2), "ahead of staying");

    // the allocation: a Fable pane goes only where the cap is read
    let rows = [("rejected", "1", &soon() as &str, "0"), ("allowed", "0.2", &soon(), "null"), ("allowed", "0.5", &late(), "0.5")];
    let (v, r) = view_of(&rows, Class::Restricted);
    assert_eq!(v.must_move(&r, 1, 1, Class::Restricted).0, Move::To(3), "no move for a guess");
    let (v, r) = view_of(&rows, Class::Plain);
    assert_eq!(v.must_move(&r, 1, 1, Class::Plain).0, Move::To(2));
}

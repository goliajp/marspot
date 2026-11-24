use super::*;
use marspot::cc_usage::CcModelLimit;

const NOW: i64 = 1_795_530_644;

fn row(u7: f64, fable: f64, at: i64, reset: i64) -> CcAccount {
    CcAccount {
        name: "Claude 3".into(),
        email: "p3".into(),
        status: "allowed".into(),
        util_5h: None,
        util_7d: Some(u7),
        reset_5h: None,
        reset_7d: Some(reset),
        model_limits: vec![CcModelLimit { label: "Fable".into(), util: Some(fable), reset: Some(reset) }],
        collected_at: at,
        limits_as_of: None,
        tier: "default_claude_max_20x".into(),
    }
}

/// Readings every five minutes from `from` to NOW, the week rising at
/// `week` and Fable at `fable` an hour.
fn trends(u7_now: f64, week: f64, f_now: f64, fable: f64, reset: i64) -> Trends {
    let mut t = Trends::default();
    for k in (0..=12).rev() {
        let back = k as f64 / 12.0;
        t.note(&[row(u7_now - week * back, f_now - fable * back, NOW - k * 300, reset)]);
    }
    t
}

fn outlook<'a>(t: &'a Trends, a: &'a CcAccount, room: &'a Room) -> Outlook<'a> {
    Outlook { trends: t, account: a, room, now: NOW }
}

#[test]
fn rebalance_an_account_lasting_an_hour_and_a_half_is_short() {
    let reset = NOW + 3 * 86_400;
    // six points an hour, nine left
    let t = trends(0.91, 0.06, 0.10, 0.0, reset);
    let a = row(0.91, 0.10, NOW, reset);
    let room = super::super::rooms::room_of(&a, NOW);
    let o = outlook(&t, &a, &room);
    let h = o.hours_left(Some("opus-5-5"), 0.0, None).unwrap();
    assert!((h - 1.5).abs() < 0.01, "{h}");
    assert!(o.short(Some("opus-5-5")));

    // twenty left at the same speed: over three hours
    let t = trends(0.80, 0.06, 0.10, 0.0, reset);
    let a = row(0.80, 0.10, NOW, reset);
    let room = super::super::rooms::room_of(&a, NOW);
    assert!(!outlook(&t, &a, &room).short(Some("opus-5-5")));

    // renewing within the hour: it never runs out
    let soon = NOW + 3000;
    let t = trends(0.91, 0.06, 0.10, 0.0, soon);
    let a = row(0.91, 0.10, NOW, soon);
    let room = super::super::rooms::room_of(&a, NOW);
    assert!(!outlook(&t, &a, &room).short(None));
}

#[test]
fn rebalance_fable_running_short_is_short_for_a_fable_pane_only() {
    let reset = NOW + 3 * 86_400;
    // the week has half left, Fable 8 points at 8 an hour
    let t = trends(0.50, 0.04, 0.92, 0.08, reset);
    let a = row(0.50, 0.92, NOW, reset);
    let room = super::super::rooms::room_of(&a, NOW);
    let o = outlook(&t, &a, &room);
    assert!(o.short(Some("fable-5-1")));
    assert!(!o.short(Some("opus-5-5")));
}

#[test]
fn rebalance_speed_restarts_after_a_reading_drop() {
    let mut s = Series::default();
    for k in 0..=6 {
        s.add(NOW + k * 300, 0.40 + 0.01 * k as f64);
    }
    let before = s.per_hour().unwrap();
    assert!((before - 0.12).abs() < 1e-9, "{before}");
    // the reading falls by half: what went before says nothing now
    assert!(s.add(NOW + 2100, 0.20));
    assert_eq!(s.per_hour(), None, "no span yet");
    for k in 1..=6 {
        s.add(NOW + 2100 + k * 300, 0.20 + 0.005 * k as f64);
    }
    let after = s.per_hour().unwrap();
    assert!((after - 0.06).abs() < 1e-9, "measured from the drop: {after}");
    assert!(!s.add(NOW, 0.9), "an older reading is not taken");
}

#[test]
fn rebalance_an_account_one_step_from_out_is_short_before_any_speed_is_known() {
    let reset = NOW + 3 * 86_400;
    let a = row(0.99, 0.10, NOW, reset);
    let room = super::super::rooms::room_of(&a, NOW);
    let t = Trends::default();
    assert!(outlook(&t, &a, &room).short(None));
    let a = row(0.90, 0.10, NOW, reset);
    let room = super::super::rooms::room_of(&a, NOW);
    assert!(!outlook(&t, &a, &room).short(None), "no speed, room left: not short");
}

#[test]
fn rebalance_trends_survive_a_restart_and_follow_the_feed() {
    let reset = NOW + 3 * 86_400;
    let mut t = trends(0.91, 0.06, 0.10, 0.0, reset);
    let text = t.changed().expect("readings came in");
    assert_eq!(t.changed(), None, "and not again");
    let back = Trends::decode(&text);
    assert_eq!(back.per_hour("p3", "7d"), t.per_hour("p3", "7d"));
    let mut t = back;
    t.note(&[]);
    assert_eq!(t.per_hour("p3", "7d"), None, "an account gone from the feed is forgotten");
}

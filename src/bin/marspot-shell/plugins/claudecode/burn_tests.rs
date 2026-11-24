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

/// Readings every five minutes over the hour up to NOW, the week rising
/// at `week` and Fable at `fable` an hour.
fn trends(u7_now: f64, week: f64, f_now: f64, fable: f64, reset: i64) -> Trends {
    let mut t = Trends::default();
    for k in (0..=12).rev() {
        let back = k as f64 / 12.0;
        t.note(&[row(u7_now - week * back, f_now - fable * back, NOW - k * 300, reset)], NOW - k * 300);
    }
    t
}

fn outlook<'a>(s: &'a Speeds, a: &'a CcAccount, room: &'a Room) -> Outlook<'a> {
    Outlook { speeds: s, account: a, room, now: NOW }
}

#[test]
fn rebalance_an_account_lasting_an_hour_and_a_half_is_short() {
    let reset = NOW + 3 * 86_400;
    // six points an hour, nine left
    let s = trends(0.91, 0.06, 0.10, 0.0, reset).speeds(NOW);
    let a = row(0.91, 0.10, NOW, reset);
    let room = super::super::rooms::room_of(&a, NOW);
    let o = outlook(&s, &a, &room);
    let h = o.hours_left(Some("opus-5-5"), Load::default(), None).unwrap();
    assert!((h - 1.5).abs() < 0.01, "{h}");
    assert!(o.short(Some("opus-5-5")));

    // twenty left at the same speed: over three hours
    let s = trends(0.80, 0.06, 0.10, 0.0, reset).speeds(NOW);
    let a = row(0.80, 0.10, NOW, reset);
    let room = super::super::rooms::room_of(&a, NOW);
    assert!(!outlook(&s, &a, &room).short(Some("opus-5-5")));

    // renewing within the hour: it never runs out
    let soon = NOW + 3000;
    let s = trends(0.91, 0.06, 0.10, 0.0, soon).speeds(NOW);
    let a = row(0.91, 0.10, NOW, soon);
    let room = super::super::rooms::room_of(&a, NOW);
    assert!(!outlook(&s, &a, &room).short(None));
}

#[test]
fn rebalance_fable_running_short_is_short_for_a_fable_pane_only() {
    let reset = NOW + 3 * 86_400;
    // the week has half left, Fable 8 points at 8 an hour
    let s = trends(0.50, 0.04, 0.92, 0.08, reset).speeds(NOW);
    let a = row(0.50, 0.92, NOW, reset);
    let room = super::super::rooms::room_of(&a, NOW);
    let o = outlook(&s, &a, &room);
    assert!(o.short(Some("fable-5-1")));
    assert!(!o.short(Some("opus-5-5")));
}

/// The week is slow, the five hours fill in under two: short.
#[test]
fn rebalance_a_five_hour_window_filling_in_under_two_hours_is_short() {
    let reset = NOW + 3 * 86_400;
    let five = |u5: f64, at: i64| CcAccount { util_5h: Some(u5), reset_5h: Some(NOW + 4 * 3600), ..row(0.30, 0.10, at, reset) };
    let mut t = Trends::default();
    for k in (0..=12i64).rev() {
        t.note(&[five(0.60 - 0.20 * k as f64 / 12.0, NOW - k * 300)], NOW - k * 300);
    }
    let s = t.speeds(NOW);
    let a = five(0.60, NOW);
    let room = super::super::rooms::room_of(&a, NOW);
    let h = outlook(&s, &a, &room).hours_left(Some("opus-5-5"), Load::default(), None).unwrap();
    assert!((h - 1.9).abs() < 0.01, "0.38 left at 0.2 an hour: {h}");
    assert!(outlook(&s, &a, &room).short(Some("opus-5-5")));
    // renewing before it fills, it never runs out
    let early = CcAccount { reset_5h: Some(NOW + 3600), ..a.clone() };
    let room = super::super::rooms::room_of(&early, NOW);
    assert!(!outlook(&s, &early, &room).short(Some("opus-5-5")));
}

#[test]
fn rebalance_speed_restarts_after_a_reading_drop() {
    let mut s = Series::default();
    for k in 0..=6 {
        s.add(NOW + k * 300, 0.40 + 0.01 * k as f64);
    }
    let before = s.per_hour(NOW + 1800).unwrap();
    assert!((before - 0.12).abs() < 1e-9, "{before}");
    // the reading falls by half: what went before says nothing now
    assert!(s.add(NOW + 2100, 0.20));
    assert_eq!(s.per_hour(NOW + 2100), None, "no span yet");
    for k in 1..=6 {
        s.add(NOW + 2100 + k * 300, 0.20 + 0.005 * k as f64);
    }
    let after = s.per_hour(NOW + 3900).unwrap();
    assert!((after - 0.06).abs() < 1e-9, "measured from the drop: {after}");
    assert!(!s.add(NOW, 0.9), "an older reading is not taken");
}

/// A feed that stopped, or one read again after a restart, says
/// nothing about the speed once its newest reading is ninety minutes
/// old by the clock.
#[test]
fn rebalance_a_stale_feed_has_no_speed() {
    let reset = NOW + 3 * 86_400;
    let mut t = trends(0.91, 0.06, 0.10, 0.0, reset);
    assert!(t.speeds(NOW).per_hour("p3", "7d").is_some());
    assert!(t.speeds(NOW + LOOKBACK_S).per_hour("p3", "7d").is_some(), "the newest is ninety minutes old");
    assert_eq!(t.speeds(NOW + LOOKBACK_S + 1).per_hour("p3", "7d"), None);
    // the same reading again changes nothing, and it ages out
    t.changed();
    t.note(&[row(0.91, 0.10, NOW, reset)], NOW + LOOKBACK_S + 1);
    assert_eq!(t.speeds(NOW + LOOKBACK_S + 1).per_hour("p3", "7d"), None);
    assert!(t.changed().is_some_and(|text| text.is_empty()), "aged out and written as such");
}

/// The feed leaves an account out for a while: its readings stay, and
/// the speed is there when it is back.
#[test]
fn rebalance_an_account_missing_from_one_reading_keeps_its_speed() {
    let reset = NOW + 3 * 86_400;
    let mut t = trends(0.91, 0.06, 0.10, 0.0, reset);
    t.note(&[], NOW + 60);
    assert!(t.speeds(NOW + 60).per_hour("p3", "7d").is_some_and(|s| (s - 0.06).abs() < 1e-9));
}

#[test]
fn rebalance_an_account_one_step_from_out_is_short_before_any_speed_is_known() {
    let reset = NOW + 3 * 86_400;
    let a = row(0.99, 0.10, NOW, reset);
    let room = super::super::rooms::room_of(&a, NOW);
    let s = Speeds::default();
    assert!(outlook(&s, &a, &room).short(None));
    let a = row(0.90, 0.10, NOW, reset);
    let room = super::super::rooms::room_of(&a, NOW);
    assert!(!outlook(&s, &a, &room).short(None), "no speed, room left: not short");
}

#[test]
fn rebalance_trends_survive_a_restart() {
    let reset = NOW + 3 * 86_400;
    let mut t = trends(0.91, 0.06, 0.10, 0.0, reset);
    let text = t.changed().expect("readings came in");
    assert_eq!(t.changed(), None, "and not again");
    let back = Trends::decode(&text);
    assert_eq!(back.speeds(NOW).per_hour("p3", "7d"), t.speeds(NOW).per_hour("p3", "7d"));
}

/// The scan thread's cache writes the file and reads it back.
#[test]
fn rebalance_the_trend_file_is_written_and_read_back() {
    let dir = std::env::temp_dir().join(format!("marspot-trend-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(TRENDS_FILE);
    let reset = NOW + 3 * 86_400;
    let mut cache = TrendCache::at(file.clone());
    let mut log = Vec::new();
    for k in (0..=12i64).rev() {
        let at = NOW - k * 300;
        let usage = CcUsage { generated_at: at, accounts: vec![row(0.91 - 0.06 * k as f64 / 12.0, 0.1, at, reset)] };
        cache.take(Some(&usage), at, &mut log);
    }
    assert!(log.is_empty(), "{log:?}");
    let mut again = TrendCache::at(file);
    again.load();
    let speed = again.take(None, NOW, &mut log).per_hour("p3", "7d").unwrap();
    assert!((speed - 0.06).abs() < 1e-9, "{speed}");
    std::fs::remove_dir_all(&dir).unwrap();
}

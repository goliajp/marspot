use super::*;
use crate::quota::Class;

const T0: i64 = 1_795_478_400; // 2026-11-25T00:00Z

fn hours(f: &mut Facts, from: i64, n: i64, at: impl Fn(i64) -> [f64; 2]) {
    f.set_hours((0..n).map(|k| (from + k * HOUR, Some(at(k)))));
}

#[test]
fn forecast_pane_levels_and_age() {
    let mut f = Facts::default();
    // pane 1: a point an hour of other models for 30 hours, fable in the last 3
    for k in 0..30 {
        f.used(T0 + k * HOUR + 60, 1, Class::Plain, 0.01);
        if k >= 27 {
            f.used(T0 + k * HOUR + 120, 1, Class::Restricted, 0.02);
        }
    }
    // pane 2: born ten minutes before now
    let now = T0 + 30 * HOUR;
    f.used(now - 600, 2, Class::Plain, 0.01);
    let fc = build(&f, now, &[2, 1, 9]);
    assert_eq!(fc.panes.len(), 2, "pane 9 has no facts");
    let p1 = fc.panes[0];
    assert!(p1.counts);
    assert!((p1.day[1] - 0.01).abs() < 1e-12 && (p1.short[1] - 0.01).abs() < 1e-12);
    assert!((p1.short[0] - 0.02).abs() < 1e-12 && (p1.day[0] - 0.0025).abs() < 1e-12, "{p1:?}");
    assert!(!fc.panes[1].counts, "younger than half an hour");
    // at an hour boundary the forecast blends 3 h of the short mean and 3 h of the day's
    let p = &f.panes[&1];
    assert!((level::pane_forecast(p, 0, now) - (0.02 + 0.0025) / 2.0).abs() < 1e-12);
}

/// A pane that holds its speed gives ratios of 1, and a band from its
/// default that is not widened.
#[test]
fn forecast_steady_pane_has_ratio_one() {
    let mut f = Facts::default();
    for k in 0..48 {
        f.used(T0 + k * HOUR + 30, 1, Class::Plain, 0.005);
    }
    let now = T0 + 48 * HOUR;
    let r = level::pane_ratios(&f, now);
    assert!(r[0].is_empty());
    assert_eq!(r[1].len(), 41, "origins from half an hour in, the last one ends at the last fact");
    assert!(r[1].iter().all(|x| (x - 1.0).abs() < 1e-9));
    let fc = build(&f, now, &[1]);
    assert!(!fc.pane_band[1].widened && fc.pane_band[1].coverage() == Some(1.0));
}

#[test]
fn forecast_fleet_level_a_hi_and_unknown_hours() {
    let mut f = Facts::default();
    hours(&mut f, T0, 48, |k| [0.0, if (36..42).contains(&k) { 0.06 } else { 0.03 }]);
    let now = T0 + 48 * HOUR + 10;
    assert!((level::fleet_level(&f, 1, now).unwrap() - (18.0 * 0.03 + 6.0 * 0.06) / 24.0).abs() < 1e-12);
    assert_eq!(level::a_hi(&f, 1, now), Some(0.06));
    // an unknown hour breaks every window that holds it, and is no zero
    let mut rows: Vec<HourRow> = f.hours.iter().map(|(k, v)| (*k, *v)).collect();
    rows[30].1 = None;
    f.set_hours(rows);
    assert!(level::fleet_origins(&f).iter().all(|t| !(*t - DAY_H * HOUR..*t + HORIZON_H * HOUR).contains(&(T0 + 30 * HOUR))));
    assert!((level::fleet_level(&f, 1, now).unwrap() - (17.0 * 0.03 + 6.0 * 0.06) / 23.0).abs() < 1e-12);
}

/// A fleet with a strong daily shape: the profile beats the 24-hour
/// mean by more than 10% and the test turns on; a flat fleet's does not.
#[test]
fn forecast_time_of_day_test() {
    let mut f = Facts::default();
    hours(&mut f, T0, 24 * 5, |k| [0.0, if k % 24 < 12 { 0.01 } else { 0.05 }]);
    let t = level::time_of_day(&f, 1);
    assert!(t.on && t.mae_profile < 1e-12 && t.mae_day > 0.005, "{t:?}");
    // a flat fleet, at a value whose sums are exact: a tie does not turn it on
    hours(&mut f, T0, 24 * 5, |_| [0.0, 0.25]);
    let t = level::time_of_day(&f, 1);
    assert!(!t.on, "{t:?}");
    assert_eq!(t.origins, 24 * 5 - 24 - 6 + 1);
}

#[test]
fn forecast_clean_rests_background_and_helpers() {
    let mut f = Facts::default();
    let b = |at, clean, tasks, oldest_s| Boundary { at, clean, tasks, oldest_s };
    // pane 1: a day and a half of rows, one clean rest every three hours
    for k in 0..12 {
        f.boundary(1, b(T0 + k * 3 * HOUR, true, 0, None));
        f.boundary(1, b(T0 + k * 3 * HOUR + 60, false, 1, Some(100 * (k as u32 + 1))));
    }
    // pane 2: an hour of rows, none clean
    f.boundary(2, b(T0, false, 2, Some(5000)));
    f.seen(T0 + HOUR, 2);
    let now = T0 + 36 * HOUR;
    let fc = build(&f, now, &[1, 2]);
    let (p1, p2) = (fc.panes[0], fc.panes[1]);
    assert_eq!(p1.clean_from, Source::Pane);
    assert!((p1.clean_per_h - 12.0 * 3600.0 / (33.0 * 3600.0 + 60.0)).abs() < 1e-12, "{p1:?}");
    assert_eq!(p2.clean_from, Source::Fleet);
    assert!((p2.clean_per_h - 12.0 * 3600.0 / (34.0 * 3600.0 + 60.0)).abs() < 1e-12);
    assert_eq!((p1.background_s, p1.background_from), (Some(980), Source::Pane));
    assert_eq!(p2.background_from, Source::Fleet);
    // no boundary fact at all: never rests cleanly
    let mut g = Facts::default();
    g.seen(T0, 3);
    let fc = build(&g, now, &[3]);
    assert_eq!((fc.panes[0].clean_per_h, fc.panes[0].clean_from, fc.panes[0].background_s), (0.0, Source::Nothing, None));
    g.set_helpers([("agent-a", 60), ("agent-a", 1200), ("agent-a", 3000), ("agent-a", 90), ("agent-b", 30)]);
    let lives = build(&g, now, &[]).helpers;
    assert_eq!(lives[0], HelperLife { agent: "agent-a".into(), finished: 4, p75_s: 1650 });
    assert_eq!(lives[1].p75_s, HELPER_FLOOR_S, "at least ten minutes");
}

/// Fed fact by fact with an hourly prune, or every kept fact at once:
/// the same store and the same forecast, past the fifteen days kept.
#[test]
fn forecast_rebuild_equals_incremental() {
    let mut all: Vec<(i64, u64, u8, f64)> = Vec::new();
    let mut seed = 7u64;
    let mut rnd = || {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) as f64 / (1u64 << 31) as f64
    };
    let span = 20 * 24 * HOUR;
    let mut at = T0;
    while at < T0 + span {
        at += (rnd() * 900.0) as i64 + 1;
        let sid = 1 + (rnd() * 12.0) as u64;
        all.push((at, sid, (rnd() * 3.0) as u8, rnd() * 0.002));
    }
    let feed = |f: &mut Facts, e: &(i64, u64, u8, f64)| match e.2 {
        0 => f.used(e.0, e.1, Class::Plain, e.3),
        1 => f.used(e.0, e.1, Class::Restricted, e.3),
        _ => f.boundary(e.1, Boundary { at: e.0, clean: e.3 < 0.001, tasks: 1, oldest_s: Some((e.3 * 1e6) as u32) }),
    };
    let now = T0 + span;
    let mut inc = Facts::default();
    let mut last_hour = T0;
    for e in &all {
        if e.0 - last_hour >= HOUR {
            last_hour = e.0;
            inc.prune(e.0);
        }
        feed(&mut inc, e);
    }
    inc.prune(now);
    let mut once = Facts::default();
    for e in all.iter().filter(|e| e.0 >= keep_from(now)) {
        feed(&mut once, e);
    }
    once.prune(now);
    assert_eq!(inc, once);
    let live: Vec<u64> = (1..=12).collect();
    assert_eq!(build(&inc, now, &live), build(&once, now, &live));
    assert!(build(&inc, now, &live).pane_band[1].pairs > 100);
}

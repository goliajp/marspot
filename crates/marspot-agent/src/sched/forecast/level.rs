//! Levels: a pane's and the fleet's speed by class, and the
//! back-tests the bands and the time-of-day test are measured on.
//!
//! A level at `t` is what was used in the whole hours that began in
//! the `h` hours before `t`, over the time the pane was watched in
//! them.  At an hour boundary that is exactly the last `h` hours.

use super::facts::{Facts, PaneFacts, keep_from};
use super::{DAY_H, FLOOR, HOUR, HORIZON_H, SHORT_H, YOUNG_S};

/// Pane `p`'s mean speed of `class` over the `h` hours before `t`,
/// weeks an hour; at least half an hour watched is assumed.
pub fn pane_mean(p: &PaneFacts, class: usize, t: i64, h: i64) -> f64 {
    let from = t - h * HOUR;
    // the first whole hour in the window, or the pane's first hour
    let start = (from + HOUR - 1).div_euclid(HOUR) * HOUR;
    let first = p.first_at().map_or(start, |f| f.div_euclid(HOUR) * HOUR);
    let span = (t - start.max(first)).max(HOUR / 2) as f64 / HOUR as f64;
    p.used(class, start, t) / span
}

/// The forecast of the pane's mean over the next `HORIZON_H` hours at
/// `t`: the three-hour mean for the first three hours, the 24-hour mean after.
pub fn pane_forecast(p: &PaneFacts, class: usize, t: i64) -> f64 {
    let short = pane_mean(p, class, t, SHORT_H);
    let day = pane_mean(p, class, t, DAY_H);
    (short * SHORT_H as f64 + day * (HORIZON_H - SHORT_H) as f64) / HORIZON_H as f64
}

/// Every measured ratio (actual six-hour mean ÷ forecast) of every
/// pane, per class, as of `now`.  An origin is a whole hour at which
/// the pane was at least half an hour old, with a day of kept facts
/// before it and the pane still there six hours later; a forecast under
/// `FLOOR` is no ratio.
pub fn pane_ratios(f: &Facts, now: i64) -> [Vec<f64>; 2] {
    let mut out = [Vec::new(), Vec::new()];
    let from = keep_from(now) + DAY_H * HOUR;
    let done = now.div_euclid(HOUR) * HOUR;
    for p in f.panes.values() {
        let Some(first) = p.first_at() else { continue };
        let mut t = ((first + YOUNG_S).max(from) + HOUR - 1).div_euclid(HOUR) * HOUR;
        while t + HORIZON_H * HOUR <= done.min(p.last_at) {
            for (c, v) in out.iter_mut().enumerate() {
                let fc = pane_forecast(p, c, t);
                if fc >= FLOOR {
                    v.push(p.used(c, t, t + HORIZON_H * HOUR) / HORIZON_H as f64 / fc);
                }
            }
            t += HOUR;
        }
    }
    out
}

/// The fleet hour starting at `at`, if a shell saw it.
fn known(f: &Facts, at: i64) -> Option<[f64; 2]> {
    f.hours.get(&at).copied().flatten()
}

/// The fleet's mean over the `n` hours from `at`, if every one is known.
fn fleet_mean(f: &Facts, class: usize, at: i64, n: i64) -> Option<f64> {
    (0..n).map(|k| known(f, at + k * HOUR).map(|h| h[class])).sum::<Option<f64>>().map(|s| s / n as f64)
}

/// Fleet origins: whole hours with a known day before and six known hours after.
pub fn fleet_origins(f: &Facts) -> Vec<i64> {
    let (Some((&first, _)), Some((&last, _))) = (f.hours.first_key_value(), f.hours.last_key_value()) else { return Vec::new() };
    let mut t = first + DAY_H * HOUR;
    let mut out = Vec::new();
    while t + HORIZON_H * HOUR <= last + HOUR {
        if (-DAY_H..HORIZON_H).all(|k| known(f, t + k * HOUR).is_some()) {
            out.push(t);
        }
        t += HOUR;
    }
    out
}

/// The fleet's ratios: actual six-hour mean ÷ the 24-hour mean before it.
pub fn fleet_ratios(f: &Facts) -> [Vec<f64>; 2] {
    let mut out = [Vec::new(), Vec::new()];
    for t in fleet_origins(f) {
        for (c, v) in out.iter_mut().enumerate() {
            let fc = fleet_mean(f, c, t - DAY_H * HOUR, DAY_H).unwrap_or(0.0);
            if fc >= FLOOR {
                v.push(fleet_mean(f, c, t, HORIZON_H).unwrap_or(0.0) / fc);
            }
        }
    }
    out
}

/// The fleet's level: the mean of the known hours among the 24 that
/// ended before `now`.
pub fn fleet_level(f: &Facts, class: usize, now: i64) -> Option<f64> {
    let done = now.div_euclid(HOUR) * HOUR;
    let v: Vec<f64> = (1..=DAY_H).filter_map(|k| known(f, done - k * HOUR)).map(|h| h[class]).collect();
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}

/// The mean of each whole six-hour bucket (from 00, 06, 12, 18 UTC) in
/// the seven days before `now` whose every hour is known, oldest first.
pub fn buckets(f: &Facts, class: usize, now: i64) -> Vec<f64> {
    let width = HORIZON_H * HOUR;
    let done = now.div_euclid(width) * width;
    let mut t = done - 7 * 24 * HOUR;
    let mut out = Vec::new();
    while t + width <= done {
        out.extend(fleet_mean(f, class, t, HORIZON_H));
        t += width;
    }
    out
}

/// The highest of [`buckets`].
pub fn a_hi(f: &Facts, class: usize, now: i64) -> Option<f64> {
    buckets(f, class, now).into_iter().reduce(f64::max)
}

/// The time-of-day test of one class: mean absolute error of the
/// 24-hour mean and of the profile (each hour's mean over the earlier
/// days at that hour of day) at the same origins.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimeOfDay {
    pub origins: usize,
    pub mae_day: f64,
    pub mae_profile: f64,
    /// The profile beats the 24-hour mean by 10%.
    pub on: bool,
}

pub fn time_of_day(f: &Facts, class: usize) -> TimeOfDay {
    let (mut n, mut e_day, mut e_prof) = (0usize, 0.0, 0.0);
    for t in fleet_origins(f) {
        let profile: Option<f64> = (0..HORIZON_H).map(|k| hour_of_day_mean(f, class, t + k * HOUR, t)).sum();
        let (Some(p), Some(day), Some(actual)) =
            (profile, fleet_mean(f, class, t - DAY_H * HOUR, DAY_H), fleet_mean(f, class, t, HORIZON_H))
        else {
            continue;
        };
        n += 1;
        e_day += (actual - day).abs();
        e_prof += (actual - p / HORIZON_H as f64).abs();
    }
    let (mae_day, mae_profile) = if n > 0 { (e_day / n as f64, e_prof / n as f64) } else { (0.0, 0.0) };
    TimeOfDay { origins: n, mae_day, mae_profile, on: n > 0 && mae_profile < 0.9 * mae_day }
}

/// The mean of the known hours before `before` at `at`'s hour of day.
fn hour_of_day_mean(f: &Facts, class: usize, at: i64, before: i64) -> Option<f64> {
    let day = 24 * HOUR;
    let first = *f.hours.first_key_value()?.0;
    // the latest hour before `before` at that hour of day
    let mut h = at - ((at - before).div_euclid(day) + 1) * day;
    let (mut sum, mut n) = (0.0, 0usize);
    while h >= first {
        if let Some(r) = known(f, h) {
            sum += r[class];
            n += 1;
        }
        h -= day;
    }
    (n > 0).then(|| sum / n as f64)
}

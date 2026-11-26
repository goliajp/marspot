//! The planner's demand forecast: levels with a band, clean-rest
//! rates, background-work lengths and helper lifetimes.
//!
//! Everything here is derived.  `Facts` holds what the fact files keep
//! (use, turn ends, the hourly roll-up, finished helpers), and `build`
//! computes the whole `Forecast` from it at one moment: deleting a
//! forecast and building again from the same facts gives the same one,
//! whether the facts came one at a time or all at once.  Nothing here
//! reads a file, and nothing reads the forecast for a decision yet.
//!
//! Units: speeds are weeks an hour (`0.01` is one point an hour).

pub mod band;
pub mod facts;
pub mod level;
mod line;
pub mod rests;

pub use band::{Band, BandFrom};
pub use facts::{Boundary, Facts, HourRow, ix, keep_from};
pub use level::TimeOfDay;
pub use rests::{HelperLife, Source};

pub const HOUR: i64 = 3600;
/// What `alloc_events` keeps.
pub const KEEP_S: i64 = 15 * 24 * HOUR;
/// The level's long and short windows and the horizon a band is
/// measured over, hours.
pub const DAY_H: i64 = 24;
pub const SHORT_H: i64 = 3;
pub const HORIZON_H: i64 = 6;
/// Facts needed before a band is measured instead of defaulted, hours.
pub const MEASURED_AFTER_H: i64 = 7 * 24;
/// A pane younger than this counts only in fleet demand.
pub const YOUNG_S: i64 = 1800;
/// A forecast under this (0.01 points an hour) gives no ratio.
pub const FLOOR: f64 = 1e-4;
pub const HELPER_FLOOR_S: u32 = 600;

/// One class of the fleet.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fleet {
    /// The 24-hour mean of the hourly roll-up.
    pub level: Option<f64>,
    /// Measured on the fleet's own six-hour actuals.
    pub band: Band,
    /// The highest six-hour bucket of the last seven days.
    pub a_hi: Option<f64>,
    pub time_of_day: TimeOfDay,
}

/// One live pane, per class (restricted first).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pane {
    pub sid: u64,
    /// Older than half an hour: it counts at pane level.
    pub counts: bool,
    pub short: [f64; 2],
    pub day: [f64; 2],
    pub clean_per_h: f64,
    pub clean_from: Source,
    pub background_s: Option<u32>,
    pub background_from: Source,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Forecast {
    pub at: i64,
    pub fleet: [Fleet; 2],
    /// Measured on every pane's six-hour actuals against its forecast.
    pub pane_band: [Band; 2],
    pub panes: Vec<Pane>,
    pub clean_per_h_fleet: Option<f64>,
    pub background_s_fleet: Option<u32>,
    pub helpers: Vec<HelperLife>,
}

fn default_band(class: usize) -> (f64, f64) {
    if class == 0 { band::DEFAULT_RESTRICTED } else { band::DEFAULT_PLAIN }
}

/// The forecast at `now` from `f`, for the live panes `live`.
pub fn build(f: &Facts, now: i64, live: &[u64]) -> Forecast {
    let known_hours = f.hours.values().filter(|h| h.is_some()).count() as i64;
    let mut fleet_r = level::fleet_ratios(f);
    let fleet = [0, 1].map(|c| Fleet {
        level: level::fleet_level(f, c, now),
        band: band::measure(&mut fleet_r[c], default_band(c), known_hours >= MEASURED_AFTER_H),
        a_hi: level::a_hi(f, c, now),
        time_of_day: level::time_of_day(f, c),
    });
    let oldest = f.panes.values().filter_map(|p| p.hours.keys().next()).min().copied();
    let enough = oldest.is_some_and(|o| now - o >= MEASURED_AFTER_H * HOUR);
    let mut pane_r = level::pane_ratios(f, now);
    let pane_band = [0, 1].map(|c| band::measure(&mut pane_r[c], default_band(c), enough));
    let clean_fleet = rests::fleet_clean_rate(f);
    let bg_fleet = rests::fleet_background(f);
    let mut sids: Vec<u64> = live.to_vec();
    sids.sort_unstable();
    sids.dedup();
    let panes = sids
        .into_iter()
        .filter_map(|sid| {
            let p = f.panes.get(&sid)?;
            let (clean_per_h, clean_from) = rests::clean_rate(p, clean_fleet);
            let (background_s, background_from) = rests::background(p, bg_fleet);
            Some(Pane {
                sid,
                counts: p.first_at().is_some_and(|a| now - a >= YOUNG_S),
                short: [0, 1].map(|c| level::pane_mean(p, c, now, SHORT_H)),
                day: [0, 1].map(|c| level::pane_mean(p, c, now, DAY_H)),
                clean_per_h,
                clean_from,
                background_s,
                background_from,
            })
        })
        .collect();
    Forecast {
        at: now,
        fleet,
        pane_band,
        panes,
        clean_per_h_fleet: clean_fleet,
        background_s_fleet: bg_fleet,
        helpers: rests::helper_lives(f),
    }
}

#[cfg(test)]
mod band_tests;
#[cfg(test)]
mod tests;

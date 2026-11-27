//! A planned switch's window `[t_open, t_dead]` and the kind of
//! boundary it is expected to be taken at.
//!
//! `t_open` is when the switch is planned, usually now.  `t_dead` is,
//! for a switch that averts a refusal, the source's exhaustion at the
//! high band less one reading interval and one point of reading error
//! at its load; for a switch that only saves reserve, the moment its
//! remaining gain (taken to accrue evenly over the pane horizon) falls
//! under its cost.
//!
//! The kind is what the executor's stopping rule is expected to reach:
//! a clean rest when one comes before `t_dead` with a chance of 0.8;
//! else the drain notice for a session with long background work; else
//! the five-minute notice when the window is too short for anything
//! else; else a short boundary.

use super::{H2, Pane, STALL_NOTICE_H, STALL_SWITCH_H};

/// One reading interval, hours.
pub const READING_H: f64 = 5.0 / 60.0;
/// One point of reading error.
const READING_ERR: f64 = 0.01;
/// The chance of a clean rest the stopping rule asks for.
pub const CLEAN_CHANCE: f64 = 0.8;
/// The five-minute notice's lead with the longest delay to a boundary.
pub const NOTICE_LEAD_H: f64 = 25.0 / 60.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Clean,
    Short,
    Notice,
    Drain,
}

impl Kind {
    pub fn word(self) -> &'static str {
        match self {
            Kind::Clean => "clean",
            Kind::Short => "short",
            Kind::Notice => "notice",
            Kind::Drain => "drain",
        }
    }

    /// Stall hours.
    pub fn stall(self) -> f64 {
        match self {
            Kind::Clean | Kind::Short => STALL_SWITCH_H,
            Kind::Notice | Kind::Drain => STALL_NOTICE_H,
        }
    }

    pub fn notice(self) -> bool {
        matches!(self, Kind::Notice | Kind::Drain)
    }

    /// Counts against the pane's disturbing actions of the day.
    pub fn disturbing(self) -> bool {
        self != Kind::Clean
    }
}

/// The chance pane `p` reaches a clean rest within `hours`.
pub fn clean_chance(p: &Pane, hours: f64) -> f64 {
    1.0 - (-p.clean_per_h * hours.max(0.0)).exp()
}

pub fn kind(p: &Pane, t_open: f64, t_dead: f64) -> Kind {
    let w = t_dead - t_open;
    if clean_chance(p, w) >= CLEAN_CHANCE {
        Kind::Clean
    } else if p.long_background {
        Kind::Drain
    } else if w <= NOTICE_LEAD_H {
        Kind::Notice
    } else {
        Kind::Short
    }
}

/// `t_dead` for a switch off an account that empties at the high band
/// at `empty_h` under a load of `load` weeks an hour.
pub fn dead_by_exhaustion(empty_h: f64, load: f64) -> f64 {
    let err = if load > 0.0 { READING_ERR / load } else { 0.0 };
    empty_h - READING_H - err
}

/// `t_dead` for a switch that saves `gain` against a cost of `cost`.
pub fn dead_by_gain(t_open: f64, gain: f64, cost: f64) -> f64 {
    if gain <= 0.0 {
        return t_open;
    }
    t_open.max(H2 * gain / (gain + cost.max(0.0)))
}

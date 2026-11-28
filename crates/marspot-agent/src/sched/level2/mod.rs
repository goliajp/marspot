//! Level 2 of the planner: which account each pane uses over the next
//! day, and when each switch happens.
//!
//! A pane's schedule is at most two switches, six hours apart.  What a
//! set of schedules costs is found by running every account forward
//! over the pane horizon at the loads the schedules put on it (`sim`):
//! plain use taken out of the reserve at `pi`, week quota lapsing at a
//! renewal inside it or just after it, every switch's rewrite (and the
//! background work a notice-switch throws away), every refusal, less
//! what is left on each account at the end at level 1's prices.
//!
//! `search` finds the schedules: proposals ranked by linear prices,
//! each kept only if the simulated cost falls, then pair swaps and
//! two-step chains, and an exhaustive pass over joint first switches
//! when few panes are in play (`exact`).  The fleet's daily count is
//! met with a charge per counted switch.  `window` gives
//! each switch its window and kind; `standing` keeps a plan's structure
//! across passes; `trigger` says when a full search is due.  Nothing
//! here executes a switch.
//!
//! Units: weeks (`0.01` is a point), hours from now.

mod ctx;
mod exact;
pub mod exec;
pub mod exec_gate;
mod finish;
mod fit;
mod grid;
mod guard;
pub mod limits;
pub mod line;
mod moves;
pub mod pass;
mod placed;
mod planned;
mod propose;
pub mod route;
mod search;
mod sim;
mod tally;
pub mod standing;
pub mod trigger;
mod window;

pub use grid::Grid;
pub use limits::{FableGuard, Limits, Profile};
pub use planned::planned_speed;
pub use tally::{Stage, Tally};
pub use window::Kind;

use crate::quota::Quant;

pub use super::level1::H2;

/// The steady profile's numbers, which the tests and replays of the
/// planner were written against; a plan reads its own from `Input::limits`.
///
/// A pane stays this long after a planned switch, hours.
pub const DWELL_H: f64 = Limits::STEADY.dwell_h;
/// Planned switches a pane's schedule holds inside the pane horizon,
/// whatever the profile allows over a day.
pub const HOPS: usize = 2;
/// Stall hours of a refusal and of each kind of switch.
pub const STALL_REFUSAL_H: f64 = 0.25;
pub const STALL_SWITCH_H: f64 = 0.05;
pub const STALL_NOTICE_H: f64 = 0.15;
/// A plan that differs from doing nothing gains this much money
/// (0.05 points) or this many stall hours.
pub const MIN_GAIN: f64 = 0.0005;
pub const MIN_GAIN_EFF: f64 = 0.25;
/// Two costs closer than this in money are equal (0.01 points), and an
/// order among equals decides.
pub const TIE: f64 = 1e-4;
/// The fleet's planned switches a day, and notices an hour.
pub const FLEET_PER_DAY: usize = Limits::STEADY.fleet_per_day;
pub const NOTICES_PER_HOUR: usize = Limits::STEADY.notices_per_hour;
/// A switch that averts a refusal forecast this soon at the high band
/// does not count toward the fleet's day.
pub const EXEMPT_H: f64 = 2.0;
/// A target must last a newcomer this long at the high band.
pub const LASTS_H: f64 = Limits::STEADY.lasts_h;

/// What a set of schedules costs, weeks and stall hours.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Totals {
    pub lapsed: f64,
    /// Week left at the end that the loads then do not use up before
    /// the account's renewal after the horizon.
    pub lapse_after: f64,
    /// Plain use beyond each account's spare, before `pi`.
    pub reserve_taken: f64,
    pub rewrites: f64,
    pub lost_work: f64,
    /// Rewrites paid at refusals.
    pub refused: f64,
    /// What is left at the end, at level 1's prices.
    pub left_value: f64,
    pub unserved: f64,
    pub money: f64,
    pub eff: f64,
    pub refusals: u16,
    pub switches: u16,
}

/// One account as level 2 sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct Account {
    pub q: Quant,
    /// What the plain class may take before the renewal without
    /// touching the reserve (level 1).
    pub spare: f64,
    /// One more point at the end of the pane horizon is worth this
    /// many points, `[usable by every class, plain only]` (level 1).
    pub price: [f64; 2],
}

/// One pane that counts at pane level.
#[derive(Clone, Debug, PartialEq)]
pub struct Pane {
    pub sid: u64,
    /// The account it is on, by id.
    pub on: u8,
    /// It needs the restricted window.
    pub restricted: bool,
    /// `[restricted, plain]` level and high band, weeks an hour.
    pub speed: [f64; 2],
    pub high: [f64; 2],
    /// What a switch rewrites, weeks; 0 when the cache has expired.
    pub rewrite: f64,
    /// What a notice-switch throws away of live background work, weeks.
    pub lost_work: f64,
    /// Clean rests an hour.
    pub clean_per_h: f64,
    /// Its background work runs past a quarter of an hour.
    pub long_background: bool,
    /// No planned switch before this, hours: the dwell after the last
    /// switch, a pin, a change by hand.
    pub earliest_h: f64,
    /// An account it left, and the hours until it may go back.
    pub left: Option<(u8, f64)>,
    /// Accounts that refused it since the feed was written, by id bit.
    pub barred: u64,
    /// Planned switches it may still take this week, and disturbing
    /// actions today.
    pub switches_left: u8,
    pub disturb_left: u8,
}

/// Everything one plan reads.
#[derive(Clone, Debug)]
pub struct Input<'a> {
    pub accounts: &'a [Account],
    pub panes: &'a [Pane],
    /// Fleet demand below pane level (young panes, helpers), weeks an hour.
    pub young: [f64; 2],
    /// What a renewed restricted window holds, weeks.
    pub window_full: f64,
    pub pi: f64,
    /// Planned switches the fleet may still make today, and notices
    /// this hour.
    pub fleet_left: usize,
    pub notices_left: usize,
    /// The feed is stale: no window opens.
    pub stale_feed: bool,
    pub limits: Limits,
}

/// One planned switch.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Switch {
    pub sid: u64,
    pub from: u8,
    pub to: u8,
    /// Its place in the pane's schedule, 0 or 1.
    pub order: u8,
    pub t_open: f64,
    pub t_dead: f64,
    pub kind: Kind,
    /// It averts a refusal within `EXEMPT_H`: not in the fleet's count.
    pub exempt: bool,
    /// What the plan saves by it, weeks, and stall hours: a switch that
    /// only averts a refusal may save no money (the refusal pays the same
    /// rewrite) and still save the refusal's stall.
    pub gain: f64,
    pub gain_eff: f64,
    /// The gains are the ones stored with the standing plan, not worked
    /// out on this pass (a re-time, a switch not yet open that averts a
    /// refusal).
    pub carried: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Every movable pane's choices enumerated jointly.
    Exact,
    /// The active panes' choices enumerated jointly, the others kept.
    Active,
    Search,
}

impl Mode {
    pub fn word(self) -> &'static str {
        match self {
            Mode::Exact => "exact",
            Mode::Active => "active",
            Mode::Search => "search",
        }
    }
}

/// The plan of one pass.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub mode: Mode,
    pub switches: Vec<Switch>,
    /// Each pane's schedule, by input order.
    pub scheds: Vec<Sched>,
    pub totals: Totals,
    /// Doing nothing, for the gain.
    pub idle: Totals,
    /// Per account, by input order: when it empties at the level and at
    /// the high band under the plan, hours.
    pub empty: Vec<[Option<f64>; 2]>,
    /// Per account, by input order: when it empties at the high band if
    /// nobody moves, capped at the horizon.
    pub deadline: Vec<f64>,
    /// Refusals the plan still foresees: pane, hours.
    pub refusals: Vec<(u64, f64)>,
    /// Panes searched, simulations run, the charge per switch.
    pub active: usize,
    pub sims: usize,
    /// The simulations by what each was run for.
    pub tally: Tally,
    pub charge: f64,
    pub cuts: Vec<f64>,
}

/// The plan for `input`, starting from `start` (the standing plan's
/// schedules, re-timed) when given.
pub fn plan(input: &Input, start: Option<&[Sched]>) -> Plan {
    search::run(input, start)
}

/// The plan of exactly `scheds`: costed and timed, nothing searched.
pub fn evaluate(input: &Input, scheds: &[Sched]) -> Plan {
    search::evaluate(input, scheds)
}

/// One pane's switches: to an account index at a cut index.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hop {
    pub to: u8,
    pub at: u8,
}

/// A pane's schedule over the pane horizon.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sched {
    pub hops: [Hop; HOPS],
    pub n: u8,
}

impl Sched {
    pub const STAY: Sched = Sched { hops: [Hop { to: 0, at: 0 }; HOPS], n: 0 };

    pub fn one(to: usize, at: usize) -> Sched {
        let mut s = Sched::STAY;
        s.push(to, at);
        s
    }

    pub fn push(&mut self, to: usize, at: usize) {
        self.hops[self.n as usize] = Hop { to: to as u8, at: at as u8 };
        self.n += 1;
    }

    pub fn hops(&self) -> &[Hop] {
        &self.hops[..self.n as usize]
    }
}

#[cfg(test)]
mod cases_tests;
#[cfg(test)]
mod loop_digest_tests;
#[cfg(test)]
mod loop_plan;
#[cfg(test)]
mod loop_price;
#[cfg(test)]
mod loop_profile_tests;
#[cfg(test)]
mod loop_tests;
#[cfg(test)]
mod loop_world;
#[cfg(test)]
mod oracle_tests;
#[cfg(test)]
mod rows_tests;
#[cfg(test)]
pub(crate) mod tests;

//! Which account each pane should be on, shared by every agent.
//!
//! Pure functions over what an agent's adapter knows: each account's
//! windows, each pane's class and speed.  Nothing here reads a file or
//! knows which agent it is serving.
//!
//! Units: quota is a fraction of an account's week (1.0 is the whole
//! week, 0.01 one point); time is hours; speed is week fractions per
//! hour.
//!
//! An account's week left, `R`, splits into what the restricted model
//! (Claude's Fable) can still use, `Φ = min(κ (1 − f), R)`, and what
//! only the other models can, `N = R − Φ`: Fable counts against the
//! week too, but the week can be spent without Fable.  `κ` is the share
//! of a week a full restricted window is worth; where it is not known
//! nothing is reserved for that model and `Φ` only says whether a pane
//! may go there.

pub(crate) mod choose;
pub(crate) mod notice;
pub(crate) mod plan;
pub(crate) mod replay;
pub(crate) mod rest;
pub(crate) mod speed;

#[cfg(test)]
mod sim;
#[cfg(test)]
mod sim_policies;
#[cfg(test)]
mod sim_gate;
#[cfg(test)]
mod sim_tests;
#[cfg(test)]
mod snapshot_tests;
#[cfg(test)]
mod core_tests;

/// The share of a Claude week a full Fable window is worth.
///
/// A regression over 32 account-days (2026-11-17 to 11-24) of each
/// day's week rise on its Fable rise plus its non-Fable dollars; left
/// one day out at a time it stays within 0.512 to 0.537.  The plain
/// ratio of the two rises reads higher (about 0.57), because it counts
/// the same day's Opus use as Fable's.
pub(crate) const KAPPA_FABLE: f64 = 0.528;

/// A pane's speed when nothing has been measured: 0.67 points an hour,
/// the control plane's use per busy pane over 2026-11-21 to 11-24.
pub(crate) const RHO0: f64 = 0.0067;

/// How far ahead the allocation looks, in hours.
pub(crate) const HORIZON_H: usize = 168;

/// Below this an amount is nothing (float noise from the forward pass).
pub(crate) const EPS: f64 = 1e-9;

/// What a pane runs, as far as quota goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Class {
    /// Any model the whole week can pay for.
    Plain,
    /// A model with a window of its own on top of the week (Fable).
    Restricted,
}

/// One account as the core sees it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Quant {
    pub id: u8,
    /// Week left, `R`.  May dip below zero in a simulation.
    pub r: f64,
    /// The restricted window left, in weeks: `κ (1 − f)`, not yet
    /// capped by `R`.  Where `κ` is unknown, `R` while the window has
    /// room and 0 once it is spent.
    pub f: f64,
    /// Hours until the week renews, `T`.
    pub t: f64,
    /// Hours until the restricted window renews.
    pub tf: f64,
    /// The account takes new work right now (no five-hour window or
    /// login problem in the way).
    pub open: bool,
    /// The reading is old (over 30 minutes): a must-move target only
    /// after every fresh one, never a resting move's target.
    pub stale: bool,
}

impl Quant {
    /// `Φ`: what the restricted model can still use.
    pub(crate) fn phi(&self) -> f64 {
        self.f.min(self.r).max(0.0)
    }

    /// `N`: what only the other models can use.
    pub(crate) fn n(&self) -> f64 {
        (self.r - self.f.max(0.0)).max(0.0)
    }

    pub(crate) fn accepts(&self, class: Class) -> bool {
        self.open && self.r > EPS && (class == Class::Plain || self.f > EPS)
    }

    /// The most this account can take an hour and still last until it
    /// renews: `R / T`.
    pub(crate) fn rate_cap(&self) -> f64 {
        self.r.max(0.0) / self.t.max(0.5)
    }

    /// The renewal that orders this account for `class`.
    pub(crate) fn renews_in(&self, class: Class) -> f64 {
        match class {
            Class::Plain => self.t,
            Class::Restricted => self.tf,
        }
    }
}

/// What the panes on one account use an hour, by class.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Load {
    pub restricted: f64,
    pub plain: f64,
}

impl Load {
    pub(crate) fn total(&self) -> f64 {
        self.restricted + self.plain
    }

    pub(crate) fn of(&self, class: Class) -> f64 {
        match class {
            Class::Plain => self.plain,
            Class::Restricted => self.restricted,
        }
    }

    pub(crate) fn add(&mut self, class: Class, rate: f64) {
        match class {
            Class::Plain => self.plain += rate,
            Class::Restricted => self.restricted += rate,
        }
    }
}

/// Hours until account `q` runs out for a pane of `class`, with
/// `extra` more an hour on top of `load`.
pub(crate) fn lasts(q: &Quant, load: &Load, class: Class, extra: f64) -> f64 {
    let total = load.total() + extra;
    let restricted = load.restricted + if class == Class::Restricted { extra } else { 0.0 };
    let mut t = if total > 0.0 { q.r / total } else { 1e9 };
    if class == Class::Restricted && restricted > 0.0 {
        t = t.min(q.f / restricted);
    }
    t
}

/// Demand is more than the accounts can give before they renew: the
/// panes' speeds add up to more than `Σ R / T`.
pub(crate) fn shortage(accounts: &[Quant], demand: f64) -> bool {
    demand > accounts.iter().map(Quant::rate_cap).sum::<f64>()
}

//! The planner's class plan over Claude's accounts, in shadow: worked
//! out when its inputs change (an account's reading or stored resets,
//! the hourly forecast) and logged as `sched.level1` when the line
//! changes.  Nothing decides by it yet.
//!
//! Demand is the forecast's 24-hour fleet level per class, Fable's
//! floored by the readings' slope; before the forecast has a level, the
//! scan's own rates, rounded to a tenth of a point an hour so that the
//! plan is not worked out again at every scan.  The reserve is taken at
//! the forecast's highest six-hour Fable bucket, never below the level.
//! No Claude account reports a five-hour reading, so no account is
//! closed for an interval.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use marspot_agent::sched::forecast::Forecast;
use marspot_agent::sched::level1::{self, Level1, Offer, World};

use super::allocation_inputs::AccountIn;
use super::allocation_view::View;
use crate::plugins::allocation::{KAPPA_FABLE, Quant};

/// A fallback rate is rounded to this, weeks an hour.
const ROUND: f64 = 0.001;

#[derive(Default)]
pub(super) struct ClassPlan {
    key: Option<u64>,
    said: String,
    last_pi: Option<f64>,
    /// The inputs and the plan last worked out, for level 2.
    last: Option<(Inputs, Level1)>,
}

/// What one plan is worked out from.
#[derive(Debug, PartialEq)]
pub(super) struct Inputs {
    pub accounts: Vec<Quant>,
    pub offers: Vec<Offer>,
    pub demand: [f64; 2],
    pub prudent: f64,
    pub buckets: Vec<f64>,
}

/// The inputs from the scan's accounts and view, the forecast last
/// built, and `slope_f`, the Fable use the readings show.
pub(super) fn inputs(accounts: &[AccountIn], view: &View, forecast: Option<&Forecast>, slope_f: f64, now: i64) -> Inputs {
    let rate_on = |p: u8| view.panes.iter().filter(|r| r.1 == p).map(|r| r.3).sum();
    let level = |c: usize, scan: f64| forecast.and_then(|f| f.fleet[c].level).unwrap_or((scan / ROUND).round() * ROUND);
    let demand = [level(0, view.b_f).max(slope_f), level(1, view.b_n)];
    let a_hi = forecast.and_then(|f| f.fleet[0].a_hi).unwrap_or(0.0);
    Inputs {
        accounts: accounts.iter().map(|a| a.reading(now, rate_on(a.profile))).collect(),
        offers: accounts.iter().flat_map(|a| a.supply.iter().map(|s| Offer { account: a.profile, supply: *s })).collect(),
        demand,
        prudent: a_hi.max(demand[0]),
        buckets: forecast.map(|f| f.fleet[0].buckets.clone()).unwrap_or_default(),
    }
}

/// What changes the plan: the readings and resets as read, the
/// forecast's hour and the demand.
fn key(accounts: &[AccountIn], forecast: Option<&Forecast>, i: &Inputs) -> u64 {
    let mut h = DefaultHasher::new();
    for a in accounts {
        (a.profile, a.week_used.to_bits(), a.fable_used.map(f64::to_bits), a.week_renews, a.fable_renews, a.open, a.measured_at).hash(&mut h);
        format!("{:?}", a.supply).hash(&mut h);
    }
    forecast.map(|f| f.at).hash(&mut h);
    i.demand.map(f64::to_bits).hash(&mut h);
    h.finish()
}

impl ClassPlan {
    /// The inputs and the class plan last worked out.
    pub(super) fn last(&self) -> Option<&(Inputs, Level1)> {
        self.last.as_ref()
    }

    /// One scan: the line when the inputs changed and the plan's line with them.
    pub(super) fn scan(&mut self, accounts: &[AccountIn], view: &View, forecast: Option<&Forecast>, slope_f: f64, now: i64) -> Option<String> {
        let i = inputs(accounts, view, forecast, slope_f, now);
        let key = key(accounts, forecast, &i);
        if i.accounts.is_empty() || self.key == Some(key) {
            return None;
        }
        self.key = Some(key);
        let w = World {
            accounts: &i.accounts,
            window_full: KAPPA_FABLE,
            demand: i.demand,
            prudent: i.prudent,
            buckets: &i.buckets,
            offers: &i.offers,
            closures: &[],
            last_pi: self.last_pi,
        };
        let plan = level1::plan(&w);
        self.last_pi = Some(plan.pi);
        let ids: Vec<u8> = i.accounts.iter().map(|q| q.id).collect();
        let line = plan.line(&ids);
        self.last = Some((i, plan));
        (line != self.said).then(|| {
            self.said = line.clone();
            line
        })
    }
}

#[cfg(test)]
#[path = "allocation_level1_tests.rs"]
mod tests;

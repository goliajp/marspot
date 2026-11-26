//! What a point of an account's quota is worth, per constraint.
//!
//! Each account has two prices: one for its week, one for its
//! restricted window.  When a class's demand is more than all accounts
//! can give that class before they renew, every account's price for it
//! is 1.  Otherwise each account's price goes from 1 to 0 as the time
//! it is expected to run out moves past its renewal: `x = (renewal −
//! run out) / time to renewal`, 1 at `x ≥ 0.2`, 0 at `x ≤ −0.2`,
//! linear between.  A new price within 0.2 of the one last used keeps
//! the old one, so a price does not flip from scan to scan.

use crate::quota::{Class, EPS, Load, Quant, shortage};

/// Half the width of the ramp, as a share of the time to renewal.
pub const RAMP: f64 = 0.2;

/// A new price closer than this to the last one used is not taken.
pub const HYSTERESIS: f64 = 0.2;

/// Where a price counts as crossing (logged as `price_cross`).
pub const CROSS_AT: f64 = 0.5;

/// One account's prices.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Price {
    /// For its week.
    pub week: f64,
    /// For its restricted window.
    pub window: f64,
}

impl Price {
    /// What a pane of `class` pays: a restricted pane spends both.
    pub fn of(self, class: Class) -> f64 {
        match class {
            Class::Plain => self.week,
            Class::Restricted => self.week.max(self.window),
        }
    }
}

/// Whether each class is short across all accounts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pool {
    pub plain: bool,
    pub restricted: bool,
}

/// The pool for `accounts` with `loads` (every pane counted).
pub fn pool(accounts: &[Quant], loads: &[Load]) -> Pool {
    let total: f64 = loads.iter().map(Load::total).sum();
    let restricted: f64 = loads.iter().map(|l| l.restricted).sum();
    let window_cap: f64 = accounts.iter().map(|q| q.phi() / q.tf.max(0.5)).sum();
    Pool { plain: shortage(accounts, total), restricted: restricted > EPS && restricted > window_cap }
}

/// The ramp for something `left` that `rate` uses an hour and that
/// renews in `renew_h` hours.
fn ramp(left: f64, rate: f64, renew_h: f64) -> f64 {
    if rate <= EPS {
        return 0.0;
    }
    let renew = renew_h.max(0.5);
    let x = (renew - left.max(0.0) / rate) / renew;
    ((x + RAMP) / (2.0 * RAMP)).clamp(0.0, 1.0)
}

/// Account `q`'s fresh prices with `load` on it.
pub fn price(q: &Quant, load: &Load, pool: Pool) -> Price {
    let week = if pool.plain { 1.0 } else { ramp(q.r, load.total(), q.t) };
    let window = if pool.restricted { 1.0 } else { ramp(q.f, load.restricted, q.tf) };
    Price { week, window }
}

/// The price to use given the last one used: the fresh one only when it
/// moved by `HYSTERESIS` or more.
pub fn held(fresh: f64, last: Option<f64>) -> f64 {
    match last {
        Some(l) if (fresh - l).abs() < HYSTERESIS => l,
        _ => fresh,
    }
}

/// A price went from one side of `CROSS_AT` to the other.
pub fn crossed(last: Option<f64>, now: f64) -> bool {
    last.is_some_and(|l| (l < CROSS_AT) != (now < CROSS_AT))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(r: f64, t: f64) -> Quant {
        Quant { id: 1, r, f: 0.0, t, tf: t, open: true, stale: false }
    }

    #[test]
    fn sched_price_ramps_around_the_renewal() {
        let no = Pool::default();
        // runs out at the renewal: halfway
        assert!((price(&q(0.5, 50.0), &Load { restricted: 0.0, plain: 0.01 }, no).week - 0.5).abs() < 1e-9);
        // runs out well before: full price; well after or idle: free
        assert_eq!(price(&q(0.1, 50.0), &Load { restricted: 0.0, plain: 0.01 }, no).week, 1.0);
        assert_eq!(price(&q(1.0, 50.0), &Load { restricted: 0.0, plain: 0.01 }, no).week, 0.0);
        assert_eq!(price(&q(1.0, 50.0), &Load::default(), no).week, 0.0);
        // a short pool prices everything at 1
        assert_eq!(price(&q(1.0, 50.0), &Load::default(), Pool { plain: true, restricted: false }).week, 1.0);
    }

    #[test]
    fn sched_price_holds_within_the_hysteresis_and_counts_crossings() {
        assert_eq!(held(0.6, Some(0.45)), 0.45);
        assert_eq!(held(0.7, Some(0.45)), 0.7);
        assert_eq!(held(0.3, None), 0.3);
        assert!(crossed(Some(0.45), 0.7) && !crossed(Some(0.45), 0.45) && !crossed(None, 0.9));
        assert_eq!(Price { week: 0.2, window: 0.9 }.of(Class::Restricted), 0.9);
        assert_eq!(Price { week: 0.2, window: 0.9 }.of(Class::Plain), 0.2);
    }
}

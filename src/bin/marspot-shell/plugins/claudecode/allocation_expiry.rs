//! The reconciliation of what each account's cycle left unused, in
//! points, from its readings: an `expiry` fact when an account is spent
//! (its week reads full) and when it renews.
//!
//! `week_left` is the week not used, `fable_lapsed` the Fable window
//! not used (`KAPPA_FABLE` of a week when wholly unused), and
//! `plain_in_phi` what other models used beyond the share of the week
//! Fable leaves them: the Fable-usable quota other models took.  The
//! renewal's row reads the last reading before it.  A shell started in
//! the middle of a cycle writes nothing for it until its next renewal
//! or exhaustion.

use std::collections::HashMap;

use super::super::allocation_inputs::AccountIn;
use crate::plugins::allocation::KAPPA_FABLE;

/// A week reading this full is spent.
const SPENT: f64 = 0.995;
/// A renewal moving less than this is the same renewal read again, seconds.
const SAME_RENEWAL_S: i64 = 3600;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Cycle {
    renews: i64,
    spent_said: bool,
    week: f64,
    fable: Option<f64>,
}

#[derive(Debug, Default)]
pub(super) struct Expiry {
    cycles: HashMap<u8, Cycle>,
}

/// `why=… week_left=… fable_lapsed=… plain_in_phi=… fable=…`, points.
fn detail(why: &str, week: f64, fable: Option<f64>) -> String {
    let pts = |x: f64| format!("{:.1}", 100.0 * x);
    let week_left = (1.0 - week).max(0.0);
    let lapsed = fable.map_or_else(|| "-".to_string(), |f| pts(KAPPA_FABLE * (1.0 - f).max(0.0)));
    let plain = (week - KAPPA_FABLE * fable.unwrap_or(0.0) - (1.0 - KAPPA_FABLE)).max(0.0);
    let f = fable.map_or_else(|| "-".to_string(), |f| format!("{f}"));
    format!("why={why} week_left={} fable_lapsed={lapsed} plain_in_phi={} week={week} fable={f}", pts(week_left), pts(plain))
}

impl Expiry {
    /// The rows `a`'s reading calls for: profile and detail.
    pub(super) fn note(&mut self, a: &AccountIn) -> Option<String> {
        let now = Cycle { renews: a.week_renews, spent_said: false, week: a.week_used, fable: a.fable_used };
        let Some(c) = self.cycles.get_mut(&a.profile) else {
            self.cycles.insert(a.profile, Cycle { spent_said: a.week_used >= SPENT, ..now });
            return None;
        };
        if a.week_renews - c.renews >= SAME_RENEWAL_S {
            let row = detail("renewed", c.week, c.fable);
            *c = now;
            return Some(row);
        }
        c.renews = c.renews.max(a.week_renews);
        (c.week, c.fable) = (a.week_used, a.fable_used);
        if a.week_used >= SPENT && !c.spent_said {
            c.spent_said = true;
            return Some(detail("spent", a.week_used, a.fable_used));
        }
        None
    }

    pub(super) fn retain(&mut self, profiles: &[u8]) {
        self.cycles.retain(|p, _| profiles.contains(p));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc(week: f64, fable: Option<f64>, renews: i64) -> AccountIn {
        AccountIn { profile: 3, week_used: week, fable_used: fable, fable_known: true, week_renews: renews, fable_renews: renews, open: true, measured_at: 0, supply: Vec::new() }
    }

    #[test]
    fn allocation_expiry_writes_spent_once_and_the_renewal_with_the_last_reading() {
        let mut e = Expiry::default();
        assert_eq!(e.note(&acc(0.50, Some(0.0), 1_000_000)), None, "the first reading only starts the cycle");
        assert_eq!(e.note(&acc(0.90, Some(0.10), 1_000_000)), None);
        // P3 of 2026-11-27: spent with Fable at 18%
        let spent = e.note(&acc(1.0, Some(0.18), 1_000_100)).unwrap();
        assert!(spent.starts_with("why=spent week_left=0.0 fable_lapsed=43.3 plain_in_phi=43.3 "), "{spent}");
        assert_eq!(e.note(&acc(1.0, Some(0.18), 1_000_100)), None, "spent is said once");
        let renewed = e.note(&acc(0.0, Some(0.0), 1_604_800)).unwrap();
        assert!(renewed.starts_with("why=renewed week_left=0.0 fable_lapsed=43.3 "), "{renewed}");
        assert_eq!(e.note(&acc(0.6, Some(0.95), 1_604_800)), None);
        let left = e.note(&acc(0.0, Some(0.0), 2_209_600)).unwrap();
        assert!(left.starts_with("why=renewed week_left=40.0 fable_lapsed=2.6 plain_in_phi=0.0 "), "{left}");
    }
}

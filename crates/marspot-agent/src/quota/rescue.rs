//! Saving what would lapse, in a shortage: part of rule (b).
//!
//! When demand exceeds supply every bit of the restricted window is
//! wanted, so a part of it that lapses at its renewal is restricted work
//! that does not get done.  It lapses where the restricted panes on an
//! account are too slow to use it up before the restricted window
//! renews, and nothing else sends a pane there: plan-following moves
//! are off in a shortage.  A resting restricted pane goes there when its
//! own account does not need it: the other restricted panes there use
//! it up before it renews, or that renewal is a day or more away and
//! the pane can come back.

use super::rest::{BAND_PER_HOUR, NO_RETURN_H, Resting};
use super::{Class, Load, Quant};

/// No rescue this soon after any move of the same pane: a rescue is
/// worth one move, not a pane bouncing between accounts.
pub const RESCUE_COOLDOWN_H: f64 = 12.0;

/// A pane may leave its own account's restricted window to others for
/// a while when it renews this far ahead.
pub const RESCUE_HOME_H: f64 = 24.0;

/// A rescue must save at least this many hours of the pane's use, or
/// all that is left before that window renews.
pub const RESCUE_WORTH_H: f64 = 2.0;

/// What of `q`'s restricted window lapses at its renewal with restricted
/// panes using `rate` an hour there.
///
/// It lapses at `T_F` only.  When the week renews first, the week left
/// limits the restricted model until then and the new week does not
/// after; when the restricted window renews first, only `Φ` was usable.
pub fn lapses(q: &Quant, rate: f64) -> f64 {
    let rate = rate.max(0.0);
    if q.tf <= q.t {
        return q.phi() - rate * q.tf;
    }
    let before_week = (rate * q.t).min(q.phi());
    q.f.max(0.0) - before_week - rate * (q.tf - q.t)
}

/// Where the resting restricted pane on `accounts[hi]` goes to use what
/// would otherwise lapse, earliest restricted renewal first.
///
/// `others` are the loads without this pane.
pub fn rescue(accounts: &[Quant], p: &Resting, hi: usize, others: &[Load]) -> Option<u8> {
    if p.mover.class != Class::Restricted || p.stale_feed || p.band_last_hour >= BAND_PER_HOUR {
        return None;
    }
    if p.since_move_h.is_some_and(|h| h < RESCUE_COOLDOWN_H) {
        return None;
    }
    let here = &accounts[hi];
    if lapses(here, others[hi].restricted) > 0.0 && here.tf < RESCUE_HOME_H {
        return None;
    }
    let mut best: Option<usize> = None;
    for (i, q) in accounts.iter().enumerate() {
        let returning = p.left.iter().any(|(id, h)| *id == q.id && *h < NO_RETURN_H);
        if i == hi || !q.accepts(Class::Restricted) || q.stale || returning || q.tf >= here.tf {
            continue;
        }
        if lapses(q, p.loads[i].restricted) < p.mover.rate * q.tf.min(RESCUE_WORTH_H) {
            continue;
        }
        // the week there has to last the pane as long as the rescue is worth
        if q.r / (others[i].total() + p.mover.rate) < q.t.min(RESCUE_WORTH_H) {
            continue;
        }
        if best.is_none_or(|b| q.tf < accounts[b].tf) {
            best = Some(i);
        }
    }
    best.map(|i| accounts[i].id)
}

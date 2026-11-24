//! Moving a resting pane without being refused: rule (b).
//!
//! Two reasons, checked in this order.  Leave before it runs out: the
//! pane's account will not last two hours at its current speed, so the
//! pane goes to one that lasts six, unless it is the slowest pane there
//! (one pane stays and uses up the rest, or that rest lapses at the
//! renewal; the slowest costs least when it is refused).  No cooldown
//! or count limits this one.  Follow the plan: only when supply
//! exceeds demand, a pane on an account a whole pane over its target
//! goes to one a whole pane short, if what it would use there before
//! that account renews is worth the move.

use super::choose::{MUST_LAST_H, Mover, pick_where};
use super::plan::Plan;
use super::{Class, EPS, Load, Quant, lasts, shortage};

/// An account that will not last this long sends its resting panes away.
pub(crate) const LEAVE_BELOW_H: f64 = 2.0;

/// No plan-following move this soon after any move of the same pane.
pub(crate) const BAND_COOLDOWN_H: f64 = 2.0;

/// No plan-following move back to an account left this recently.
pub(crate) const NO_RETURN_H: f64 = 6.0;

/// Plan-following moves of all panes, per hour.
pub(crate) const BAND_PER_HOUR: usize = 4;

/// What a plan-following move must be worth, in weeks: what the pane
/// would use on the target before it renews.
pub(crate) const BAND_WORTH: f64 = 0.03;

/// A plain pane using more of the restricted model than this an hour
/// (subagents) is not sent where that model is spent.
pub(crate) const RESTRICTED_USER: f64 = 0.0005;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum RestMove {
    /// Its account will not last two hours.
    LeaveEarly(u8),
    /// Supply exceeds demand and the plan wants it elsewhere.
    Band(u8),
}

/// One resting pane and what limits moving it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Resting<'a> {
    pub here: u8,
    pub mover: Mover,
    /// Every pane's load per account, this one included.
    pub loads: &'a [Load],
    /// It is the slowest pane on its account (ties by pane number).
    pub keeper: bool,
    /// Hours since this pane last moved for any reason.
    pub since_move_h: Option<f64>,
    /// Accounts it left, with hours since.
    pub left: &'a [(u8, f64)],
    /// Plan-following moves of all panes in the last hour.
    pub band_last_hour: usize,
    /// All panes' believed speeds, summed.
    pub demand: f64,
    /// The feed is over 15 minutes old.
    pub stale_feed: bool,
    /// The pane's speed was measured, not assumed.
    pub speed_known: bool,
    /// Marked as needing the restricted model in the last day, or
    /// using it through subagents: kept off accounts where it is spent.
    pub keeps_restricted: bool,
}

pub(crate) fn rest_move(accounts: &[Quant], plan: &Plan, p: &Resting) -> Option<RestMove> {
    let hi = accounts.iter().position(|q| q.id == p.here)?;
    let class = p.mover.class;
    let mut others = p.loads.to_vec();
    others[hi].add(class, -p.mover.rate);
    if lasts(&accounts[hi], &p.loads[hi], class, 0.0) < LEAVE_BELOW_H {
        if p.keeper {
            return None;
        }
        // a resting move never targets a stale reading, nor sends a pane
        // that uses the restricted model where that model is spent
        let allow = |q: &Quant| !q.stale && !(p.keeps_restricted && class == Class::Plain && q.phi() <= EPS);
        let to = pick_where(accounts, plan, &others, p.mover, Some(p.here), MUST_LAST_H, allow)?;
        let ti = accounts.iter().position(|q| q.id == to)?;
        return (lasts(&accounts[ti], &others[ti], class, p.mover.rate) >= MUST_LAST_H).then_some(RestMove::LeaveEarly(to));
    }
    band_move(accounts, plan, p, hi, &others).map(RestMove::Band)
}

fn band_move(accounts: &[Quant], plan: &Plan, p: &Resting, hi: usize, others: &[Load]) -> Option<u8> {
    let class = p.mover.class;
    if p.since_move_h.is_some_and(|h| h < BAND_COOLDOWN_H) || p.band_last_hour >= BAND_PER_HOUR || p.stale_feed {
        return None;
    }
    if shortage(accounts, p.demand) {
        return None;
    }
    let target = |i: usize| match class {
        Class::Restricted => plan.t_f[i],
        Class::Plain => plan.t_n[i],
    };
    if p.loads[hi].of(class) - target(hi) < p.mover.rate {
        return None;
    }
    let worth = if p.speed_known { BAND_WORTH } else { 2.0 * BAND_WORTH };
    let mut best: Option<usize> = None;
    for (i, q) in accounts.iter().enumerate() {
        let returning = p.left.iter().any(|(id, h)| *id == q.id && *h < NO_RETURN_H);
        if i == hi || !q.accepts(class) || q.stale || q.t < 1.0 || returning {
            continue;
        }
        if p.keeps_restricted && class == Class::Plain && q.phi() <= EPS {
            continue;
        }
        let d = target(i) - p.loads[i].of(class);
        if d < p.mover.rate || lasts(q, &others[i], class, p.mover.rate) < MUST_LAST_H {
            continue;
        }
        if d.min(p.mover.rate) * q.t < worth {
            continue;
        }
        if best.is_none_or(|b| q.renews_in(class) < accounts[b].renews_in(class)) {
            best = Some(i);
        }
    }
    best.map(|i| accounts[i].id)
}

/// Is pane `me` the one that stays on its account when it runs low:
/// the slowest there, ties to the lower number.
pub(crate) fn is_keeper(me: (u64, f64), on_same_account: impl Iterator<Item = (u64, f64)>) -> bool {
    on_same_account.filter(|(sid, _)| *sid != me.0).all(|(sid, rate)| (me.1, me.0) < (rate, sid))
}

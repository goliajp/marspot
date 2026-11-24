//! Moving a resting pane without being refused: rule (b).
//!
//! Four reasons.
//!
//! Leave before it runs out: the pane's account will run out within two
//! hours and before it renews, so the pane goes to one that lasts,
//! unless it is the slowest pane of its class there (one pane stays and
//! uses up the rest, or that rest lapses at the renewal; the slowest
//! costs least when it is refused).  No cooldown or count limits this
//! one.
//!
//! Leave what is the restricted model's (`UOut`), checked first: a
//! plain pane leaves when what the restricted model will not take
//! there, `U`, runs out that soon while the restricted model still has
//! some there, keeper or not: past that point it eats what the
//! restricted panes need.  When nothing lasts it and the restricted
//! model is short it goes where the restricted model has nothing left,
//! to be refused there and wait.  `U` follows the restricted demand,
//! which moves from scan to scan, so this has the plan-following
//! cooldown and hourly cap.
//!
//! Follow the plan: only when supply exceeds demand, a pane on an
//! account a whole pane over its target goes to one a whole pane short,
//! if what it would use there before that account renews is worth the
//! move.
//!
//! Save what would lapse (`rescue`): in a shortage, a restricted pane
//! goes where the restricted window would lapse unused.
//!
//! Every reason but leaving early stops once the pane has made six
//! resting moves in the last week.

use super::choose::{MUST_LAST_H, Mover, Reserve, pick_where};
use super::plan::Plan;
use super::rescue::rescue;
use super::{Class, EPS, Load, Quant, lasts, plain_lasts, shortage};

/// An account that will not last this long sends its resting panes away.
pub(crate) const LEAVE_BELOW_H: f64 = 2.0;

/// How long a plain pane's new account has to last it when it leaves
/// early: longer than a must-move's six hours, so a pane is not sent on
/// again by the next account running low.
pub(crate) const LEAVE_TO_H: f64 = 9.0;

/// No plan-following move this soon after any move of the same pane.
pub(crate) const BAND_COOLDOWN_H: f64 = 2.0;

/// No plan-following move back to an account left this recently.
pub(crate) const NO_RETURN_H: f64 = 6.0;

/// Resting moves one pane makes in a week before it makes no more
/// that have limits (all but leaving early).
pub(crate) const MOVES_PER_WEEK: usize = 6;

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
    /// A plain pane: what the restricted model will not take on its
    /// account will not last two hours.
    UOut(u8),
    /// Supply exceeds demand and the plan wants it elsewhere.
    Band(u8),
    /// Demand exceeds supply and the restricted window there would
    /// lapse unused.
    Rescue(u8),
}

/// One resting pane and what limits moving it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Resting<'a> {
    pub here: u8,
    pub mover: Mover,
    /// Every pane's load per account, this one included.
    pub loads: &'a [Load],
    /// It is the slowest pane of its class on its account (ties by
    /// pane number).
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
    /// Resting moves of this pane in the last week.
    pub moves_week: usize,
}

pub(crate) fn rest_move(accounts: &[Quant], plan: &Plan, p: &Resting) -> Option<RestMove> {
    let hi = accounts.iter().position(|q| q.id == p.here)?;
    let class = p.mover.class;
    let mut others = p.loads.to_vec();
    others[hi].add(class, -p.mover.rate);
    let q = &accounts[hi];
    let limited = p.moves_week >= MOVES_PER_WEEK;
    // what is left once U is gone is the restricted panes', keeper or not
    let u_out = class == Class::Plain && q.phi() > EPS && plain_lasts(plan.u[hi], &p.loads[hi], 0.0) < LEAVE_BELOW_H.min(q.t);
    if u_out && !limited && !p.since_move_h.is_some_and(|h| h < BAND_COOLDOWN_H) && p.band_last_hour < BAND_PER_HOUR {
        if let Some(to) = leave(accounts, plan, p, &others).or_else(|| set_aside(accounts, plan, p, hi)) {
            return Some(RestMove::UOut(to));
        }
    }
    if runs_out_soon(q, &p.loads[hi], class) {
        // the slowest stays for the rest of the account
        if p.keeper {
            return None;
        }
        return leave(accounts, plan, p, &others).map(RestMove::LeaveEarly);
    }
    if u_out || limited {
        return None;
    }
    if shortage(accounts, p.demand) {
        return if class == Class::Restricted { rescue(accounts, p, hi, &others).map(RestMove::Rescue) } else { None };
    }
    band_move(accounts, plan, p, hi, &others).map(RestMove::Band)
}

/// The account runs out within two hours, judged against the renewal of
/// whatever runs out: running out after it renews is not running out.
fn runs_out_soon(q: &Quant, load: &Load, class: Class) -> bool {
    let total = load.total();
    let week = total > 0.0 && q.r / total < LEAVE_BELOW_H.min(q.t);
    let restricted = class == Class::Restricted && load.restricted > 0.0 && q.f / load.restricted < LEAVE_BELOW_H.min(q.tf);
    week || restricted
}

/// Where a pane leaving early goes, if anywhere lasts it long enough: a
/// plain pane counts both `R` and `U`, so it does not land where `U` is
/// gone and leave again.
fn leave(accounts: &[Quant], plan: &Plan, p: &Resting, others: &[Load]) -> Option<u8> {
    let class = p.mover.class;
    let horizon = if class == Class::Plain { LEAVE_TO_H } else { MUST_LAST_H };
    // a resting move never targets a stale reading, nor sends a pane
    // that uses the restricted model where that model is spent
    let allow = |q: &Quant| !q.stale && !(p.keeps_restricted && class == Class::Plain && q.phi() <= EPS);
    let to = pick_where(accounts, plan, others, p.mover, Some(p.here), horizon, Reserve::U, allow)?;
    let ti = accounts.iter().position(|q| q.id == to)?;
    let mut there = lasts(&accounts[ti], &others[ti], class, p.mover.rate);
    if class == Class::Plain {
        there = there.min(plain_lasts(plan.u[ti], &others[ti], p.mover.rate));
    }
    (there >= horizon).then_some(to)
}

/// Where a plain pane whose `U` is gone goes when nothing lasts it nine
/// hours and the restricted model is short: the account with the most
/// week left that the restricted model has nothing of, however little.
/// This is a planned wait: once that is spent the pane is refused and
/// waits (`Reserve::N`) instead of using what the restricted panes are
/// short of.  A target that has to last it longer keeps it at home,
/// eating that.
fn set_aside(accounts: &[Quant], plan: &Plan, p: &Resting, hi: usize) -> Option<u8> {
    if plan.unassigned_f <= EPS || p.keeps_restricted {
        return None;
    }
    let ok = |i: usize, q: &Quant| i != hi && q.accepts(Class::Plain) && !q.stale && q.phi() <= EPS;
    let best = accounts.iter().enumerate().filter(|(i, q)| ok(*i, q)).max_by(|(_, a), (_, b)| a.r.total_cmp(&b.r).then(b.id.cmp(&a.id)));
    best.map(|(_, q)| q.id)
}

fn band_move(accounts: &[Quant], plan: &Plan, p: &Resting, hi: usize, others: &[Load]) -> Option<u8> {
    let class = p.mover.class;
    if p.since_move_h.is_some_and(|h| h < BAND_COOLDOWN_H) || p.band_last_hour >= BAND_PER_HOUR || p.stale_feed {
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
        if d.min(p.mover.rate) * q.renews_in(class).min(q.t) < worth {
            continue;
        }
        if best.is_none_or(|b| q.renews_in(class) < accounts[b].renews_in(class)) {
            best = Some(i);
        }
    }
    best.map(|i| accounts[i].id)
}

/// Is pane `me` the one that stays on its account when it runs low:
/// the slowest of its class there, ties to the lower number.  The
/// caller passes the other panes of the same class only.
pub(crate) fn is_keeper(me: (u64, f64), on_same_account: impl Iterator<Item = (u64, f64)>) -> bool {
    on_same_account.filter(|(sid, _)| *sid != me.0).all(|(sid, rate)| (me.1, me.0) < (rate, sid))
}

//! The rules the simulator compares.
//!
//! `Old` is today's ordering (`best_profile` and `better_home`) as
//! closely as a few lines can say it: an account at 1% or less counts
//! as nearly out; plain panes prefer accounts whose Fable is spent,
//! then the earliest week renewal, then the least left; a resting pane
//! moves at most every 30 minutes, one per pass, when its account is
//! nearly out, when that leaves Fable to Fable panes, or when the
//! other week renews six hours sooner.  `Plan` is this core: `pick`
//! for every forced move and `rest_move` for resting panes.

use super::choose::{MUST_LAST_H, Mover, loads_of, pick as core_pick};
use super::plan::{Plan, plan};
use super::rest::{Resting, RestMove, is_keeper, rest_move};
use super::sim::SimPane;
use super::{Class, EPS, Quant, lasts};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Policy {
    Old,
    Plan,
}

fn room(q: &Quant, class: Class) -> f64 {
    match class {
        Class::Restricted => q.phi(),
        Class::Plain => q.r,
    }
}

fn table(panes: &[SimPane], t: f64) -> Vec<(Option<u8>, Class, f64)> {
    panes.iter().map(|p| if p.live(t) { (p.on, p.class, p.est) } else { (None, p.class, 0.0) }).collect()
}

pub(super) fn targets(policy: Policy, a: &[Quant], panes: &[SimPane], t: f64, kappa: f64) -> Option<Plan> {
    if policy == Policy::Old {
        return None;
    }
    let live = || panes.iter().filter(|p| p.live(t));
    let b_f = live().filter(|p| p.class == Class::Restricted).map(|p| p.est).sum();
    let b_n = live().filter(|p| p.class == Class::Plain).map(|p| p.est).sum();
    Some(plan(a, b_f, b_n, Some(kappa)))
}

pub(super) fn pick(policy: Policy, a: &[Quant], panes: &[SimPane], i: usize, from: Option<u8>, t: f64, kappa: f64) -> Option<u8> {
    let class = panes[i].class;
    match policy {
        Policy::Old => old_pick(a, class, from),
        Policy::Plan => {
            let p = targets(policy, a, panes, t, kappa)?;
            let loads = loads_of(a, &table(panes, t), Some(i));
            core_pick(a, &p, &loads, Mover { class, rate: panes[i].est }, from, MUST_LAST_H)
        }
    }
}

pub(super) fn old_pick(a: &[Quant], class: Class, from: Option<u8>) -> Option<u8> {
    let key = |q: &Quant| {
        let low = room(q, class) <= 0.01;
        let keeps = class == Class::Plain && q.f <= EPS;
        (low, !keeps, q.t, room(q, class), q.id)
    };
    a.iter()
        .filter(|q| Some(q.id) != from && q.accepts(class))
        .min_by(|x, y| key(x).partial_cmp(&key(y)).unwrap_or(std::cmp::Ordering::Equal))
        .map(|q| q.id)
}

/// Today's helper rule: the most left per helper already there.
pub(super) fn old_helper_pick(a: &[Quant], class: Class, placed: &[(u8, usize)]) -> Option<u8> {
    let per = |q: &Quant| room(q, class) / (placed.iter().find(|(id, _)| *id == q.id).map_or(0, |(_, n)| *n) + 1) as f64;
    a.iter().filter(|q| q.accepts(class)).max_by(|x, y| per(x).total_cmp(&per(y)).then(y.id.cmp(&x.id))).map(|q| q.id)
}

/// Today's resting move of a pane of `class` on `here`, `since_h`
/// after its last move.
pub(super) fn old_rest(a: &[Quant], class: Class, here: u8, since_h: Option<f64>) -> Option<u8> {
    if since_h.is_some_and(|h| h < 0.5) {
        return None;
    }
    let h = a.iter().find(|q| q.id == here)?;
    let y = old_pick(a, class, Some(here))?;
    let to = a.iter().find(|q| q.id == y)?;
    if room(to, class) <= 0.01 {
        return None;
    }
    if room(h, class) <= 0.01 {
        return Some(y);
    }
    let plain = class == Class::Plain;
    if plain && h.f > EPS && to.f <= EPS {
        return Some(y);
    }
    if plain && h.f <= EPS && to.f > EPS {
        return None;
    }
    (h.t - to.t >= 6.0).then_some(y)
}

/// A resting move and whether it was urgent (exempt from the hourly cap).
pub(super) fn rest(policy: Policy, a: &[Quant], panes: &[SimPane], i: usize, t: f64, plan: Option<&Plan>, hour: usize) -> Option<(u8, bool)> {
    let me = &panes[i];
    let here = me.on?;
    let since_h = me.last.map(|l| t - l).filter(|h| h.is_finite());
    let hi = a.iter().position(|q| q.id == here)?;
    let tab = table(panes, t);
    let loads = loads_of(a, &tab, None);
    let urgent = match policy {
        Policy::Old => room(&a[hi], me.class) <= 0.01,
        Policy::Plan => lasts(&a[hi], &loads[hi], me.class, 0.0) < super::rest::LEAVE_BELOW_H,
    };
    if !urgent && hour >= super::rest::BAND_PER_HOUR {
        return None;
    }
    if policy == Policy::Old {
        return old_rest(a, me.class, here, since_h).map(|y| (y, urgent));
    }
    let same = tab.iter().enumerate().filter(|(j, (on, _, _))| *j != i && *on == Some(here)).map(|(j, (_, _, r))| (j as u64, *r));
    let left: Vec<(u8, f64)> = me.left.iter().map(|(id, at)| (*id, t - at)).collect();
    let p = Resting {
        here,
        mover: Mover { class: me.class, rate: me.est },
        loads: &loads,
        keeper: is_keeper((i as u64, me.est), same),
        since_move_h: since_h,
        left: &left,
        band_last_hour: hour,
        demand: tab.iter().map(|(_, _, r)| r).sum(),
        stale_feed: false,
        speed_known: true,
        keeps_restricted: false,
    };
    match rest_move(a, plan?, &p)? {
        RestMove::LeaveEarly(y) => Some((y, true)),
        RestMove::Band(y) => Some((y, false)),
    }
}

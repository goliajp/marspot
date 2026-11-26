//! An account decision about one pane, as the selector sees it.
//!
//! The candidates are the accounts the pane could go to, and staying
//! where it is.  What the objective in force (`quota::waste`) forbids
//! becomes a hard constraint: the weekly, daily and hourly move limits,
//! the cooldown, a stale feed, the slowest pane that keeps an account's
//! rest, accounts left less than six hours ago, accounts where the
//! restricted model is spent for a pane that uses it, stale readings
//! while a fresh one takes the pane, accounts that would refuse it
//! while one would not, and a resting move that does not save more
//! than the margin.  Money is the objective's cost of each candidate,
//! in points: the quota lapsing at the renewals with the pane there,
//! plus the prompt cache a move rewrites.  Efficiency is the stall a
//! resting move causes.  The candidates are listed in the objective's
//! own tie order (earlier renewal for the pane's class, then the
//! account that lasts it longest, then the number), so a tie the
//! selector breaks by order is broken as the objective breaks it.
//!
//! A pane that has to move, or a new one, has no "stay": it goes
//! wherever an account takes it.

use super::choose::{Choice, Policy, Terms, choose};
use crate::quota::choose::MUST_LAST_H;
use crate::quota::replay::{Inputs, Kind, Rule};
use crate::quota::rest::{BAND_COOLDOWN_H, BAND_PER_HOUR, MOVES_PER_WEEK, NO_RETURN_H};
use crate::quota::waste::{Params, added, keeps_the_rest, switch_cost};
use crate::quota::{Class, EPS, Load, Quant, lasts_of};

/// Points in a week of quota.
pub const POINTS: f64 = 100.0;

/// A resting move's stall: the agent restarts and the person sees an
/// account changed when they come back.
pub const K_MOVE_H: f64 = 0.05;

/// A refusal's stall when nothing has measured it.
pub const STALL_H: f64 = 0.25;

/// The candidates of a decision and doing nothing, when allowed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cands {
    /// Target account id and terms, in tie order.
    pub to: Vec<(u8, Terms)>,
    pub stay: Option<Terms>,
}

/// The candidates of `i`'s decision under `rule`'s weights; `allow`
/// is what the caller lets through besides (a badge's accounts, say).
pub fn cands(i: &Inputs, rule: Rule, allow: &dyn Fn(&Quant) -> bool) -> Cands {
    let p = rule.params();
    match &i.kind {
        Kind::Out { horizon_h } => placed(i, Some(i.here), *horizon_h, &p, allow),
        Kind::New { horizon_h } => placed(i, None, *horizon_h, &p, allow),
        Kind::Rest { .. } => resting(i, rule, &p, allow),
    }
}

/// The selector's answer for `i`, with `week_eff` points spent for
/// efficiency this week.
pub fn decide(i: &Inputs, rule: Rule, allow: &dyn Fn(&Quant) -> bool, week_eff: f64, p: &Policy) -> Choice<u8> {
    let c = cands(i, rule, allow);
    choose(&c.to, c.stay, week_eff, p)
}

fn tie_order(accounts: &[Quant], loads: &[Load], i: &Inputs, to: &mut [(usize, Terms)]) {
    let m = i.mover;
    to.sort_by(|(a, _), (b, _)| {
        let (qa, qb) = (&accounts[*a], &accounts[*b]);
        let last = |j: usize| lasts_of(&accounts[j], &loads[j], m);
        qa.renews_in(m.class).total_cmp(&qb.renews_in(m.class)).then(last(*b).total_cmp(&last(*a))).then(qa.id.cmp(&qb.id))
    });
}

fn placed(i: &Inputs, from: Option<u8>, horizon_h: f64, p: &Params, allow: &dyn Fn(&Quant) -> bool) -> Cands {
    let m = i.mover;
    let mut all: Vec<(usize, f64, bool, bool)> = Vec::new();
    for (j, q) in i.accounts.iter().enumerate() {
        if Some(q.id) == from || !q.accepts(m.class) || !allow(q) {
            continue;
        }
        let a = added(q, &i.loads[j], m, i.kappa, horizon_h, p);
        all.push((j, a.cost, a.refuses, q.stale));
    }
    // fresh before stale, then accounts that keep the pane over the horizon
    if all.iter().any(|x| !x.3) {
        all.retain(|x| !x.3);
    }
    if all.iter().any(|x| !x.2) {
        all.retain(|x| !x.2);
    }
    // `added`'s cost still prices a refusal's stall (`waste::STALL`) as money until the selector acts
    let mut to: Vec<(usize, Terms)> = all.iter().map(|&(j, cost, refuses, _)| (j, Terms { money: cost * POINTS, efficiency: if refuses { STALL_H } else { 0.0 }, fairness: 0.0 })).collect();
    tie_order(&i.accounts, &i.loads, i, &mut to);
    Cands { to: to.into_iter().map(|(j, t)| (i.accounts[j].id, t)).collect(), stay: None }
}

fn resting(i: &Inputs, rule: Rule, p: &Params, allow: &dyn Fn(&Quant) -> bool) -> Cands {
    let Kind::Rest { keeper, since_move_h, left, band_last_hour, stale_feed, speed_known, keeps_restricted, moves_week, .. } = &i.kind else {
        return Cands::default();
    };
    let m = i.mover;
    let moves_day = match rule {
        Rule::Waste { moves_day, .. } => moves_day,
        Rule::Bands => 0,
    };
    let Some(hi) = i.accounts.iter().position(|q| q.id == i.here) else { return Cands::default() };
    let mut others = i.loads.to_vec();
    others[hi].add_mover(m, -1.0);
    let stays = added(&i.accounts[hi], &others[hi], m, i.kappa, MUST_LAST_H, p).cost;
    let stay = Some(Terms { money: stays * POINTS, efficiency: 0.0, fairness: 0.0 });
    let limited = *stale_feed || *moves_week >= MOVES_PER_WEEK || moves_day >= p.per_day || *band_last_hour >= BAND_PER_HOUR;
    let cooling = since_move_h.is_some_and(|h| h < BAND_COOLDOWN_H);
    if limited || cooling || (*keeper && keeps_the_rest(&i.accounts[hi], &i.loads[hi], m.class, i.kappa)) {
        return Cands { to: Vec::new(), stay };
    }
    let returning = |id: u8| left.iter().any(|(l, h)| *l == id && *h < NO_RETURN_H);
    let margin = if *speed_known { p.margin } else { 2.0 * p.margin };
    let rewrite = p.lambda * switch_cost(m.class, m.tokens);
    let mut to: Vec<(usize, Terms)> = Vec::new();
    for (j, q) in i.accounts.iter().enumerate() {
        let spent_for_it = *keeps_restricted && m.class == Class::Plain && q.phi() <= EPS;
        if j == hi || !q.accepts(m.class) || q.stale || returning(q.id) || spent_for_it || !allow(q) {
            continue;
        }
        let a = added(q, &others[j], m, i.kappa, MUST_LAST_H, p);
        if a.refuses || stays - a.cost - rewrite <= margin {
            continue;
        }
        to.push((j, Terms { money: (a.cost + rewrite) * POINTS, efficiency: K_MOVE_H, fairness: 0.0 }));
    }
    tie_order(&i.accounts, &others, i, &mut to);
    Cands { to: to.into_iter().map(|(j, t)| (i.accounts[j].id, t)).collect(), stay }
}

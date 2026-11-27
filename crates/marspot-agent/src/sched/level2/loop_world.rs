//! A closed loop: simulated weeks in five-minute steps, demand drawn
//! hour by hour from the recorded fleet hours, panes resting at random
//! at their measured rates, every account renewing on its own clock.
//!
//! Two policies on the same draws:
//! - `Today`: at a rest the objective in force (`waste::decide`) may
//!   move the pane; a refused pane goes where it says.
//! - `Planner`: a full search on the hour and after a refusal over the
//!   standing plan (inertia, commitments and pins as in the shell), from
//!   each pane's measured speed (blind to the draws ahead) times `bias`;
//!   at a rest a standing switch whose window is open is taken; a
//!   refused pane goes where the objective says, as today.
//!
//! Money realised: every rewrite paid (switches and refusals), plus the
//! week lapsing at a renewal while demand went unmet in that week.

use super::standing::Standing;
use super::tests::KAPPA;
use super::*;
use crate::quota::choose::{MUST_LAST_H, Mover};
use crate::quota::replay::Kind as Decision;
use crate::quota::rest::is_keeper;
use crate::quota::speed::PaneUse;
use crate::quota::waste::{self, PARAMS, switch_cost};
use crate::quota::{Class, Load, Quant};
use crate::sched::level1::{self, World};

const FIXTURE: &str = include_str!("../../../tests/fixtures/forecast_facts_20261126.tsv");
const STEP: i64 = 300;
const WEEK: i64 = 7 * 24 * 3600;
/// Transcript rests of a minute or more per busy hour, the eight
/// largest panes of 2026-11-24 to 11-26.
const RESTS: [f64; 8] = [19.7, 14.3, 11.2, 9.3, 6.6, 3.9, 2.4, 1.9];
/// Each pane's share of the fleet's plain and restricted use.
const SHARE: [f64; 8] = [0.2, 0.17, 0.15, 0.13, 0.11, 0.1, 0.08, 0.06];

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Policy {
    Today,
    Planner { bias: f64 },
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Week {
    pub money: f64,
    pub unmet: f64,
    pub refusals: usize,
    pub switches: usize,
    /// A pane switched twice by plan inside six hours.
    pub double: usize,
}

/// The fleet hours recorded, `[restricted, plain]` weeks an hour.
pub(super) fn hours() -> Vec<[f64; 2]> {
    FIXTURE.lines().filter_map(|l| l.strip_prefix("h\t")).filter_map(|l| {
        let f: Vec<f64> = l.split('\t').filter_map(|x| x.parse().ok()).collect();
        (f.len() == 3).then(|| [f[1], f[2]])
    }).collect()
}

struct P {
    on: u8,
    restricted: bool,
    used: PaneUse,
    stalled: bool,
    planned: Vec<i64>,
    moved_at: Option<i64>,
    left: Option<(u8, i64)>,
}

pub(super) fn run(seed: u64, policy: Policy) -> Week {
    let mut rng = super::oracle_tests::Rng(seed);
    let hours = hours();
    let mut acc: Vec<(Quant, i64)> = (0..8u8)
        .map(|k| {
            let r = 0.3 + 0.7 * rng.next();
            let renew = (5 + 21 * i64::from(k)) * 3600;
            (Quant { id: k + 1, r, f: KAPPA * (0.2 + 0.8 * rng.next()), t: 0.0, tf: 0.0, open: true, stale: false }, renew)
        })
        .collect();
    let mut panes: Vec<P> = (0..8).map(|k| P { on: 1 + (k % 8) as u8, restricted: k < 2, used: PaneUse::default(), stalled: false, planned: Vec::new(), moved_at: None, left: None }).collect();
    for p in &mut panes {
        p.used.watched(-6 * 3600);
    }
    let mut w = Week::default();
    let mut st = Standing::default();
    let mut planned_once = false;
    let mut unmet_week = false;
    let mut demand = [0.0; 2];
    for step in 0..(WEEK / STEP) {
        let now = step * STEP;
        if now % 3600 == 0 {
            demand = hours[rng.below(hours.len())];
        }
        for (q, renew) in &mut acc {
            q.t = (*renew - now) as f64 / 3600.0;
            q.tf = q.t;
            if now >= *renew {
                if unmet_week {
                    w.money += q.r.max(0.0);
                }
                q.r = 1.0;
                q.f = KAPPA;
                *renew += WEEK;
            }
        }
        if let Policy::Planner { bias } = policy
            && (now % 3600 == 0 || !planned_once)
        {
            planned(&mut st, &acc, &panes, now, bias);
            planned_once = true;
        }
        for (k, rate) in RESTS.iter().enumerate().take(panes.len()) {
            if rng.next() < 1.0 - (-rate / 12.0).exp() {
                rest(&mut acc, &mut panes, k, now, policy, &st, &mut w);
            }
        }
        let mut refused = false;
        for (k, p) in panes.iter_mut().enumerate() {
            let class = if p.restricted { 0 } else { 1 };
            // the restricted use is the two restricted panes', by their shares
            let a = if p.restricted { demand[0] * SHARE[k] / (SHARE[0] + SHARE[1]) } else { 0.0 };
            let b = demand[1] * SHARE[k];
            let (a, b) = (a * STEP as f64 / 3600.0, b * STEP as f64 / 3600.0);
            let (q, _) = acc.iter_mut().find(|x| x.0.id == p.on).expect("its account");
            let served_a = a.min(q.f.max(0.0)).min(q.r.max(0.0));
            q.r -= served_a;
            q.f -= served_a;
            let served_b = b.min(q.r.max(0.0));
            q.r -= served_b;
            p.used.add(now, Class::Restricted, served_a);
            p.used.add(now, Class::Plain, served_b);
            let short = (if class == 0 { a - served_a } else { 0.0 }) + b - served_b;
            if short > 1e-12 {
                w.unmet += short;
                unmet_week = true;
                if !p.stalled {
                    p.stalled = true;
                    w.refusals += 1;
                    refused = true;
                }
            } else {
                p.stalled = false;
            }
        }
        for k in 0..panes.len() {
            if panes[k].stalled {
                refuse(&acc, &mut panes, k, now, &mut w);
            }
        }
        if refused && let Policy::Planner { bias } = policy {
            planned(&mut st, &acc, &panes, now, bias);
        }
    }
    w
}

fn quants(acc: &[(Quant, i64)]) -> Vec<Quant> {
    acc.iter().map(|x| x.0.clone()).collect()
}

fn speeds(p: &P, now: i64) -> (f64, f64) {
    let (f, n, _) = p.used.rate_or_default(now, if p.restricted { Class::Restricted } else { Class::Plain });
    (f, n)
}

fn loads(acc: &[(Quant, i64)], panes: &[P], now: i64, skip: Option<usize>) -> Vec<Load> {
    let mut l = vec![Load::default(); acc.len()];
    for (_, p) in panes.iter().enumerate().filter(|(k, _)| Some(*k) != skip) {
        let (f, n) = speeds(p, now);
        if let Some(j) = acc.iter().position(|x| x.0.id == p.on) {
            l[j].restricted += f;
            l[j].plain += n;
        }
    }
    l
}

fn mover(p: &P, now: i64) -> Mover {
    let (f, n) = speeds(p, now);
    Mover { class: if p.restricted { Class::Restricted } else { Class::Plain }, rate: f + n, tokens: Some(400_000), fable: Some(f) }
}

fn switch(panes: &mut [P], k: usize, to: u8, now: i64, w: &mut Week) {
    let p = &mut panes[k];
    w.switches += 1;
    w.money += switch_cost(if p.restricted { Class::Restricted } else { Class::Plain }, Some(400_000));
    p.left = Some((p.on, now));
    p.on = to;
    p.moved_at = Some(now);
    p.stalled = false;
}

fn refuse(acc: &[(Quant, i64)], panes: &mut [P], k: usize, now: i64, w: &mut Week) {
    let q = quants(acc);
    let l = loads(acc, panes, now, Some(k));
    let m = mover(&panes[k], now);
    if let Some((to, _)) = waste::decide(&q, &l, m, panes[k].on, Some(KAPPA), &Decision::Out { horizon_h: MUST_LAST_H }, 0, &PARAMS) {
        w.money += switch_cost(m.class, m.tokens);
        let p = &mut panes[k];
        p.left = Some((p.on, now));
        p.on = to;
        p.moved_at = Some(now);
        p.stalled = false;
    }
}

fn rest(acc: &mut [(Quant, i64)], panes: &mut [P], k: usize, now: i64, policy: Policy, st: &Standing, w: &mut Week) {
    match policy {
        Policy::Today => {
            let q = quants(acc);
            let l = loads(acc, panes, now, None);
            let p = &panes[k];
            let me = (k as u64, mover(p, now).rate);
            let same = panes.iter().enumerate().filter(|(j, y)| *j != k && y.on == p.on && y.restricted == p.restricted).map(|(j, y)| (j as u64, mover(y, now).rate));
            let kind = Decision::Rest {
                keeper: is_keeper(me, same),
                since_move_h: p.moved_at.map(|at| (now - at) as f64 / 3600.0),
                left: p.left.map(|(id, at)| (id, (now - at) as f64 / 3600.0)).into_iter().collect(),
                band_last_hour: 0,
                demand: 0.0,
                stale_feed: false,
                speed_known: true,
                keeps_restricted: false,
                moves_week: p.planned.iter().filter(|at| now - **at < WEEK).count(),
            };
            let day = panes.iter().flat_map(|x| x.planned.iter()).filter(|at| now - **at < 86_400).count();
            if let Some((to, _)) = waste::decide(&q, &l, mover(p, now), p.on, Some(KAPPA), &kind, day, &PARAMS) {
                panes[k].planned.push(now);
                switch(panes, k, to, now, w);
            }
        }
        Policy::Planner { .. } => {
            let sid = k as u64;
            let Some(s) = st.switches.iter().find(|s| s.sid == sid && s.order == 0 && s.from == panes[k].on && s.open_at <= now) else { return };
            if panes[k].planned.last().is_some_and(|t| now - t < 6 * 3600) {
                w.double += 1;
            }
            panes[k].planned.push(now);
            switch(panes, k, s.to, now, w);
        }
    }
}

fn planned(st: &mut Standing, acc: &[(Quant, i64)], panes: &[P], now: i64, bias: f64) {
    let q = quants(acc);
    let sp: Vec<(f64, f64)> = panes.iter().map(|p| speeds(p, now)).map(|(f, n)| (f * bias, n * bias)).collect();
    let demand = [sp.iter().map(|x| x.0).sum(), sp.iter().map(|x| x.1).sum()];
    let l1 = level1::plan(&World { accounts: &q, window_full: KAPPA, demand, prudent: demand[0] * 2.5, buckets: &[], offers: &[], closures: &[], last_pi: None });
    let accounts: Vec<Account> = q.iter().enumerate().map(|(j, x)| Account { q: x.clone(), spare: l1.spare[j], price: l1.price[j].unwrap_or([0.0; 2]) }).collect();
    let ps: Vec<Pane> = panes
        .iter()
        .enumerate()
        .map(|(k, p)| {
            let since = p.planned.last().map_or(f64::INFINITY, |t| (now - t) as f64 / 3600.0);
            Pane {
                sid: k as u64,
                on: p.on,
                restricted: p.restricted,
                speed: [sp[k].0, sp[k].1],
                high: [sp[k].0 * 2.5, sp[k].1 * 1.5],
                rewrite: switch_cost(if p.restricted { Class::Restricted } else { Class::Plain }, Some(400_000)),
                lost_work: 0.0,
                clean_per_h: RESTS[k],
                long_background: false,
                earliest_h: (DWELL_H - since).max(0.0),
                left: p.left.map(|(id, at)| (id, 6.0 - (now - at) as f64 / 3600.0)),
                barred: 0,
                switches_left: (6usize.saturating_sub(p.planned.iter().filter(|t| now - **t < WEEK).count())) as u8,
                disturb_left: 4,
            }
        })
        .collect();
    let day = panes.iter().flat_map(|x| x.planned.iter()).filter(|at| now - **at < 86_400).count();
    let unserved = l1.unserved_until(0, H2) + l1.unserved_until(1, H2) > 1e-9;
    let i = Input { accounts: &accounts, panes: &ps, young: [0.0; 2], window_full: KAPPA, pi: l1.pi, short: unserved, fleet_left: FLEET_PER_DAY.saturating_sub(day), notices_left: NOTICES_PER_HOUR, stale_feed: false };
    pass::full(st, &i, now as u64, now);
}

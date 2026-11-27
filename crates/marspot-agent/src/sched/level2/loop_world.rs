//! A closed loop: simulated weeks in five-minute steps, demand drawn
//! hour by hour from the recorded fleet hours, panes resting at random
//! at their measured rates, every account renewing on its own clock.
//!
//! Two policies on the same draws:
//! - `Today`: at a rest the objective in force (`waste::decide`) may
//!   move the pane; a refused pane goes where it says.  Its speeds may be
//!   given wrong by the same factor as the planner's.
//! - `Planner`: a full search on the hour and after a refusal over the
//!   standing plan (inertia, commitments and pins as in the shell), from
//!   each pane's measured speed (blind to the draws ahead) times `bias`;
//!   or driven as the shell drives it: at every reading a full search
//!   when one of the shell's triggers is due, else the standing plan
//!   re-timed.  At a rest a standing switch whose window is open is
//!   taken; a refused pane goes where the objective says, as today.
//!
//! Money realised: every rewrite paid (switches and refusals), plus the
//! week lapsing at a renewal while demand went unmet in the week before
//! it.  Each run is two weeks and only the second is counted: a single
//! week from a cold start leaves the quota still on the accounts at its
//! end uncounted, and a policy that lapses less at the renewals but
//! leaves more at the end looked dearer than one that did the reverse.
//! Level 1 reads the recorded restricted use in six-hour buckets and
//! their highest as the prudent rate, as the shell hands it.
//!
//! Both policies read each account's week and restricted window with a
//! reading error of up to one point either way, drawn afresh every
//! reading from a stream of its own, so both see the same readings; an
//! account with nothing left reads empty.  Now and then the feed goes
//! out and both read the accounts as the shell does then.

use super::loop_plan::{Driver, Feed, Read};
use super::oracle_tests::Rng;
use super::standing::Standing;
use super::tests::KAPPA;
use crate::quota::choose::{MUST_LAST_H, Mover};
use crate::quota::replay::Kind as Decision;
use crate::quota::rest::is_keeper;
use crate::quota::speed::PaneUse;
use crate::quota::waste::{self, PARAMS, switch_cost};
use crate::quota::{Class, Load, Quant};

const FIXTURE: &str = include_str!("../../../tests/fixtures/forecast_facts_20261126.tsv");
const STEP: i64 = 300;
pub(super) const WEEK: i64 = 7 * 24 * 3600;
/// Transcript rests of a minute or more per busy hour, the eight
/// largest panes of 2026-11-24 to 11-26.
pub(super) const RESTS: [f64; 8] = [19.7, 14.3, 11.2, 9.3, 6.6, 3.9, 2.4, 1.9];
/// Each pane's share of the fleet's plain and restricted use.
const SHARE: [f64; 8] = [0.2, 0.17, 0.15, 0.13, 0.11, 0.1, 0.08, 0.06];

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Policy {
    /// Today's rule, its speeds times `bias` at a rest.
    Today { bias: f64 },
    /// The planner, its level times `bias` and its three-hour speed
    /// times `short`: a full search on the hour and after a refusal, or
    /// (`shell`) as the shell drives it, at every reading a full search
    /// when a trigger is due and else a re-time.
    Planner { bias: f64, short: f64, shell: bool },
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Week {
    pub money: f64,
    pub unmet: f64,
    pub refusals: usize,
    pub switches: usize,
    /// A pane switched twice by plan inside six hours.
    pub double: usize,
    /// The money in parts: rewrites of switches, rewrites at refusals,
    /// week lapsing at renewals.
    pub switch_rw: f64,
    pub refusal_rw: f64,
    pub lapsed: f64,
    /// Switches and refusals of the restricted panes.
    pub switches_r: usize,
    pub refusals_r: usize,
}

/// The fleet hours recorded, `[restricted, plain]` weeks an hour.
pub(super) fn hours() -> Vec<[f64; 2]> {
    FIXTURE.lines().filter_map(|l| l.strip_prefix("h\t")).filter_map(|l| {
        let f: Vec<f64> = l.split('\t').filter_map(|x| x.parse().ok()).collect();
        (f.len() == 3).then(|| [f[1], f[2]])
    }).collect()
}

pub(super) struct P {
    pub on: u8,
    pub restricted: bool,
    used: PaneUse,
    stalled: bool,
    pub planned: Vec<i64>,
    moved_at: Option<i64>,
    pub left: Option<(u8, i64)>,
}

/// One point of reading error either way at most.
const READ_ERR: f64 = 0.01;

pub(super) fn run(seed: u64, policy: Policy) -> Week {
    let mut rng = Rng(seed);
    let mut noise = Rng(seed ^ 0x5DEE_CE66_D1CE_4E5B);
    let mut err = vec![[0.0; 2]; 8];
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
    let mut drv = Driver::default();
    let mut feed = Feed::new(seed);
    // the last moment demand went unmet; a renewal lapses as waste when
    // demand went unmet in the week before it
    let mut last_unmet: Option<i64> = None;
    let mut demand = [0.0; 2];
    for step in 0..(2 * WEEK / STEP) {
        let now = step * STEP;
        if now == WEEK {
            w = Week { double: w.double, ..Week::default() };
        }
        if now % 3600 == 0 {
            demand = hours[rng.below(hours.len())];
        }
        for (q, renew) in &mut acc {
            q.t = (*renew - now) as f64 / 3600.0;
            q.tf = q.t;
            if now >= *renew {
                let unmet_week = last_unmet.is_some_and(|t| t > now - WEEK);
                if unmet_week {
                    w.money += q.r.max(0.0);
                    w.lapsed += q.r.max(0.0);
                }
                q.r = 1.0;
                q.f = KAPPA;
                *renew += WEEK;
            }
        }
        for e in &mut err {
            *e = [READ_ERR * (2.0 * noise.next() - 1.0), READ_ERR * (2.0 * noise.next() - 1.0)];
        }
        feed.step(&acc, &err, &panes, now, STEP);
        drv.step(policy, &Read { acc: &acc, err: &err, feed: &feed, now }, &panes, now, false);
        for (k, rate) in RESTS.iter().enumerate().take(panes.len()) {
            if rng.next() < 1.0 - (-rate / 12.0).exp() {
                rest(&Read { acc: &acc, err: &err, feed: &feed, now }, &mut panes, k, now, policy, &drv.st, &mut w);
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
                last_unmet = Some(now);
                if !p.stalled {
                    p.stalled = true;
                    w.refusals += 1;
                    w.refusals_r += usize::from(p.restricted);
                    refused = true;
                }
            } else {
                p.stalled = false;
            }
        }
        for k in 0..panes.len() {
            if panes[k].stalled {
                refuse(&Read { acc: &acc, err: &err, feed: &feed, now }, &mut panes, k, now, &mut w);
            }
        }
        if refused {
            drv.step(policy, &Read { acc: &acc, err: &err, feed: &feed, now }, &panes, now, true);
        }
    }
    w
}

pub(super) fn speeds(p: &P, now: i64) -> (f64, f64) {
    let (f, n, _) = p.used.rate_or_default(now, if p.restricted { Class::Restricted } else { Class::Plain });
    (f, n)
}

fn loads(q: &[Quant], panes: &[P], now: i64, skip: Option<usize>, bias: f64) -> Vec<Load> {
    let mut l = vec![Load::default(); q.len()];
    for (_, p) in panes.iter().enumerate().filter(|(k, _)| Some(*k) != skip) {
        let (f, n) = speeds(p, now);
        let (f, n) = (f * bias, n * bias);
        if let Some(j) = q.iter().position(|x| x.id == p.on) {
            l[j].restricted += f;
            l[j].plain += n;
        }
    }
    l
}

fn mover(p: &P, now: i64, bias: f64) -> Mover {
    let (f, n) = speeds(p, now);
    let (f, n) = (f * bias, n * bias);
    Mover { class: if p.restricted { Class::Restricted } else { Class::Plain }, rate: f + n, tokens: Some(400_000), fable: Some(f) }
}

fn switch(panes: &mut [P], k: usize, to: u8, now: i64, w: &mut Week) {
    let p = &mut panes[k];
    w.switches += 1;
    w.switches_r += usize::from(p.restricted);
    let rw = switch_cost(if p.restricted { Class::Restricted } else { Class::Plain }, Some(400_000));
    w.money += rw;
    w.switch_rw += rw;
    p.left = Some((p.on, now));
    p.on = to;
    p.moved_at = Some(now);
    p.stalled = false;
}

fn refuse(read: &Read, panes: &mut [P], k: usize, now: i64, w: &mut Week) {
    let q = read.quants();
    let l = loads(&q, panes, now, Some(k), 1.0);
    let m = mover(&panes[k], now, 1.0);
    if let Some((to, _)) = waste::decide(&q, &l, m, panes[k].on, Some(KAPPA), &Decision::Out { horizon_h: MUST_LAST_H }, 0, &PARAMS) {
        w.money += switch_cost(m.class, m.tokens);
        w.refusal_rw += switch_cost(m.class, m.tokens);
        let p = &mut panes[k];
        p.left = Some((p.on, now));
        p.on = to;
        p.moved_at = Some(now);
        p.stalled = false;
    }
}

fn rest(read: &Read, panes: &mut [P], k: usize, now: i64, policy: Policy, st: &Standing, w: &mut Week) {
    match policy {
        Policy::Today { bias } => {
            let q = read.quants();
            let l = loads(&q, panes, now, None, bias);
            let p = &panes[k];
            let me = (k as u64, mover(p, now, bias).rate);
            let same = panes.iter().enumerate().filter(|(j, y)| *j != k && y.on == p.on && y.restricted == p.restricted).map(|(j, y)| (j as u64, mover(y, now, bias).rate));
            let kind = Decision::Rest {
                keeper: is_keeper(me, same),
                since_move_h: p.moved_at.map(|at| (now - at) as f64 / 3600.0),
                left: p.left.map(|(id, at)| (id, (now - at) as f64 / 3600.0)).into_iter().collect(),
                band_last_hour: 0,
                demand: 0.0,
                stale_feed: read.stale_feed(),
                speed_known: true,
                keeps_restricted: false,
                moves_week: p.planned.iter().filter(|at| now - **at < WEEK).count(),
            };
            let day = panes.iter().flat_map(|x| x.planned.iter()).filter(|at| now - **at < 86_400).count();
            if let Some((to, _)) = waste::decide(&q, &l, mover(p, now, bias), p.on, Some(KAPPA), &kind, day, &PARAMS) {
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

//! What the planner policy of the closed loop reads and hands a full
//! search: the accounts as read, level 1's spare and prices from the
//! recorded restricted buckets, and each pane's level (its measured
//! speed times the forecast's bias) and three-hour speed (its measured
//! speed times its own factor).
//!
//! The loop's measured speed has a three-hour half-life: it stands for
//! the shell's three-hour speed, and the 24-hour level is the forecast
//! made wrong by `bias`.  The three-hour speed is given the error the
//! rule it is compared with reads: true (`short` 1) against today's rule
//! on the true speeds, wrong by the same factor (`short` = `bias`)
//! against today's rule on the same wrong speeds, so the two sides see
//! the same information.  Panes whose three-hour speed is outside the
//! band around their level are handed to the triggers as the shell
//! hands them; no offers are modelled.  As in the shell, the speeds are
//! checked against what the accounts read (`calib`) and scaled by the
//! factor in force.  The feed goes out now and then, and the accounts
//! are read then as the shell reads them (`Feed`).

use super::loop_world::{P, Policy, RESTS, WEEK, hours, speeds};
use super::standing::Standing;
use super::trigger::{Now, Triggers};
use super::tests::KAPPA;
use super::*;
use crate::quota::{Class, Quant};
use crate::quota::waste::switch_cost;
use crate::sched::level1::{self, World};

/// What the policies read of the accounts: each one's week and
/// restricted window, with this reading's error, or while the feed is
/// out, as the shell reads them then (`Feed`).
pub(super) struct Read<'a> {
    pub acc: &'a [(Quant, i64)],
    pub err: &'a [[f64; 2]],
    pub feed: &'a Feed,
    pub now: i64,
}

impl Read<'_> {
    pub(super) fn quants(&self) -> Vec<Quant> {
        if self.feed.out(self.now) {
            return self.feed.stale(self.now);
        }
        self.fresh()
    }

    fn fresh(&self) -> Vec<Quant> {
        fresh(self.acc, self.err)
    }

    /// The feed is older than the shell's stale-feed limit.
    pub(super) fn stale_feed(&self) -> bool {
        self.feed.out(self.now) && self.now - self.feed.since > STALE_FEED_S
    }
}

fn fresh(acc: &[(Quant, i64)], err: &[[f64; 2]]) -> Vec<Quant> {
    acc.iter()
        .zip(err)
        .map(|((q, _), e)| {
            // an account that refuses reads empty: the feed knows it
            let read = |x: f64, e: f64, top: f64| if x <= 0.0 { 0.0 } else { (x + e).clamp(0.0, top) };
            let r = read(q.r, e[0], 1.0);
            Quant { r, f: read(q.f.min(q.r), e[1], r), ..q.clone() }
        })
        .collect()
}

/// The shell's limits: a feed older than this is stale, and an account
/// reading older than this has its week pushed forward by the measured
/// speeds of the panes on it.
const STALE_FEED_S: i64 = 15 * 60;
const STALE_ACCOUNT_S: i64 = 30 * 60;
/// Feed outages: how often one starts, an hour, and how long one lasts,
/// seconds.  The recorded decisions of 11-24 to 11-26 read no stale
/// account in 776 readings over 34 hours, which bounds the share of time
/// with the feed out to a few percent: one outage in about four days,
/// lasting from a reading missed to three hours (the length is not in
/// the records).
const OUT_PER_H: f64 = 0.01;
const OUT_S: [f64; 2] = [20.0 * 60.0, 180.0 * 60.0];

/// The usage feed as the shell sees it, outages drawn from a stream of
/// their own so that both policies meet the same ones.
pub(super) struct Feed {
    rng: super::oracle_tests::Rng,
    /// The last fresh reading, and when it was read.
    last: Vec<Quant>,
    since: i64,
    until: i64,
    /// The measured week an hour of the panes on each account, by input
    /// order, as of the last step.
    rate: Vec<f64>,
}

impl Feed {
    pub(super) fn new(seed: u64) -> Self {
        Feed { rng: super::oracle_tests::Rng(seed ^ 0x0FEE_D0FF_5EED_1234), last: Vec::new(), since: 0, until: 0, rate: Vec::new() }
    }

    fn out(&self, now: i64) -> bool {
        now > self.since && now < self.until
    }

    /// A step at `now`: while the feed is up, its reading is kept and an
    /// outage may start after it; while it is out, the speeds that push
    /// the old readings forward are taken.
    pub(super) fn step(&mut self, acc: &[(Quant, i64)], err: &[[f64; 2]], panes: &[P], now: i64, step_s: i64) {
        if self.out(now) {
            self.rate.clear();
            self.rate.extend(acc.iter().map(|(q, _)| panes.iter().filter(|p| p.on == q.id).map(|p| speeds(p, now)).map(|(f, n)| f + n).sum::<f64>()));
            return;
        }
        self.last = fresh(acc, err);
        self.since = now;
        if self.rng.next() < OUT_PER_H * step_s as f64 / 3600.0 {
            self.until = now + (OUT_S[0] + (OUT_S[1] - OUT_S[0]) * self.rng.next()) as i64;
        }
    }

    /// The accounts as the shell reads them while the feed is out: the
    /// last reading, its week pushed forward once it is older than the
    /// account limit; the clocks run on.
    fn stale(&self, now: i64) -> Vec<Quant> {
        let age = now - self.since;
        let stale = age > STALE_ACCOUNT_S;
        self.last
            .iter()
            .enumerate()
            .map(|(j, x)| {
                let r = if stale { (x.r - self.rate.get(j).copied().unwrap_or(0.0) * age as f64 / 3600.0).max(0.0) } else { x.r };
                let t = x.t - age as f64 / 3600.0;
                Quant { r, t, tf: x.tf - age as f64 / 3600.0, stale, ..x.clone() }
            })
            .collect()
    }
}

/// The recorded restricted use in six-hour buckets, weeks an hour: what
/// the shell hands level 1 as the fleet's buckets, and their highest as
/// the prudent rate.
fn buckets() -> Vec<f64> {
    hours().chunks_exact(6).map(|c| c.iter().map(|h| h[0]).sum::<f64>() / 6.0).collect()
}

/// The planner as the loop drives it: a full search on the hour and
/// after a refusal, or as the shell drives it, at every reading a full
/// search when a trigger says one is due, else the standing plan
/// re-timed.
#[derive(Default)]
pub(super) struct Driver {
    pub st: Standing,
    tr: Triggers,
    ended: bool,
    planned_once: bool,
    calib: crate::sched::calib::Calib,
}

impl Driver {
    /// A reading at `now`; `refused`: a pane was refused since the last.
    pub(super) fn step(&mut self, policy: Policy, read: &Read, panes: &[P], now: i64, refused: bool) {
        if let Policy::Planner { bias, short, .. } = policy {
            let q = read.quants();
            let sp = handed(panes, now, [bias, short]);
            let asked: Vec<f64> = q.iter().map(|x| panes.iter().zip(&sp).filter(|(p, _)| p.on == x.id).map(|(_, s)| s[0] + s[1]).sum()).collect();
            self.calib.observe(now, &q, &asked, read.stale_feed());
        }
        let f = self.calib.factor();
        match policy {
            Policy::Planner { bias, short, shell: true } => self.pass(read, panes, now, [bias, short], f, refused),
            Policy::Planner { bias, shell: false, .. } if refused || now % 3600 == 0 || !self.planned_once => {
                with_input(read, panes, now, [bias, bias], f, |i, _| {
                    pass::full(&mut self.st, i, now as u64, now);
                });
                self.planned_once = true;
            }
            _ => {}
        }
    }

    fn pass(&mut self, read: &Read, panes: &[P], now: i64, bias: [f64; 2], f: f64, refused: bool) {
        with_input(read, panes, now, bias, f, |i, outside| {
            let quants: Vec<_> = i.accounts.iter().map(|x| x.q.clone()).collect();
            let high: Vec<f64> = i.accounts.iter().map(|x| i.panes.iter().filter(|p| p.on == x.q.id).map(|p| p.high[0] + p.high[1]).sum()).collect();
            let counted: Vec<u64> = i.panes.iter().map(|p| p.sid).collect();
            let n = Now { at: now, accounts: &quants, high: &high, counted: &counted, offers: 0, refused, ended: self.ended, outside };
            let done = match self.tr.due(&n) {
                Some(_) => {
                    let r = pass::full(&mut self.st, i, now as u64, now);
                    self.tr.searched(&n, &r.plan.empty.iter().map(|e| e[1]).collect::<Vec<_>>(), r.plan.cuts.get(1).copied().unwrap_or(1.0));
                    Some(r)
                }
                None => pass::retime(&mut self.st, i, now),
            };
            self.ended = done.is_some_and(|r| !r.ended.cancelled.is_empty() || !r.ended.missed.is_empty());
        });
    }
}

/// The pane band the shell uses without a forecast: `[restricted, plain]`.
const BAND_LO: [f64; 2] = [0.3, 0.6];
const BAND_HI: [f64; 2] = [2.5, 1.5];

/// The level is the measured speed times `bias[0]`, the three-hour
/// speed the measured speed times `bias[1]`, and a pane is planned as
/// the shell plans it (`planned_speed`); `f` also gets the panes whose
/// three-hour speed is outside the band around their level.
/// Each pane's planned speed as the shell would hand it, before the
/// calibration factor.
fn handed(panes: &[P], now: i64, bias: [f64; 2]) -> Vec<[f64; 2]> {
    panes
        .iter()
        .map(|p| {
            let (fa, na) = speeds(p, now);
            let m = [fa, na];
            let (day, short) = (m.map(|x| x * bias[0]), m.map(|x| x * bias[1]));
            [0, 1].map(|c| planned_speed(day[c], short[c], BAND_LO[c], BAND_HI[c]))
        })
        .collect()
}

fn with_input(read: &Read, panes: &[P], now: i64, bias: [f64; 2], factor: f64, f: impl FnOnce(&Input, &[u64])) {
    let q = read.quants();
    let mut outside = Vec::new();
    for (k, p) in panes.iter().enumerate() {
        let (fa, na) = speeds(p, now);
        let m = [fa, na];
        let (day, short) = (m.map(|x| x * bias[0]), m.map(|x| x * bias[1]));
        if (0..2).any(|c| day[c] > 0.0 && (short[c] > day[c] * BAND_HI[c] || short[c] < day[c] * BAND_LO[c])) {
            outside.push(k as u64);
        }
    }
    let sp: Vec<[f64; 2]> = handed(panes, now, bias).into_iter().map(|s| s.map(|x| x * factor)).collect();
    let demand = [sp.iter().map(|x| x[0]).sum(), sp.iter().map(|x| x[1]).sum()];
    let b = buckets();
    let a_hi = b.iter().copied().fold(0.0, f64::max);
    let l1 = level1::plan(&World { accounts: &q, window_full: KAPPA, demand, prudent: a_hi.max(demand[0]), buckets: &b, offers: &[], closures: &[], last_pi: None });
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
                speed: sp[k],
                high: [sp[k][0] * 2.5, sp[k][1] * 1.5],
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
    let i = Input { accounts: &accounts, panes: &ps, young: [0.0; 2], window_full: KAPPA, pi: l1.pi, fleet_left: FLEET_PER_DAY.saturating_sub(day), notices_left: NOTICES_PER_HOUR, stale_feed: read.stale_feed() };
    f(&i, &outside);
}

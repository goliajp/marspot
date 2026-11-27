//! What the planner policy of the closed loop hands a full search: the
//! accounts as read, level 1's spare and prices from the recorded
//! restricted buckets, and each pane's measured speed times the
//! forecast's bias.

use super::loop_world::{P, RESTS, Read, WEEK, hours, speeds};
use super::standing::Standing;
use super::trigger::{Now, Triggers};
use super::tests::KAPPA;
use super::*;
use crate::quota::Class;
use crate::quota::waste::switch_cost;
use crate::sched::level1::{self, World};

/// The recorded restricted use in six-hour buckets, weeks an hour: what
/// the shell hands level 1 as the fleet's buckets, and their highest as
/// the prudent rate.
fn buckets() -> Vec<f64> {
    hours().chunks_exact(6).map(|c| c.iter().map(|h| h[0]).sum::<f64>() / 6.0).collect()
}

pub(super) fn planned(st: &mut Standing, read: &Read, panes: &[P], now: i64, bias: f64) {
    with_input(read, panes, now, bias, false, |i| {
        pass::full(st, i, now as u64, now);
    });
}

/// The planner as the shell drives it: at every reading a full search
/// when a trigger says one is due, else the standing plan re-timed.
#[derive(Default)]
pub(super) struct Driver {
    pub st: Standing,
    tr: Triggers,
    ended: bool,
}

impl Driver {
    pub(super) fn pass(&mut self, read: &Read, panes: &[P], now: i64, bias: f64, refused: bool) {
        with_input(read, panes, now, bias, true, |i| {
            let quants: Vec<_> = i.accounts.iter().map(|x| x.q.clone()).collect();
            let high: Vec<f64> = i.accounts.iter().map(|x| i.panes.iter().filter(|p| p.on == x.q.id).map(|p| p.high[0] + p.high[1]).sum()).collect();
            let counted: Vec<u64> = i.panes.iter().map(|p| p.sid).collect();
            let n = Now { at: now, accounts: &quants, high: &high, counted: &counted, offers: 0, refused, ended: self.ended, outside: &[] };
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

/// The level is the measured speed times `bias`; with `short`, as the
/// shell plans, the measured speed itself where the level is off by more
/// than its band.
fn with_input(read: &Read, panes: &[P], now: i64, bias: f64, short: bool, f: impl FnOnce(&Input)) {
    let q = read.quants();
    let level = |m: f64, c: usize| if short { planned_speed(m * bias, m, BAND_LO[c], BAND_HI[c]) } else { m * bias };
    let sp: Vec<(f64, f64)> = panes.iter().map(|p| speeds(p, now)).map(|(f, n)| (level(f, 0), level(n, 1))).collect();
    let demand = [sp.iter().map(|x| x.0).sum(), sp.iter().map(|x| x.1).sum()];
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
    let i = Input { accounts: &accounts, panes: &ps, young: [0.0; 2], window_full: KAPPA, pi: l1.pi, fleet_left: FLEET_PER_DAY.saturating_sub(day), notices_left: NOTICES_PER_HOUR, stale_feed: false };
    f(&i);
}

//! The real panes of 2026-11-25 00:00Z to 11-26 02:55Z (the fixture of
//! `waste_world`) under the pane planner.
//!
//! - `Actual`: every pane where it really was, refused panes waiting for
//!   the move that really happened; the planner is only asked, at the
//!   probe times, what it would plan.
//! - `Planner`: a full plan on the hour and after every refusal, from
//!   each pane's measured speed; a pane at rest whose planned switch has
//!   its window open takes it (every rest is taken as usable, an upper
//!   bound on the chances); new and refused panes are placed by the
//!   objective in force, as today.
//!
//! Plain use drawn from P3 while its week was all restricted window is
//! counted: what other models took of P3's `Phi`.

use std::collections::BTreeMap;

use super::choose::{HELPER_LAST_H, MUST_LAST_H, Mover};
use super::replay::Kind;
use super::speed::PaneUse;
use super::waste::{self, PARAMS, switch_cost};
use super::waste_world::{Ev, Fixture};
use super::{Class, EPS, KAPPA_FABLE, Load, Quant, RHO0};
use crate::sched::level1::{self, World};
use crate::sched::level2::{self, Account, DWELL_H, FLEET_PER_DAY, Input, NOTICES_PER_HOUR, Pane, Plan};

const WEEK_S: i64 = 7 * 24 * 3600;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Mode {
    Actual,
    Planner,
}

struct P {
    on: u8,
    class: Class,
    busy: bool,
    used: PaneUse,
    stalled: bool,
    left: Option<(u8, i64)>,
    planned: Vec<i64>,
}

#[derive(Default)]
pub(super) struct Run {
    pub switches: usize,
    pub planned: Vec<(i64, u64, u8, u8)>,
    pub refusals: Vec<(i64, u8, u64)>,
    pub unmet: f64,
    pub p3_phi_taken: f64,
    /// At each probe: the plan and every pane's account.
    pub probes: Vec<(i64, Plan, Vec<(u64, u8)>)>,
}

struct W {
    mode: Mode,
    accs: Vec<(Quant, i64, i64)>,
    panes: BTreeMap<u64, P>,
    run: Run,
}

pub(super) fn run(fx: &Fixture, mode: Mode, probes: &[i64]) -> Run {
    let accs = fx.start.iter().map(|&(id, w, f, r, rf)| (Quant { id, r: 1.0 - w, f: KAPPA_FABLE * (1.0 - f), t: 0.0, tf: 0.0, open: true, stale: false }, r, rf)).collect();
    let mut w = W { mode, accs, panes: BTreeMap::new(), run: Run::default() };
    let mut next = 0;
    let mut plan: Option<(i64, Plan)> = None;
    for k in 0..fx.len / fx.step {
        let now = fx.t0 + k * fx.step;
        w.renew(now);
        while next < fx.events.len() && fx.events[next].0 < now + fx.step {
            w.event(fx.events[next].0, &fx.events[next].1);
            next += 1;
        }
        for &t in probes.iter().filter(|t| **t >= now && **t < now + fx.step) {
            let p = w.plan(now);
            let on = w.panes.iter().map(|(s, x)| (*s, x.on)).collect();
            w.run.probes.push((t, p, on));
        }
        if mode == Mode::Planner {
            if plan.is_none() || (now - fx.t0) % 3600 == 0 {
                plan = Some((now, w.plan(now)));
            }
            if let Some((at, p)) = &plan {
                w.follow(now, *at, p);
            }
        }
        let before = w.run.refusals.len();
        for (sid, class, amount) in fx.usage.get(&k).cloned().unwrap_or_default() {
            w.use_(now, sid, class, amount);
        }
        if mode == Mode::Planner && w.run.refusals.len() > before {
            plan = Some((now, w.plan(now)));
        }
    }
    w.run
}

impl W {
    fn renew(&mut self, now: i64) {
        for (q, r, rf) in &mut self.accs {
            if now >= *r {
                q.r = 1.0;
                *r += WEEK_S;
            }
            if now >= *rf {
                q.f = KAPPA_FABLE;
                *rf += WEEK_S;
            }
            q.t = (*r - now) as f64 / 3600.0;
            q.tf = (*rf - now) as f64 / 3600.0;
        }
    }

    fn quants(&self) -> Vec<Quant> {
        self.accs.iter().map(|a| a.0.clone()).collect()
    }

    fn rates(&self, now: i64, p: &P) -> (f64, f64) {
        let (f, n, _) = p.used.rate_or_default(now, p.class);
        (f, n)
    }

    fn loads(&self, now: i64, skip: Option<u64>) -> Vec<Load> {
        let mut l = vec![Load::default(); self.accs.len()];
        for (_, p) in self.panes.iter().filter(|(s, _)| Some(**s) != skip) {
            let (f, n) = self.rates(now, p);
            if let Some(j) = self.accs.iter().position(|a| a.0.id == p.on) {
                l[j].restricted += f;
                l[j].plain += n;
            }
        }
        l
    }

    fn place(&self, now: i64, m: Mover, from: Option<(u8, u64)>, kind: Kind) -> Option<u8> {
        let q = self.quants();
        let l = self.loads(now, from.map(|f| f.1));
        waste::decide(&q, &l, m, from.map_or(0, |f| f.0), Some(KAPPA_FABLE), &kind, 0, &PARAMS).map(|(t, _)| t)
    }

    fn event(&mut self, at: i64, ev: &Ev) {
        match *ev {
            Ev::Bind(sid, to, class) => match self.panes.get_mut(&sid) {
                None => self.add(at, sid, to, class),
                Some(p) if self.mode == Mode::Actual && p.on != to => {
                    p.left = Some((p.on, at));
                    p.on = to;
                    p.stalled = false;
                    self.run.switches += 1;
                }
                Some(_) => {}
            },
            Ev::New(sid, to, class) => {
                let to = if self.mode == Mode::Planner { self.place(at, Mover { class, rate: RHO0, tokens: None, fable: None }, None, Kind::New { horizon_h: HELPER_LAST_H }).unwrap_or(to) } else { to };
                self.add(at, sid, to, class);
            }
            Ev::Gone(sid) => {
                self.panes.remove(&sid);
            }
            Ev::Busy(sid, busy) => {
                if let Some(p) = self.panes.get_mut(&sid) {
                    p.busy = busy;
                }
            }
            Ev::Would(..) => {}
        }
    }

    fn add(&mut self, at: i64, sid: u64, on: u8, class: Class) {
        let mut used = PaneUse::default();
        used.watched(at);
        self.panes.insert(sid, P { on, class, busy: false, used, stalled: false, left: None, planned: Vec::new() });
    }

    fn use_(&mut self, now: i64, sid: u64, class: Class, amount: f64) {
        let Some(p) = self.panes.get_mut(&sid) else { return };
        p.used.add(now, class, amount);
        let (on, own) = (p.on, p.class);
        let a = &mut self.accs.iter_mut().find(|a| a.0.id == on).expect("a known account").0;
        let served = match class {
            Class::Restricted => amount.min(a.f.max(0.0)).min(a.r.max(0.0)),
            Class::Plain => amount.min(a.r.max(0.0)),
        };
        if class == Class::Plain && on == 3 && a.r <= a.f + EPS {
            self.run.p3_phi_taken += served;
        }
        a.r -= served;
        if class == Class::Restricted {
            a.f -= served;
        }
        self.run.unmet += amount - served;
        if amount - served <= EPS || class != own {
            return;
        }
        let p = self.panes.get_mut(&sid).expect("the pane");
        if p.stalled {
            return;
        }
        p.stalled = true;
        self.run.refusals.push((now, on, sid));
        if self.mode == Mode::Planner {
            let (f, n) = self.rates(now, &self.panes[&sid]);
            let m = Mover { class: own, rate: f + n, tokens: None, fable: Some(f) };
            if let Some(to) = self.place(now, m, Some((on, sid)), Kind::Out { horizon_h: MUST_LAST_H }) {
                let p = self.panes.get_mut(&sid).expect("the pane");
                p.left = Some((p.on, now));
                p.on = to;
                p.stalled = false;
                self.run.switches += 1;
            }
        }
    }

    /// Resting panes take their planned switches whose windows are open.
    fn follow(&mut self, now: i64, at: i64, plan: &Plan) {
        let h = (now - at) as f64 / 3600.0;
        for s in plan.switches.iter().filter(|s| s.order == 0 && s.t_open <= h + 1e-9) {
            let Some(p) = self.panes.get_mut(&s.sid) else { continue };
            if p.busy || p.on != s.from {
                continue;
            }
            p.left = Some((p.on, now));
            p.on = s.to;
            p.planned.push(now);
            p.stalled = false;
            self.run.switches += 1;
            self.run.planned.push((now, s.sid, s.from, s.to));
        }
    }

    fn plan(&self, now: i64) -> Plan {
        let q = self.quants();
        let sp: Vec<(u64, f64, f64)> = self.panes.iter().map(|(s, p)| (*s, self.rates(now, p).0, self.rates(now, p).1)).collect();
        let demand = [sp.iter().map(|x| x.1).sum(), sp.iter().map(|x| x.2).sum()];
        let l1 = level1::plan(&World { accounts: &q, window_full: KAPPA_FABLE, demand, prudent: demand[0] * 2.5, buckets: &[], offers: &[], closures: &[], last_pi: None });
        let accounts: Vec<Account> = q.iter().enumerate().map(|(j, x)| Account { q: x.clone(), spare: l1.spare[j], price: l1.price[j].unwrap_or([0.0; 2]) }).collect();
        let panes: Vec<Pane> = self
            .panes
            .iter()
            .zip(&sp)
            .map(|((sid, p), x)| {
                let since = p.planned.last().map_or(f64::INFINITY, |t| (now - t) as f64 / 3600.0);
                Pane {
                    sid: *sid,
                    on: p.on,
                    restricted: p.class == Class::Restricted,
                    speed: [x.1, x.2],
                    high: [x.1 * 2.5, x.2 * 1.5],
                    rewrite: switch_cost(p.class, None),
                    lost_work: 0.0,
                    clean_per_h: 0.0,
                    long_background: false,
                    earliest_h: (DWELL_H - since).max(0.0),
                    left: p.left.map(|(id, at)| (id, DWELL_H - (now - at) as f64 / 3600.0)),
                    barred: 0,
                    switches_left: 6usize.saturating_sub(p.planned.iter().filter(|t| now - **t < WEEK_S).count()) as u8,
                    disturb_left: 4,
                }
            })
            .collect();
        let day = self.run.planned.iter().filter(|x| now - x.0 < 86_400).count();
        let i = Input { accounts: &accounts, panes: &panes, young: [0.0; 2], window_full: KAPPA_FABLE, pi: l1.pi, fleet_left: FLEET_PER_DAY.saturating_sub(day), notices_left: NOTICES_PER_HOUR, stale_feed: false };
        level2::plan(&i, None)
    }
}

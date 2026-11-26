//! The real panes of 2026-11-25 00:00Z to 11-26 02:55Z, replayed under
//! three rules.
//!
//! The fixture holds the accounts' readings at the start and the end
//! (the control plane's five-minute polls), each pane's use per five
//! minutes by class (its transcript's tokens at the prices the shell
//! uses), when each pane was busy or resting, where the shell actually
//! put it, and the resting moves the dry run would have made.  Use is
//! served while the account has room for its class; what is not is
//! unmet, and a pane whose own class is not served is refused once and
//! moves where its rule says.
//!
//! - `Actual`: every pane where it really was; a refused pane waits for
//!   the move that really happened.
//! - `DryRun`: the same, with the dry run's would-moves made; a pane
//!   moved off its real path is placed by the bands when refused.
//! - `Waste`: the one objective places new panes and refused ones and
//!   makes the resting moves it finds worth their cost.

use std::collections::BTreeMap;

use super::choose::{HELPER_LAST_H, MUST_LAST_H, Mover};
use super::replay::{Inputs, Kind, Rule};
use super::rest::RESTRICTED_USER;
use super::speed::PaneUse;
use super::waste::{self, Params};
use super::{Class, EPS, KAPPA_FABLE, Load, Quant};

pub(super) const FIXTURE: &str = include_str!("fixtures/replay_20261125.txt");

const WEEK_S: i64 = 7 * 24 * 3600;

#[derive(Clone, Debug)]
enum Ev {
    Bind(u64, u8, Class),
    New(u64, u8, Class),
    Gone(u64),
    Busy(u64, bool),
    Would(u64, u8),
}

#[derive(Clone, Debug, Default)]
pub(super) struct Fixture {
    t0: i64,
    len: i64,
    step: i64,
    /// `(account, week used, Fable used, week renews, Fable renews)`, unix.
    start: Vec<(u8, f64, f64, i64, i64)>,
    /// What the polls read at the end: `(account, week used, Fable used)`.
    pub end: Vec<(u8, f64, f64)>,
    events: Vec<(i64, Ev)>,
    usage: BTreeMap<i64, Vec<(u64, Class, f64)>>,
}

fn class_of(w: &str) -> Class {
    if w == "F" { Class::Restricted } else { Class::Plain }
}

pub(super) fn fixture() -> Fixture {
    let mut fx = Fixture::default();
    for line in FIXTURE.lines().filter(|l| !l.starts_with('#')) {
        let w: Vec<&str> = line.split(' ').collect();
        let n = |i: usize| w[i].parse::<i64>().unwrap();
        let x = |i: usize| w[i].parse::<f64>().unwrap();
        let at = |i: usize| fx.t0 + n(i);
        match w[0] {
            "T" => (fx.t0, fx.len, fx.step) = (n(1), n(2), n(3)),
            "A" => fx.start.push((n(1) as u8, x(2), x(3), at(4), at(5))),
            "E" => fx.end.push((n(1) as u8, x(2), x(3))),
            "B" => fx.events.push((at(1), Ev::Bind(n(2) as u64, n(3) as u8, class_of(w[4])))),
            "N" => fx.events.push((at(1), Ev::New(n(2) as u64, n(3) as u8, class_of(w[4])))),
            "G" => fx.events.push((at(1), Ev::Gone(n(2) as u64))),
            "X" | "R" => fx.events.push((at(1), Ev::Busy(n(2) as u64, w[0] == "X"))),
            "W" => fx.events.push((at(1), Ev::Would(n(2) as u64, n(3) as u8))),
            "U" => fx.usage.entry(n(1)).or_default().push((n(2) as u64, class_of(w[3]), x(4))),
            _ => panic!("unknown fixture line {line}"),
        }
    }
    fx.events.sort_by_key(|(at, _)| *at);
    fx
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Policy {
    Actual,
    DryRun,
    Waste(Params),
}

struct Pane {
    on: u8,
    class: Class,
    busy: bool,
    used: PaneUse,
    moved_at: Option<i64>,
    left: Option<(u8, i64)>,
    rest_moves: Vec<i64>,
    /// Moved off where it really was (`DryRun`).
    diverged: bool,
    stalled: bool,
}

struct Acc {
    id: u8,
    r: f64,
    f: f64,
    renews: i64,
    renews_f: i64,
}

/// One account at the end, and run on to its renewal.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct AccEnd {
    pub id: u8,
    pub week: f64,
    pub fable: f64,
    pub w7: f64,
    pub wf: f64,
}

impl AccEnd {
    /// Its week is full at the end with this much of Fable unused.
    pub(super) fn stranded(&self) -> f64 {
        if self.week >= 0.97 { 1.0 - self.fable } else { 0.0 }
    }

    /// Its week is full at the renewal with this much of Fable unused.
    pub(super) fn stranded_at_reset(&self) -> f64 {
        if self.w7 < 0.03 { self.wf / KAPPA_FABLE } else { 0.0 }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Outcome {
    pub switches: usize,
    pub resting_moves: usize,
    pub refusals: usize,
    pub unmet: f64,
    pub accounts: Vec<AccEnd>,
}

impl Outcome {
    pub(super) fn w7(&self) -> f64 {
        self.accounts.iter().map(|a| a.w7).sum()
    }

    pub(super) fn wf(&self) -> f64 {
        self.accounts.iter().map(|a| a.wf).sum()
    }
}

struct World {
    policy: Policy,
    accs: Vec<Acc>,
    panes: BTreeMap<u64, Pane>,
    resting_at: Vec<i64>,
    out: Outcome,
}

pub(super) fn run(fx: &Fixture, policy: Policy) -> Outcome {
    let accs = fx.start.iter().map(|&(id, w, f, renews, renews_f)| Acc { id, r: 1.0 - w, f: KAPPA_FABLE * (1.0 - f), renews, renews_f }).collect();
    let mut w = World { policy, accs, panes: BTreeMap::new(), resting_at: Vec::new(), out: Outcome::default() };
    let mut next = 0;
    let steps = fx.len / fx.step;
    for k in 0..steps {
        let now = fx.t0 + k * fx.step;
        while next < fx.events.len() && fx.events[next].0 < now + fx.step {
            w.event(fx.events[next].0, &fx.events[next].1);
            next += 1;
        }
        if let Policy::Waste(p) = policy {
            w.rest(now, &p);
        }
        for (sid, class, amount) in fx.usage.get(&k).cloned().unwrap_or_default() {
            w.use_(now, sid, class, amount);
        }
        w.renew(now + fx.step);
    }
    w.finish(fx.t0 + steps * fx.step)
}

impl World {
    fn event(&mut self, at: i64, ev: &Ev) {
        match *ev {
            Ev::Bind(sid, to, class) => match self.panes.get_mut(&sid) {
                None => self.add(at, sid, to, class),
                Some(p) if self.policy == Policy::Actual || (self.policy == Policy::DryRun && !p.diverged) => {
                    if p.on != to {
                        self.out.switches += 1;
                        p.left = Some((p.on, at));
                        p.on = to;
                        p.moved_at = Some(at);
                        p.stalled = false;
                    }
                }
                Some(_) => {}
            },
            Ev::New(sid, to, class) => {
                let to = match self.policy {
                    Policy::Waste(p) => self.place(at, Mover { class, rate: super::RHO0 }, None, Kind::New { horizon_h: HELPER_LAST_H }, &p).unwrap_or(to),
                    _ => to,
                };
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
            Ev::Would(sid, to) => {
                if self.policy == Policy::DryRun && self.panes.get(&sid).is_some_and(|p| p.on != to) {
                    self.move_to(at, sid, to, true);
                    if let Some(p) = self.panes.get_mut(&sid) {
                        p.diverged = true;
                    }
                }
            }
        }
    }

    fn add(&mut self, at: i64, sid: u64, on: u8, class: Class) {
        let mut used = PaneUse::default();
        used.watched(at);
        self.panes.insert(sid, Pane { on, class, busy: false, used, moved_at: None, left: None, rest_moves: Vec::new(), diverged: false, stalled: false });
    }

    fn move_to(&mut self, at: i64, sid: u64, to: u8, resting: bool) {
        let Some(p) = self.panes.get_mut(&sid) else { return };
        self.out.switches += 1;
        if resting {
            self.out.resting_moves += 1;
            p.rest_moves.push(at);
            self.resting_at.push(at);
        }
        p.left = Some((p.on, at));
        p.on = to;
        p.moved_at = Some(at);
        p.stalled = false;
    }

    fn quants(&self, now: i64) -> Vec<Quant> {
        let h = |at: i64| (at - now) as f64 / 3600.0;
        self.accs.iter().map(|a| Quant { id: a.id, r: a.r, f: a.f, t: h(a.renews), tf: h(a.renews_f), open: true, stale: false }).collect()
    }

    /// Restricted and plain speed of `sid` now, and whether measured.
    fn rates(&self, now: i64, sid: u64) -> (f64, f64, bool) {
        let p = &self.panes[&sid];
        p.used.rate_or_default(now, p.class)
    }

    /// Every pane's load per account, leaving `skip` out.
    fn loads(&self, now: i64, accounts: &[Quant], skip: Option<u64>) -> (Vec<Load>, f64, f64) {
        let mut loads = vec![Load::default(); accounts.len()];
        let (mut bf, mut bn) = (0.0, 0.0);
        for (&sid, p) in &self.panes {
            let (f, n, _) = self.rates(now, sid);
            bf += f;
            bn += n;
            if Some(sid) == skip {
                continue;
            }
            if let Some(i) = accounts.iter().position(|q| q.id == p.on) {
                loads[i].restricted += f;
                loads[i].plain += n;
            }
        }
        (loads, bf, bn)
    }

    fn inputs(&self, now: i64, m: Mover, here: u8, skip: Option<u64>, kind: Kind) -> Inputs {
        let accounts = self.quants(now);
        let (loads, b_f, b_n) = self.loads(now, &accounts, skip);
        Inputs { accounts, b_f, b_n, kappa: Some(KAPPA_FABLE), loads, mover: m, here, kind }
    }

    fn place(&self, now: i64, m: Mover, from: Option<(u8, u64)>, kind: Kind, p: &Params) -> Option<u8> {
        let x = self.inputs(now, m, from.map_or(0, |f| f.0), from.map(|f| f.1), kind);
        waste::decide(&x.accounts, &x.loads, x.mover, x.here, x.kappa, &x.kind, 0, p).map(|(t, _)| t)
    }

    fn mover(&self, now: i64, sid: u64) -> Mover {
        let (f, n, _) = self.rates(now, sid);
        Mover { class: self.panes[&sid].class, rate: f + n }
    }

    /// The first resting pane the objective would move, moved.
    fn rest(&mut self, now: i64, p: &Params) {
        self.resting_at.retain(|at| now - at < 24 * 3600);
        let band = self.resting_at.iter().filter(|at| now - **at < 3600).count();
        let sids: Vec<u64> = self.panes.iter().filter(|(_, x)| !x.busy && !x.stalled).map(|(s, _)| *s).collect();
        for sid in sids {
            let x = &self.panes[&sid];
            let (f, _, known) = self.rates(now, sid);
            let kind = Kind::Rest {
                keeper: false,
                since_move_h: x.moved_at.map(|at| (now - at) as f64 / 3600.0),
                left: x.left.map(|(id, at)| (id, (now - at) as f64 / 3600.0)).into_iter().collect(),
                band_last_hour: band,
                demand: 0.0,
                stale_feed: false,
                speed_known: known,
                keeps_restricted: x.class == Class::Plain && f > RESTRICTED_USER,
                moves_week: x.rest_moves.iter().filter(|at| now - **at < WEEK_S).count(),
            };
            let i = self.inputs(now, self.mover(now, sid), x.on, None, kind);
            if let Some((to, _)) = waste::decide(&i.accounts, &i.loads, i.mover, i.here, i.kappa, &i.kind, self.resting_at.len(), p) {
                self.move_to(now, sid, to, true);
                return;
            }
        }
    }

    fn use_(&mut self, now: i64, sid: u64, class: Class, amount: f64) {
        let Some(p) = self.panes.get_mut(&sid) else { return };
        p.used.add(now, class, amount);
        let on = p.on;
        let own = p.class;
        let a = self.accs.iter_mut().find(|a| a.id == on).expect("a known account");
        let served = match class {
            Class::Restricted => amount.min(a.f.max(0.0)).min(a.r.max(0.0)),
            Class::Plain => amount.min(a.r.max(0.0)),
        };
        a.r -= served;
        if class == Class::Restricted {
            a.f -= served;
        }
        self.out.unmet += amount - served;
        if amount - served <= EPS || class != own {
            return;
        }
        let p = self.panes.get_mut(&sid).expect("the pane");
        if !p.stalled {
            p.stalled = true;
            self.out.refusals += 1;
        }
        let diverged = p.diverged;
        let to = match self.policy {
            Policy::Actual => None,
            Policy::DryRun if !diverged => None,
            Policy::DryRun => {
                let m = self.mover(now, sid);
                let x = self.inputs(now, m, on, Some(sid), Kind::Out { horizon_h: MUST_LAST_H });
                x.decide_by(&x.plan(), Rule::Bands).map(|(t, _)| t)
            }
            Policy::Waste(params) => self.place(now, self.mover(now, sid), Some((on, sid)), Kind::Out { horizon_h: MUST_LAST_H }, &params),
        };
        if let Some(to) = to {
            self.move_to(now, sid, to, false);
        }
    }

    fn renew(&mut self, at: i64) {
        for a in &mut self.accs {
            if at >= a.renews {
                a.r = 1.0;
                a.renews += WEEK_S;
            }
            if at >= a.renews_f {
                a.f = KAPPA_FABLE;
                a.renews_f += WEEK_S;
            }
        }
        let now = at;
        for sid in self.panes.keys().copied().collect::<Vec<_>>() {
            let (f, n, known) = self.rates(now, sid);
            if known && let Some(p) = self.panes.get_mut(&sid) {
                p.class = if f > n { Class::Restricted } else { Class::Plain };
            }
        }
    }

    fn finish(mut self, now: i64) -> Outcome {
        let accounts = self.quants(now);
        let (loads, _, _) = self.loads(now, &accounts, None);
        self.out.accounts = accounts
            .iter()
            .zip(&loads)
            .map(|(q, l)| {
                let o = waste::forecast(q, l, None, Some(KAPPA_FABLE), Class::Plain);
                AccEnd { id: q.id, week: 1.0 - q.r, fable: 1.0 - q.f / KAPPA_FABLE, w7: o.w7, wf: o.wf }
            })
            .collect();
        self.out
    }
}

/// Rounded, for a table.
pub(super) fn pts(x: f64) -> f64 {
    (x * 1000.0).round() / 10.0
}

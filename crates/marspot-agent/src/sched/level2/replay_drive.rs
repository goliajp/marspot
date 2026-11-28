//! The replay's planner: what each pass reads, as the shell builds it,
//! and the executor at each recorded boundary.
//!
//! A pane counts at pane level once half an hour old; its level is its
//! measured speed (three-hour half-life), its high band the shell's
//! default multipliers.  A switch costs what this machine measured for
//! the context the pane's last reply read.  Level 1 reads the recorded
//! restricted use so far in six-hour buckets, their highest as the
//! prudent rate.  Background work is not modelled: nothing is lost by a
//! switch after a notice, and no pane holds long background work.

use super::super::exec::{self, Boundary, Watched, Would};
use super::super::loop_price::{self, MEASURED, Pricing};
use super::fx::Bound;
use super::super::route::{Newcomer, RefusedTo, Routing};
use super::super::trigger::Now;
use super::super::{Account, H2, Input, Kind, Limits, Pane, pass};
use super::{W, WEEK, Watch};
use crate::quota::choose::{MUST_LAST_H, Mover};
use crate::quota::replay::Kind as Decision;
use crate::quota::waste::{self, PARAMS};
use crate::quota::{Class, KAPPA_FABLE, Load, RHO0};
use crate::sched::level1::{self, World};

const PRICING: Pricing = Pricing::Context(MEASURED);
const YOUNG_S: i64 = 1800;
const DAY_S: i64 = 86_400;
/// Clean rests an hour before a pane has an hour of boundaries.
const CLEAN_PER_H: f64 = 0.09;

fn high_of(class: Class) -> f64 {
    if class == Class::Restricted { 2.5 } else { 1.5 }
}

impl W<'_> {
    pub(super) fn speeds(&self, sid: u64, now: i64) -> [f64; 2] {
        let p = &self.panes[&sid];
        let (f, n, _) = p.used.rate_or_default(now, p.class);
        [f, n]
    }

    fn clean_per_h(&self, sid: u64, now: i64) -> f64 {
        let p = &self.panes[&sid];
        let h = (now - p.first_at.max(self.fx.bounds_from)) as f64 / 3600.0;
        // as the forecast does, a pane with no clean rest of its own reads the fleet's rate
        if h < 1.0 || p.clean == 0 { CLEAN_PER_H } else { f64::from(p.clean) / h }
    }

    /// One pass at `now`: a full search when a trigger is due, else the
    /// standing plan re-timed; the routing table and the watched
    /// switches from what it leaves.
    pub(super) fn pass(&mut self, now: i64, lim: Limits) {
        let quants: Vec<_> = self.accs.iter().map(|a| a.q.clone()).collect();
        let mut young = [0.0; 2];
        let mut panes = Vec::new();
        let sids: Vec<u64> = self.panes.keys().copied().collect();
        for sid in sids {
            let sp = self.speeds(sid, now);
            let p = &self.panes[&sid];
            if now - p.first_at < YOUNG_S {
                young = [young[0] + sp[0], young[1] + sp[1]];
                continue;
            }
            let hours = |at: i64| (now - at) as f64 / 3600.0;
            let since = p.moved_at.map_or(f64::INFINITY, hours);
            let ctx = if p.ctx > 0 { p.ctx } else { loop_price::FIXED_CTX };
            let planned = |span: i64| p.planned.iter().filter(|t| now - **t < span).count();
            panes.push(Pane {
                sid,
                on: p.on,
                restricted: p.class == Class::Restricted,
                speed: sp,
                high: [sp[0] * 2.5, sp[1] * 1.5],
                rewrite: loop_price::cost(PRICING, p.class, ctx),
                lost_work: 0.0,
                clean_per_h: self.clean_per_h(sid, now),
                long_background: false,
                earliest_h: (lim.dwell_h - since).clamp(0.0, H2),
                left: p.left.map(|(id, at)| (id, lim.dwell_h - hours(at))),
                barred: 0,
                switches_left: lim.switches_left(planned(WEEK), planned(DAY_S)),
                disturb_left: 4,
                attended: p.user_at.is_some_and(|t| now - t < 3600),
            });
        }
        let demand = [panes.iter().map(|p| p.speed[0]).sum::<f64>() + young[0], panes.iter().map(|p| p.speed[1]).sum::<f64>() + young[1]];
        let buckets: Vec<f64> = self.hours_r.chunks_exact(6).map(|c| c.iter().sum::<f64>() / 6.0).collect();
        let prudent = buckets.iter().copied().fold(demand[0], f64::max);
        let l1 = level1::plan(&World { accounts: &quants, window_full: KAPPA_FABLE, demand, prudent, buckets: &buckets, offers: &[], closures: &[], last_pi: None });
        for (a, s) in self.accs.iter_mut().zip(&l1.spare) {
            a.spare = *s;
        }
        let accounts: Vec<Account> = quants.iter().enumerate().map(|(j, q)| Account { q: q.clone(), spare: l1.spare[j], price: l1.price[j].unwrap_or([0.0; 2]) }).collect();
        // a switch that averts a refusal soon does not count against the fleet's day
        let day = self.out.planned.iter().filter(|x| now - x.0 < DAY_S && !x.5).count();
        self.notices.retain(|t| now - t < 3600);
        let input = Input { accounts: &accounts, panes: &panes, young, window_full: KAPPA_FABLE, pi: lim.pi(l1.pi), fleet_left: lim.fleet_per_day.saturating_sub(day), notices_left: lim.notices_per_hour.saturating_sub(self.notices.len()), stale_feed: false, limits: lim };
        self.waits = super::super::guard::waits(&input);
        let high: Vec<f64> = accounts.iter().map(|x| panes.iter().filter(|p| p.on == x.q.id).map(|p| p.high[0] + p.high[1]).sum()).collect();
        let counted: Vec<u64> = panes.iter().map(|p| p.sid).collect();
        let n = Now { at: now, accounts: &quants, high: &high, counted: &counted, offers: 0, refused: self.refused, ended: self.ended, outside: &[] };
        let done = match self.tr.due(&n) {
            Some(_) => {
                self.out.full += 1;
                let r = pass::full(&mut self.st, &input, now as u64, now);
                self.tr.searched(&n, &r.plan.empty.iter().map(|e| e[1]).collect::<Vec<_>>(), r.plan.cuts.get(1).copied().unwrap_or(1.0));
                Some(r)
            }
            None => pass::retime(&mut self.st, &input, now),
        };
        self.refused = false;
        self.ended = done.as_ref().is_some_and(|r| !r.ended.cancelled.is_empty() || !r.ended.missed.is_empty());
        self.watch.clear();
        let Some(r) = done else {
            self.deadlines.passed();
            return;
        };
        self.table = Some(Routing::of(&input, &r.plan));
        let at = |h: f64| now + (h * 3600.0) as i64;
        for s in r.plan.switches.iter().filter(|s| s.order == 0) {
            let Some(p) = panes.iter().find(|p| p.sid == s.sid && p.on == s.from) else { continue };
            let w = Watched { open_at: at(s.t_open), dead_at: at(s.t_dead), exempt: s.exempt, clean_per_h: p.clean_per_h, long_background: false, lost_work: 0.0, gain: s.gain, noticed_at: None };
            let (w, _) = self.deadlines.hold(s.sid, s.from, s.to, w, now, i64::from(lim.notice_wait_min) * 60);
            self.watch.push(Watch { sid: s.sid, from: s.from, to: s.to, w });
        }
        self.deadlines.passed();
    }

    /// Before the first recorded boundary: a pane at rest takes its
    /// committed switch once the window is open.
    pub(super) fn take_at_rest(&mut self, now: i64) {
        let open: Vec<Watch> = self.watch.iter().copied().filter(|w| w.w.open_at <= now).collect();
        for w in open {
            let ready = self.panes.get(&w.sid).is_some_and(|p| p.on == w.from && !p.busy && !p.stalled);
            if ready && self.open(w.to) {
                self.switch(now, w.sid, w.to, Some((Kind::Clean, w.w.exempt)));
                self.ended = true;
            }
        }
    }

    /// A recorded boundary of `sid` at `at`: the executor's answer for
    /// its watched switch, if any.
    pub(super) fn at_boundary(&mut self, at: i64, sid: u64, b: Bound, lim: &Limits) {
        let Some(p) = self.panes.get_mut(&sid) else { return };
        if b.ctx > 0 {
            p.ctx = b.ctx;
        }
        p.clean += u32::from(!b.live && b.quiet >= exec::CLEAN_S);
        if b.user {
            p.user_at = Some(at);
        }
        let (on, noticed) = (p.on, p.noticed);
        let Some(w) = self.watch.iter().copied().find(|w| w.sid == sid && w.from == on) else { return };
        self.out.diag[0] += 1;
        self.out.diag[1] += usize::from(b.user);
        self.out.diag[2] += usize::from(at < w.w.open_at);
        let mut watched = w.w;
        watched.noticed_at = noticed.filter(|n| n.1 == w.to).map(|n| n.0);
        let boundary = Boundary { at, turn_running: false, live: b.live, user: b.user, quiet_s: b.quiet };
        let would = exec::decide(&watched, &boundary, lim);
        self.out.diag[3 + match would { Would::Wait => 0, Would::Notify(_) => 1, _ => 2 }] += 1;
        match would {
            Would::Switch(k) if self.open(w.to) => {
                self.switch(at, sid, w.to, Some((k, w.w.exempt)));
                self.ended = true;
            }
            Would::Notify(_) if noticed.is_none() => {
                self.panes.get_mut(&sid).expect("the pane").noticed = Some((at, w.to));
                self.notices.push(at);
                self.out.notices += 1;
            }
            Would::Cancel => {
                self.panes.get_mut(&sid).expect("the pane").noticed = None;
                self.watch.retain(|x| x.sid != sid);
            }
            _ => {}
        }
    }

    /// Where the routing table places a pane opened now.
    pub(super) fn place_new(&mut self, class: Class) -> Option<u8> {
        let m = Newcomer { class, rate: RHO0, high: RHO0 * high_of(class), life_h: H2 };
        let to = self.table.as_mut()?.place(&m, None)?.to;
        self.open(to).then_some(to)
    }

    /// Where a refused pane goes: the routing table's entry, else today's objective.
    pub(super) fn place_refused(&mut self, now: i64, sid: u64) -> Option<u8> {
        let sp = self.speeds(sid, now);
        let p = &self.panes[&sid];
        let (on, class, ctx) = (p.on, p.class, p.ctx);
        let rate = if sp[0] + sp[1] > 0.0 { sp[0] + sp[1] } else { RHO0 };
        let m = Newcomer { class, rate, high: rate * high_of(class), life_h: H2 };
        match self.table.as_mut().map(|t| t.place_refused(&m, on, sid)) {
            Some(RefusedTo::To(x)) if self.open(x.to) => return Some(x.to),
            Some(RefusedTo::Wait) => return None,
            _ => {}
        }
        let q: Vec<_> = self.accs.iter().map(|a| a.q.clone()).collect();
        let mut loads = vec![Load::default(); q.len()];
        for (s, x) in self.panes.iter().filter(|(s, _)| **s != sid) {
            let v = self.speeds(*s, now);
            if let Some(j) = q.iter().position(|a| a.id == x.on) {
                loads[j].restricted += v[0];
                loads[j].plain += v[1];
            }
        }
        let mover = Mover { class, rate, tokens: Some(ctx), fable: Some(sp[0]) };
        waste::decide(&q, &loads, mover, on, Some(KAPPA_FABLE), &Decision::Out { horizon_h: MUST_LAST_H }, 0, &PARAMS).map(|(t, _)| t)
    }
}

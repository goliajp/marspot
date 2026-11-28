//! What happens to a pane of the closed loop at a rest and at a
//! refusal, under each policy, and what each switch costs.

use super::loop_plan::{Driver, Read};
use super::loop_price::{self, Pricing};
use super::loop_world::{P, Policy, WEEK, Week, speeds};
use super::tests::KAPPA;
use crate::quota::choose::{MUST_LAST_H, Mover};
use crate::quota::replay::Kind as Decision;
use crate::quota::rest::is_keeper;
use crate::quota::waste::{self, PARAMS};
use crate::quota::{Load, Quant};

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
    Mover { class: p.class(), rate: f + n, tokens: Some(p.ctx), fable: Some(f) }
}

fn switch(panes: &mut [P], k: usize, to: u8, now: i64, pricing: Pricing, w: &mut Week) {
    let p = &mut panes[k];
    w.switches += 1;
    w.switches_r += usize::from(p.restricted);
    let rw = loop_price::cost(pricing, p.class(), p.ctx);
    w.long_switches += usize::from(p.ctx >= loop_price::LONG);
    w.money += rw;
    w.switch_rw += rw;
    p.left = Some((p.on, now));
    p.on = to;
    p.moved_at = Some(now);
    p.stalled = false;
}

pub(super) fn refuse(read: &Read, panes: &mut [P], k: usize, now: i64, pricing: Pricing, w: &mut Week) {
    let q = read.quants();
    let l = loads(&q, panes, now, Some(k), 1.0);
    let m = mover(&panes[k], now, 1.0);
    if let Some((to, _)) = waste::decide(&q, &l, m, panes[k].on, Some(KAPPA), &Decision::Out { horizon_h: MUST_LAST_H }, 0, &PARAMS) {
        let rw = loop_price::cost(pricing, m.class, panes[k].ctx);
        w.money += rw;
        w.refusal_rw += rw;
        let p = &mut panes[k];
        p.left = Some((p.on, now));
        p.on = to;
        p.moved_at = Some(now);
        p.stalled = false;
    }
}

pub(super) fn rest(read: &Read, panes: &mut [P], k: usize, now: i64, policy: Policy, drv: &Driver, w: &mut Week) {
    let (st, dwell_h) = (&drv.st, drv.lim.dwell_h);
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
                switch(panes, k, to, now, drv.pricing, w);
            }
        }
        Policy::Planner { .. } => {
            let sid = k as u64;
            let Some(s) = st.switches.iter().find(|s| s.sid == sid && s.order == 0 && s.from == panes[k].on && s.open_at <= now) else { return };
            if panes[k].planned.last().is_some_and(|t| now - t < 6 * 3600) {
                w.double += 1;
            }
            if panes[k].planned.last().is_some_and(|t| ((now - t) as f64) < dwell_h * 3600.0) {
                w.short_stay += 1;
            }
            panes[k].planned.push(now);
            switch(panes, k, s.to, now, drv.pricing, w);
        }
    }
}

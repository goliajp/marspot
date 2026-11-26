//! One objective for where a pane goes: quota left unused at the
//! renewals, plus what the moves cost, in the same unit (weeks).
//!
//! Each account is run forward to its week's renewal at the speeds of
//! the panes on it: what is left of the week then lapses (`w7`), and
//! so does what is left of the restricted window (`wf`), even when the
//! week is spent and that window could not be used any more.  Counting
//! both makes a plain pane that eats the week the restricted panes on
//! an account would have used cost what those panes lose, and makes a
//! restricted pane on a restricted window that would lapse worth twice
//! a plain one there.  A pane being placed stays some hours around
//! `stay_h`, or until that account renews.
//!
//! A move costs the prompt cache it rewrites; a pane refused costs that
//! and a stall.  So a pane goes where it lowers the waste most, a pane
//! that has to move anyway to the cheapest place, and a resting pane
//! moves only when the waste it saves beats `lambda` times the cost of
//! its move by `margin`, at most `per_day` such moves a day.  Busy
//! panes are never asked: they move only when refused.

use super::choose::Mover;
use super::replay::{Kind, Why};
use super::rest::{BAND_COOLDOWN_H, BAND_PER_HOUR, MOVES_PER_WEEK, NO_RETURN_H};
use super::{Class, EPS, Load, Quant};

/// The prompt cache a restricted pane rewrites when it restarts on
/// another account, in weeks: the 0.365 points a move cost on the
/// control plane's records of 2026-11-17 to 11-24.
pub(crate) const SWITCH_RESTRICTED: f64 = 0.00365;

/// The same for a plain pane: 0.073 points.
pub(crate) const SWITCH_PLAIN: f64 = 0.00073;

/// What a refusal costs beyond the move it forces: the turn it breaks
/// and the wait until the pane runs again, in weeks.
pub(crate) const STALL: f64 = 0.01;

const WEEK_H: f64 = 168.0;

/// Two placements this close are equal; the earlier renewal wins.
const TIE: f64 = 1e-6;

/// How the objective is weighed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Params {
    /// Weight of a move's cost against the waste it saves.
    pub lambda: f64,
    /// What a resting move must save beyond its weighted cost, weeks.
    pub margin: f64,
    /// Resting moves of all panes in a day.
    pub per_day: usize,
    /// How long a placed pane is assumed to stay, hours.
    pub stay_h: f64,
}

/// The weights in force, chosen on the replay of 2026-11-25 and 11-26.
pub(crate) const PARAMS: Params = Params { lambda: 1.0, margin: 0.01, per_day: 8, stay_h: 48.0 };

pub(crate) fn switch_cost(class: Class) -> f64 {
    match class {
        Class::Restricted => SWITCH_RESTRICTED,
        Class::Plain => SWITCH_PLAIN,
    }
}

/// One account run forward to its week's renewal.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Outcome {
    /// Week left at the renewal.
    pub w7: f64,
    /// Restricted window left at its renewal, in weeks.
    pub wf: f64,
    /// Hours until it refuses a pane of the class watched.
    pub out_h: Option<f64>,
}

impl Outcome {
    pub(crate) fn waste(&self) -> f64 {
        self.w7 + self.wf
    }
}

struct Run {
    r: f64,
    f: f64,
    h: f64,
    watch: Class,
    out_h: Option<f64>,
}

impl Run {
    fn refuses(&self) -> bool {
        self.r <= EPS || (self.watch == Class::Restricted && self.f <= EPS)
    }

    /// `dur` hours at `a` restricted and `b` plain an hour: the
    /// restricted model is served while both its window and the week
    /// last, the others while the week does.
    fn run(&mut self, dur: f64, a: f64, b: f64) {
        let mut left = dur;
        while left > 1e-9 {
            if self.out_h.is_none() && self.refuses() {
                self.out_h = Some(self.h);
            }
            let fa = if self.f > EPS { a } else { 0.0 };
            let total = if self.r > EPS { fa + b } else { 0.0 };
            if total <= 0.0 {
                self.h += left;
                return;
            }
            let until_f = if fa > 0.0 { self.f / fa } else { f64::INFINITY };
            let d = left.min(self.r / total).min(until_f);
            self.r -= total * d;
            self.f -= fa * d;
            self.h += d;
            left -= d;
        }
        if self.out_h.is_none() && self.refuses() {
            self.out_h = Some(self.h);
        }
    }
}

/// Account `q` run forward with `load`, plus `extra` (a pane, for so
/// many hours) on top.  `kappa` refills a restricted window that renews
/// before the week; where it is unknown no restricted waste is counted.
pub(crate) fn forecast(q: &Quant, load: &Load, extra: Option<(Mover, f64)>, kappa: Option<f64>, watch: Class) -> Outcome {
    // no week is longer than this; a later renewal is a reading gone wrong
    let end = q.t.clamp(0.0, WEEK_H);
    let mut run = Run { r: q.r.max(0.0), f: q.f.max(0.0), h: 0.0, watch, out_h: None };
    let (mx, for_h) = extra.map_or((Load::default(), 0.0), |(m, h)| {
        let mut l = Load::default();
        l.add(m.class, m.rate);
        (l, h.clamp(0.0, end))
    });
    // the restricted window renewing well before the week is refilled then
    let split = q.tf < q.t - 1.0 && q.tf > 0.0;
    let mut cuts = vec![for_h, end];
    if split {
        cuts.push(q.tf);
    }
    cuts.sort_by(f64::total_cmp);
    cuts.dedup();
    let mut wf = 0.0;
    let mut at = 0.0;
    for cut in cuts {
        if cut > at {
            let on = at < for_h;
            run.run(cut - at, load.restricted + if on { mx.restricted } else { 0.0 }, load.plain + if on { mx.plain } else { 0.0 });
            at = cut;
        }
        if split && (cut - q.tf).abs() < 1e-12 {
            wf += run.f;
            run.f = kappa.unwrap_or(0.0);
        }
    }
    // a window renewing after the week may still be used after it
    if !split && q.tf <= q.t + 1.0 {
        wf += run.f;
    }
    let wf = if kappa.is_some() { wf } else { 0.0 };
    Outcome { w7: run.r, wf, out_h: run.out_h }
}

/// How long a placed pane stays is not known: the cost is the mean over
/// half, once and twice the typical stay.
const STAYS: [f64; 3] = [0.5, 1.0, 2.0];

/// What mover `m` adds to account `q`'s cost: the waste with it there
/// against without it, over the stays around `stay_h`, and a refusal if
/// `q` refuses it within `horizon_h`.
pub(crate) fn added(q: &Quant, load: &Load, m: Mover, kappa: Option<f64>, horizon_h: f64, stay_h: f64) -> f64 {
    let without = forecast(q, load, None, kappa, m.class).waste();
    let mut sum = 0.0;
    for k in STAYS {
        let with = forecast(q, load, Some((m, stay_h * k)), kappa, m.class);
        let refused = with.out_h.is_some_and(|h| h < horizon_h);
        sum += with.waste() - without + if refused { STALL + switch_cost(m.class) } else { 0.0 };
    }
    sum / STAYS.len() as f64
}

/// The account that costs least with `m` on it, among those that take
/// it, are not `from` and pass `allow`; stale readings after fresh
/// ones, ties to the earlier renewal for its class, then the number.
#[allow(clippy::too_many_arguments)] // one choice's inputs, each a separate piece
pub(crate) fn pick(accounts: &[Quant], loads: &[Load], m: Mover, from: Option<u8>, kappa: Option<f64>, horizon_h: f64, p: &Params, allow: impl Fn(&Quant) -> bool) -> Option<u8> {
    let costs: Vec<(usize, f64)> = accounts
        .iter()
        .enumerate()
        .filter(|(_, q)| Some(q.id) != from && q.accepts(m.class) && allow(q))
        .map(|(i, q)| (i, added(q, &loads[i], m, kappa, horizon_h, p.stay_h)))
        .collect();
    best(accounts, &costs, m.class, |c| c)
}

/// The lowest of `costs` by `key`, fresh before stale, near ties to the
/// earlier renewal for `class`, then the number.
fn best(accounts: &[Quant], costs: &[(usize, f64)], class: Class, key: impl Fn(f64) -> f64) -> Option<u8> {
    let stale_last = |i: usize| accounts[i].stale;
    let low = costs.iter().map(|(i, c)| (stale_last(*i), key(*c))).min_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)))?;
    costs
        .iter()
        .filter(|(i, c)| stale_last(*i) == low.0 && key(*c) - low.1 < TIE)
        .min_by(|(a, _), (b, _)| accounts[*a].renews_in(class).total_cmp(&accounts[*b].renews_in(class)).then(accounts[*a].id.cmp(&accounts[*b].id)))
        .map(|(i, _)| accounts[*i].id)
}

/// The decision of `Inputs::decide_by` under this objective;
/// `moves_day` are the resting moves of all panes in the last day.
#[allow(clippy::too_many_arguments)] // one decision's inputs, each a separate piece
pub(crate) fn decide(accounts: &[Quant], loads: &[Load], m: Mover, here: u8, kappa: Option<f64>, kind: &Kind, moves_day: usize, p: &Params) -> Option<(u8, Why)> {
    match kind {
        Kind::Out { horizon_h } => pick(accounts, loads, m, Some(here), kappa, *horizon_h, p, |_| true).map(|t| (t, Why::Out)),
        Kind::New { horizon_h } => pick(accounts, loads, m, None, kappa, *horizon_h, p, |_| true).map(|t| (t, Why::Out)),
        Kind::Rest { since_move_h, left, band_last_hour, stale_feed, keeps_restricted, moves_week, .. } => {
            let limited = *stale_feed || *moves_week >= MOVES_PER_WEEK || moves_day >= p.per_day || *band_last_hour >= BAND_PER_HOUR;
            if limited || since_move_h.is_some_and(|h| h < BAND_COOLDOWN_H) {
                return None;
            }
            let returning = |id: u8| left.iter().any(|(l, h)| *l == id && *h < NO_RETURN_H);
            let allow = |q: &Quant| !q.stale && !returning(q.id) && !(*keeps_restricted && m.class == Class::Plain && q.phi() <= EPS);
            rest(accounts, loads, m, here, kappa, p, allow).map(|t| (t, Why::Waste))
        }
    }
}

/// Where a resting pane goes, if moving saves more than it costs.
fn rest(accounts: &[Quant], loads: &[Load], m: Mover, here: u8, kappa: Option<f64>, p: &Params, allow: impl Fn(&Quant) -> bool) -> Option<u8> {
    use super::choose::MUST_LAST_H;
    let hi = accounts.iter().position(|q| q.id == here)?;
    let mut others = loads.to_vec();
    others[hi].add(m.class, -m.rate);
    let stays = added(&accounts[hi], &others[hi], m, kappa, MUST_LAST_H, p.stay_h);
    let cost = p.lambda * switch_cost(m.class);
    let gains: Vec<(usize, f64)> = accounts
        .iter()
        .enumerate()
        .filter(|(i, q)| *i != hi && q.accepts(m.class) && allow(q))
        .map(|(i, q)| (i, stays - added(q, &others[i], m, kappa, MUST_LAST_H, p.stay_h) - cost))
        .filter(|(_, g)| *g > p.margin)
        .collect();
    best(accounts, &gains, m.class, |g| -g)
}

/// A helper started with no model asked for runs the restricted model
/// when that lowers the expected waste more than the other models do:
/// some restricted window would lapse.  Never applied to a pane the
/// person started.
pub(crate) fn restricted_helper(accounts: &[Quant], loads: &[Load], rate: f64, kappa: Option<f64>, horizon_h: f64, p: &Params) -> bool {
    let low = |class: Class| {
        let m = Mover { class, rate };
        accounts.iter().enumerate().filter(|(_, q)| q.accepts(class)).map(|(i, q)| added(q, &loads[i], m, kappa, horizon_h, p.stay_h)).fold(f64::INFINITY, f64::min)
    };
    kappa.is_some() && low(Class::Restricted) < low(Class::Plain) - TIE
}

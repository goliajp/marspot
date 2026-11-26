//! One objective for where a pane goes: quota left unused at the
//! renewals, plus what the moves cost, in the same unit (weeks).
//!
//! Every account is run forward over one span, the same for all of
//! them, at the speeds of the panes on it.  At each renewal inside the
//! span what is left of the week lapses (`w7`) and the account starts
//! a new one; at each renewal of the restricted window what is left of
//! it lapses (`wf`), even when the week is spent and that window could
//! not be used any more.  The span is the longest stay a placed pane is
//! assumed to make and a week after it, so every account renews after
//! the pane has left and all it used shows in some lapse: an account
//! renewing early is not cheaper because its next week falls outside.
//!
//! Counting both lapses makes a plain pane that eats the week the
//! restricted panes on an account would have used cost what those
//! panes lose, and makes a restricted pane on a restricted window that
//! would lapse worth twice a plain one there.
//!
//! A move costs the prompt cache it rewrites, which grows with the
//! pane's context; a pane its new account refuses before it would have
//! left costs that again and a stall.  A pane that has to move, or a
//! new one, goes first to an account that neither refuses it nor
//! starts refusing the panes already there within its horizon, and
//! among those to the cheapest.  A resting pane moves only when the
//! waste it saves beats `lambda` times its move by `margin`, at most
//! `per_day` such moves a day.  Busy panes are never asked: they move
//! only when refused.

use super::choose::{MUST_LAST_H, Mover};
use super::replay::{Kind, Why};
use super::rest::{BAND_COOLDOWN_H, BAND_PER_HOUR, MOVES_PER_WEEK, NO_RETURN_H};
use super::{Class, EPS, Load, Quant};

/// The prompt cache a restricted pane rewrites when it restarts on
/// another account, in weeks: the 0.365 points a move cost on the
/// control plane's records of 2026-11-17 to 11-24.
pub const SWITCH_RESTRICTED: f64 = 0.00365;

/// The same for a plain pane: 0.073 points.
pub const SWITCH_PLAIN: f64 = 0.00073;

/// The context the two costs above stand for, tokens: the median of a
/// session's context at the end of its turns on this machine.
pub const SWITCH_TOKENS: f64 = 460_000.0;

/// What a refusal costs beyond the move it forces: the turn it breaks
/// and the wait until the pane runs again, in weeks.
pub const STALL: f64 = 0.01;

const WEEK_H: f64 = 168.0;

/// No span is longer: a stay assumed longer than two weeks is cut.
const MAX_SPAN_H: f64 = 3.0 * WEEK_H;

/// Two placements this close are equal; the earlier renewal wins.
const TIE: f64 = 1e-6;

/// How the objective is weighed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    /// Weight of a move's cost against the waste it saves.
    pub lambda: f64,
    /// What a resting move must save beyond its weighted cost, weeks;
    /// twice this for a pane whose speed was not measured.
    pub margin: f64,
    /// Resting moves of all panes in a day.
    pub per_day: usize,
    /// How long a placed pane is assumed to stay, hours.
    pub stay_h: f64,
}

/// The weights in force, chosen on the replay of 2026-11-25 and 11-26:
/// any `lambda` from 0.5 to 4 and `margin` up to 0.01 gave the same
/// moves there, since a move's cache is small against what it saves.
pub const PARAMS: Params = Params { lambda: 1.0, margin: 0.01, per_day: 8, stay_h: 12.0 };

/// What moving a pane of `class` costs in rewritten prompt cache, for a
/// context of `tokens`; `None`, where the context is not known, is the
/// typical one.
pub fn switch_cost(class: Class, tokens: Option<u64>) -> f64 {
    let typical = match class {
        Class::Restricted => SWITCH_RESTRICTED,
        Class::Plain => SWITCH_PLAIN,
    };
    tokens.map_or(typical, |t| typical * t as f64 / SWITCH_TOKENS)
}

/// One account run forward over a span.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Outcome {
    /// Week lapsing at the renewals inside the span.
    pub w7: f64,
    /// Restricted window lapsing at its renewals inside the span, weeks.
    pub wf: f64,
    /// Hours until it first refuses a plain pane, within the span.
    pub out_plain: Option<f64>,
    /// The same for a restricted pane.
    pub out_restricted: Option<f64>,
    /// Week the extra pane used; a restricted pane used as much of its
    /// window.
    pub extra_used: f64,
}

impl Outcome {
    pub fn waste(&self) -> f64 {
        self.w7 + self.wf
    }

    pub fn out(&self, class: Class) -> Option<f64> {
        match class {
            Class::Plain => self.out_plain,
            Class::Restricted => self.out_restricted,
        }
    }
}

struct Run {
    r: f64,
    f: f64,
    h: f64,
    o: Outcome,
}

impl Run {
    fn note_refusals(&mut self) {
        if self.r <= EPS {
            self.o.out_plain.get_or_insert(self.h);
        }
        if self.r <= EPS || self.f <= EPS {
            self.o.out_restricted.get_or_insert(self.h);
        }
    }

    /// `dur` hours at `a` restricted and `b` plain an hour, of which the
    /// extra pane is `x`: the restricted model is served while both its
    /// window and the week last, the others while the week does.
    fn run(&mut self, dur: f64, a: f64, b: f64, x: Load) {
        let mut left = dur;
        while left > 1e-9 {
            self.note_refusals();
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
            self.o.extra_used += (if fa > 0.0 { x.restricted } else { 0.0 } + x.plain) * d;
            self.h += d;
            left -= d;
        }
        self.note_refusals();
    }
}

/// Account `q` run forward `span_h` hours with `load`, plus `extra` (a
/// pane, for so many hours or until the account refuses it) on top.
/// `kappa` refills a restricted window at its renewal; where it is
/// unknown no restricted waste is counted and the renewed window only
/// says the model is served.
pub fn forecast(q: &Quant, load: &Load, extra: Option<(Mover, f64)>, kappa: Option<f64>, span_h: f64) -> Outcome {
    let span = span_h.clamp(0.0, MAX_SPAN_H);
    let mut run = Run { r: q.r.max(0.0), f: q.f.max(0.0), h: 0.0, o: Outcome::default() };
    let (mut mx, mut for_h) = (Load::default(), extra.map_or(0.0, |(_, h)| h.clamp(0.0, span)));
    if let Some((m, _)) = extra {
        mx.add(m.class, m.rate);
    }
    // no week is longer than this; a later renewal is a reading gone wrong
    let mut week = q.t.clamp(0.0, WEEK_H);
    let mut window = q.tf.clamp(0.0, WEEK_H);
    let mut at = 0.0;
    loop {
        let next = week.min(window).min(span).min(if at < for_h { for_h } else { f64::INFINITY });
        let on = at < for_h;
        let x = if on { mx } else { Load::default() };
        run.run(next - at, load.restricted + x.restricted, load.plain + x.plain, x);
        at = next;
        // a pane refused leaves for another account: a renewal here does
        // not bring it back
        if let Some((m, _)) = extra
            && on
            && run.o.out(m.class).is_some()
        {
            for_h = for_h.min(at);
        }
        if week <= at {
            run.o.w7 += run.r;
            run.r = 1.0;
            week += WEEK_H;
        }
        if window <= at {
            run.o.wf += run.f;
            run.f = kappa.unwrap_or(1.0);
            window += WEEK_H;
        }
        if at >= span {
            break;
        }
    }
    if kappa.is_none() {
        run.o.wf = 0.0;
    }
    run.o
}

/// How long a placed pane stays is not known: the cost is the mean over
/// half, once and twice the typical stay.
const STAYS: [f64; 3] = [0.5, 1.0, 2.0];

/// The span every placement is compared over: the longest stay, then a
/// week in which each account renews once more.
pub fn span_h(p: &Params) -> f64 {
    p.stay_h * STAYS[STAYS.len() - 1] + WEEK_H
}

/// What a pane adds to one account.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Added {
    /// The account refuses the pane within the horizon.
    pub refuses: bool,
    /// The waste with the pane there against without it, over the stays
    /// around `stay_h`, and a second move and a stall for each stay it
    /// would be refused before its end, and for each class of the panes
    /// there it would have refused by then that would not be otherwise.
    pub cost: f64,
}

/// What mover `m` adds to account `q` with `load` on it already.
pub fn added(q: &Quant, load: &Load, m: Mover, kappa: Option<f64>, horizon_h: f64, p: &Params) -> Added {
    let span = span_h(p);
    let without = forecast(q, load, None, kappa, span);
    let mut sum = 0.0;
    for k in STAYS {
        let stay = p.stay_h * k;
        let with = forecast(q, load, Some((m, stay)), kappa, span);
        let again = with.out(m.class).is_some_and(|h| h < stay);
        sum += with.waste() - without.waste() + if again { STALL + switch_cost(m.class, m.tokens) } else { 0.0 };
        // the panes there it has refused before it leaves move too
        for c in [Class::Plain, Class::Restricted] {
            let before = |o: &Outcome| o.out(c).is_some_and(|h| h < stay);
            if load.of(c) > EPS && before(&with) && !before(&without) {
                sum += STALL + switch_cost(c, None);
            }
        }
    }
    // every class draws on the week, and the restricted ones on one
    // window: whatever refuses a pane already there within the horizon
    // refuses this one no later, so its own refusal covers theirs
    let held = forecast(q, load, Some((m, horizon_h)), kappa, span);
    Added { refuses: held.out(m.class).is_some_and(|h| h < horizon_h), cost: sum / STAYS.len() as f64 }
}

/// The candidate that comes first among `costs` (index, cost, refuses),
/// lowest `key` of the cost: fresh before stale, then accounts that do
/// not refuse, then the key; near ties to the earlier renewal for the
/// mover's class, then to the account that lasts it longest with
/// `loads` on it, then the number.
fn best(accounts: &[Quant], loads: &[Load], m: Mover, costs: &[(usize, f64, bool)], key: impl Fn(f64) -> f64) -> Option<usize> {
    let rank = |i: usize, refuses: bool| (accounts[i].stale, refuses);
    let low = costs.iter().map(|(i, c, x)| (rank(*i, *x), key(*c))).min_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)))?;
    costs
        .iter()
        .filter(|(i, c, x)| rank(*i, *x) == low.0 && key(*c) - low.1 < TIE)
        .min_by(|(a, ..), (b, ..)| {
            let (qa, qb) = (&accounts[*a], &accounts[*b]);
            let lasts = |i: usize| super::lasts(&accounts[i], &loads[i], m.class, m.rate);
            qa.renews_in(m.class).total_cmp(&qb.renews_in(m.class)).then(lasts(*b).total_cmp(&lasts(*a))).then(qa.id.cmp(&qb.id))
        })
        .map(|(i, ..)| *i)
}

/// The account `pick` chooses and what `m` adds to it.
#[allow(clippy::too_many_arguments)] // one choice's inputs, each a separate piece
pub fn placed(accounts: &[Quant], loads: &[Load], m: Mover, from: Option<u8>, kappa: Option<f64>, horizon_h: f64, p: &Params, allow: impl Fn(&Quant) -> bool) -> Option<(u8, Added)> {
    let adds: Vec<(usize, Added)> = accounts
        .iter()
        .enumerate()
        .filter(|(_, q)| Some(q.id) != from && q.accepts(m.class) && allow(q))
        .map(|(i, q)| (i, added(q, &loads[i], m, kappa, horizon_h, p)))
        .collect();
    let costs: Vec<(usize, f64, bool)> = adds.iter().map(|(i, a)| (*i, a.cost, a.refuses)).collect();
    let i = best(accounts, loads, m, &costs, |c| c)?;
    adds.iter().find(|(j, _)| *j == i).map(|(_, a)| (accounts[i].id, *a))
}

/// Where a pane that has to move, or a new one, goes: among the
/// accounts that take it, are not `from` and pass `allow`, those that
/// neither refuse it nor start refusing the panes there within
/// `horizon_h` first, the cheapest of them; stale readings after fresh.
#[allow(clippy::too_many_arguments)] // one choice's inputs, each a separate piece
pub fn pick(accounts: &[Quant], loads: &[Load], m: Mover, from: Option<u8>, kappa: Option<f64>, horizon_h: f64, p: &Params, allow: impl Fn(&Quant) -> bool) -> Option<u8> {
    placed(accounts, loads, m, from, kappa, horizon_h, p, allow).map(|(id, _)| id)
}

/// The decision of `Inputs::decide_by` under this objective;
/// `moves_day` are the resting moves of all panes in the last day.
///
/// Of what a resting pane's inputs carry, the total demand is not
/// read: whether the accounts are short shows in the forecast itself.
#[allow(clippy::too_many_arguments)] // one decision's inputs, each a separate piece
pub fn decide(accounts: &[Quant], loads: &[Load], m: Mover, here: u8, kappa: Option<f64>, kind: &Kind, moves_day: usize, p: &Params) -> Option<(u8, Why)> {
    match kind {
        Kind::Out { horizon_h } => pick(accounts, loads, m, Some(here), kappa, *horizon_h, p, |_| true).map(|t| (t, Why::Out)),
        Kind::New { horizon_h } => pick(accounts, loads, m, None, kappa, *horizon_h, p, |_| true).map(|t| (t, Why::Out)),
        Kind::Rest { keeper, since_move_h, left, band_last_hour, stale_feed, speed_known, keeps_restricted, moves_week, .. } => {
            let limited = *stale_feed || *moves_week >= MOVES_PER_WEEK || moves_day >= p.per_day || *band_last_hour >= BAND_PER_HOUR;
            if limited || since_move_h.is_some_and(|h| h < BAND_COOLDOWN_H) {
                return None;
            }
            let hi = accounts.iter().position(|q| q.id == here)?;
            if *keeper && keeps_the_rest(&accounts[hi], &loads[hi], m.class, kappa) {
                return None;
            }
            let returning = |id: u8| left.iter().any(|(l, h)| *l == id && *h < NO_RETURN_H);
            let allow = |q: &Quant| !q.stale && !returning(q.id) && !(*keeps_restricted && m.class == Class::Plain && q.phi() <= EPS);
            let margin = if *speed_known { p.margin } else { 2.0 * p.margin };
            rest(accounts, loads, m, hi, kappa, &Params { margin, ..*p }, allow).map(|t| (t, Why::Waste))
        }
    }
}

/// The slowest pane of its class on an account stays while, without
/// any pane of that class, part of the account would lapse at its next
/// renewal: the panes of the other class would not use it up.
fn keeps_the_rest(q: &Quant, load: &Load, class: Class, kappa: Option<f64>) -> bool {
    let mut rest = *load;
    rest.add(class, -load.of(class));
    let o = forecast(q, &rest, None, kappa, q.renews_in(class));
    match class {
        Class::Plain => o.w7 > EPS,
        Class::Restricted => o.waste() > EPS,
    }
}

/// Where the resting pane on `accounts[hi]` goes, if moving saves more
/// than it costs; `loads` count the pane where it is.
fn rest(accounts: &[Quant], loads: &[Load], m: Mover, hi: usize, kappa: Option<f64>, p: &Params, allow: impl Fn(&Quant) -> bool) -> Option<u8> {
    let mut others = loads.to_vec();
    others[hi].add(m.class, -m.rate);
    let stays = added(&accounts[hi], &others[hi], m, kappa, MUST_LAST_H, p).cost;
    let cost = p.lambda * switch_cost(m.class, m.tokens);
    let gains: Vec<(usize, f64, bool)> = accounts
        .iter()
        .enumerate()
        .filter(|(i, q)| *i != hi && q.accepts(m.class) && allow(q))
        .map(|(i, q)| (i, added(q, &others[i], m, kappa, MUST_LAST_H, p)))
        .filter(|(_, a)| !a.refuses)
        .map(|(i, a)| (i, stays - a.cost - cost, false))
        .filter(|(_, g, _)| *g > p.margin)
        .collect();
    best(accounts, &others, m, &gains, |g| -g).map(|i| accounts[i].id)
}

/// What mover `m` on `q` takes from the other panes: the week (and for
/// a restricted pane the window) it uses that would not have lapsed,
/// over the stays around `p.stay_h`, with a second move and a stall for
/// each stay it would be refused before its end.
fn displaced(q: &Quant, load: &Load, m: Mover, kappa: Option<f64>, p: &Params) -> f64 {
    let span = span_h(p);
    let without = forecast(q, load, None, kappa, span);
    let mut sum = 0.0;
    for k in STAYS {
        let stay = p.stay_h * k;
        let with = forecast(q, load, Some((m, stay)), kappa, span);
        let mut taken = with.extra_used - (without.w7 - with.w7);
        if m.class == Class::Restricted {
            taken += with.extra_used - (without.wf - with.wf);
        }
        let again = with.out(m.class).is_some_and(|h| h < stay);
        sum += taken + if again { STALL + switch_cost(m.class, None) } else { 0.0 };
    }
    sum / STAYS.len() as f64
}

/// A helper started with no model asked for runs the restricted model
/// when, on the account a restricted helper would be placed on, it takes
/// no more from the other panes than a plain helper would on its own:
/// the same work costs the restricted model `restricted` of the week an
/// hour and the others `plain`, so what it uses has to be what would
/// lapse.  The restricted model is the better one, so a tie goes to it.
/// `p.stay_h` is how long the helper is expected to run.  Never applied
/// to a pane the person started.
#[allow(clippy::too_many_arguments)] // one decision's inputs, each a separate piece
pub fn restricted_helper(accounts: &[Quant], loads: &[Load], plain: f64, restricted: f64, kappa: Option<f64>, horizon_h: f64, p: &Params) -> bool {
    if kappa.is_none() {
        return false;
    }
    let at = |class: Class, rate: f64| {
        let m = Mover { class, rate, tokens: None };
        let (id, a) = placed(accounts, loads, m, None, kappa, horizon_h, p, |_| true)?;
        let i = accounts.iter().position(|q| q.id == id)?;
        Some((accounts[i].stale, a.refuses, displaced(&accounts[i], &loads[i], m, kappa, p)))
    };
    match (at(Class::Restricted, restricted), at(Class::Plain, plain)) {
        (Some(r), Some(n)) => (r.0, r.1).cmp(&(n.0, n.1)).then(r.2.total_cmp(&(n.2 + TIE))).is_le(),
        (Some(_), None) => true,
        _ => false,
    }
}

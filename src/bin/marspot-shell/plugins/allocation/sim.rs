//! A week of panes and accounts, to compare placement rules.
//!
//! Fifteen-minute steps over 168 hours from the 2026-11-24 14:30Z
//! readings.  Panes use quota at their own (lognormal) speeds; the
//! rules see each speed times a bias.  On each step a pane rests, and
//! may be moved without being refused, with probability `p_rest`.  Use
//! is capped at what the account has left; the shortfall is unmet
//! demand, and a pane whose account no longer takes it counts one
//! refusal and is moved by the rule.  Every move costs `move_cost` on
//! the target, the first turn rewriting the cache.  Deterministic for a
//! seed.

use super::sim_policies::{Policy, pick, rest};
use super::{Class, KAPPA_FABLE, Quant};

pub(super) const STEP: f64 = 0.25;
pub(super) const WEEK_H: f64 = 168.0;
pub(super) const NOW: i64 = 1_795_530_644;

/// `(account, week used, Fable used, week renews at)`, 2026-11-24 14:30:44Z.
pub(super) const FEED: [(u8, f64, f64, i64); 8] = [
    (1, 1.00, 0.95, 1_795_748_400),
    (2, 0.74, 1.00, 1_795_985_999),
    (3, 0.50, 0.00, 1_795_928_399),
    (4, 0.00, 0.00, 1_795_791_600),
    (5, 1.00, 1.00, 1_795_658_399),
    (6, 0.05, 0.09, 1_795_755_600),
    (7, 0.56, 1.00, 1_796_097_600),
    (8, 1.00, 1.00, 1_795_582_799),
];

/// The accounts at those readings, a Fable window worth `kappa` of a week.
pub(super) fn world_at(kappa: f64) -> Vec<Quant> {
    FEED.iter()
        .map(|&(id, w, f, reset)| {
            let t = (reset - NOW) as f64 / 3600.0;
            Quant { id, r: 1.0 - w, f: kappa * (1.0 - f), t, tf: t, open: true, stale: false }
        })
        .collect()
}

/// splitmix64, with a Box–Muller normal.
pub(super) struct Rng(u64);

impl Rng {
    pub(super) fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03)
    }

    pub(super) fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    }

    fn gauss(&mut self) -> f64 {
        let u = self.next().max(1e-12);
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * self.next()).cos()
    }
}

#[derive(Clone, Debug)]
pub(super) struct SimPane {
    pub class: Class,
    pub rho: f64,
    pub est: f64,
    pub on: Option<u8>,
    pub last: Option<f64>,
    pub left: Vec<(u8, f64)>,
    /// Hours it exists, `[from, until)`: helpers come in batches.
    pub alive: (f64, f64),
    pub rest_moves: u32,
}

impl SimPane {
    pub(super) fn live(&self, t: f64) -> bool {
        self.alive.0 <= t && t < self.alive.1
    }
}

/// A batch of `n` plain panes at `rate` each, alive `[from, until)`.
#[derive(Clone, Copy, Debug)]
pub(super) struct Batch {
    pub n: usize,
    pub rate: f64,
    pub from: f64,
    pub until: f64,
}

#[derive(Clone, Debug)]
pub(super) struct Case {
    pub n_f: usize,
    pub n_n: usize,
    pub demand: f64,
    pub fable_share: f64,
    pub bias: f64,
    pub sigma: f64,
    pub p_rest: f64,
    pub move_cost: f64,
    /// Plain panes that stop at this hour, from the end of the list.
    pub leave: Option<(usize, f64)>,
    pub batches: Vec<Batch>,
    /// What a Fable window is worth, for the world and the planners.
    pub kappa: f64,
}

impl Case {
    pub(super) fn base(n_f: usize, n_n: usize, demand: f64) -> Self {
        Case { n_f, n_n, demand, fable_share: 0.5, bias: 1.0, sigma: 0.0, p_rest: 0.4, move_cost: 0.0, leave: None, batches: Vec::new(), kappa: KAPPA_FABLE }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Outcome {
    /// Points.
    pub unmet: f64,
    /// The restricted panes' part of it, points.
    pub unmet_restricted: f64,
    pub refusals: u32,
    pub forced: u32,
    pub rest: u32,
    /// Weeks lapsed at renewals.
    pub waste: f64,
    /// The most resting moves of any one pane.
    pub most_rest_per_pane: u32,
}

fn panes_of(c: &Case, rng: &mut Rng) -> Vec<SimPane> {
    let mut rates = |n: usize, total: f64| -> Vec<f64> {
        let w: Vec<f64> = (0..n).map(|_| (c.sigma * rng.gauss()).exp()).collect();
        let s: f64 = w.iter().sum();
        w.into_iter().map(|x| total * x / s).collect()
    };
    let mut out = Vec::new();
    let f = rates(c.n_f, c.demand * c.fable_share);
    let n = rates(c.n_n, c.demand * (1.0 - c.fable_share));
    let pane = |class, rho: f64, alive| SimPane { class, rho, est: rho * c.bias, on: None, last: None, left: Vec::new(), alive, rest_moves: 0 };
    out.extend(f.into_iter().map(|r| pane(Class::Restricted, r, (0.0, f64::INFINITY))));
    let plain_from = out.len();
    out.extend(n.into_iter().map(|r| pane(Class::Plain, r, (0.0, f64::INFINITY))));
    if let Some((k, at)) = c.leave {
        for p in out[plain_from..].iter_mut().rev().take(k) {
            p.alive.1 = at;
        }
    }
    for b in &c.batches {
        out.extend((0..b.n).map(|_| pane(Class::Plain, b.rate, (b.from, b.until))));
    }
    out
}

fn charge(a: &mut [Quant], to: u8, class: Class, cost: f64) {
    if let Some(q) = a.iter_mut().find(|q| q.id == to) {
        q.r -= cost;
        if class == Class::Restricted {
            q.f -= cost;
        }
    }
}

pub(super) fn run(policy: Policy, c: &Case, seed: u64) -> Outcome {
    let mut rng = Rng::new(seed);
    let mut a = world_at(c.kappa);
    let mut panes = panes_of(c, &mut rng);
    let mut o = Outcome::default();
    let mut hour: Vec<f64> = Vec::new();
    let mut t = 0.0;
    while t < WEEK_H {
        for q in &mut a {
            if q.t <= STEP / 2.0 {
                o.waste += q.r.max(0.0);
                q.r = 1.0;
                q.f = c.kappa;
                q.t += 168.0;
                q.tf = q.t;
            }
        }
        place_and_force(policy, c, &mut a, &mut panes, t, &mut o);
        hour.retain(|x| t - x < 1.0);
        rest_pass(policy, c, &mut a, &mut panes, t, &mut rng, &mut hour, &mut o);
        use_step(&mut a, &panes, t, &mut o);
        for q in &mut a {
            q.t -= STEP;
            q.tf = q.t;
        }
        t += STEP;
    }
    o.unmet = (o.unmet * 100.0 * 10.0).round() / 10.0;
    o.unmet_restricted = (o.unmet_restricted * 100.0 * 10.0).round() / 10.0;
    o.most_rest_per_pane = panes.iter().map(|p| p.rest_moves).max().unwrap_or(0);
    o
}

fn place_and_force(policy: Policy, c: &Case, a: &mut [Quant], panes: &mut [SimPane], t: f64, o: &mut Outcome) {
    for i in 0..panes.len() {
        if !panes[i].live(t) {
            panes[i].on = None;
            continue;
        }
        let here = panes[i].on.and_then(|id| a.iter().find(|q| q.id == id));
        if here.is_some_and(|q| q.accepts(panes[i].class)) {
            continue;
        }
        let was = panes[i].on;
        let starting = was.is_none() && panes[i].last.is_none();
        if was.is_some() {
            o.refusals += 1;
        }
        let Some(to) = pick(policy, a, panes, i, was, t, c.kappa) else {
            panes[i].on = None;
            continue;
        };
        if starting {
            panes[i].on = Some(to);
            panes[i].last = Some(f64::NEG_INFINITY);
            continue;
        }
        if let Some(w) = was {
            o.forced += 1;
            panes[i].left.push((w, t));
        }
        charge(a, to, panes[i].class, c.move_cost);
        panes[i].on = Some(to);
        panes[i].last = Some(t);
    }
}

#[allow(clippy::too_many_arguments)] // the step's whole state, each a separate piece
fn rest_pass(policy: Policy, c: &Case, a: &mut [Quant], panes: &mut [SimPane], t: f64, rng: &mut Rng, hour: &mut Vec<f64>, o: &mut Outcome) {
    let targets = super::sim_policies::targets(policy, a, panes, t, c.kappa);
    for i in 0..panes.len() {
        if panes[i].on.is_none() || !panes[i].live(t) || rng.next() > c.p_rest {
            continue;
        }
        let Some((to, urgent)) = rest(policy, a, panes, i, t, targets.as_ref(), hour.len()) else { continue };
        let from = panes[i].on.unwrap_or(to);
        panes[i].left.push((from, t));
        panes[i].on = Some(to);
        panes[i].last = Some(t);
        panes[i].rest_moves += 1;
        o.rest += 1;
        charge(a, to, panes[i].class, c.move_cost);
        if !urgent {
            hour.push(t);
        }
        if policy == Policy::Old {
            break;
        }
    }
}

fn use_step(a: &mut [Quant], panes: &[SimPane], t: f64, o: &mut Outcome) {
    for p in panes.iter().filter(|p| p.live(t)) {
        let want = p.rho * STEP;
        let restricted = p.class == Class::Restricted;
        let Some(q) = p.on.and_then(|id| a.iter_mut().find(|q| q.id == id)) else {
            o.unmet += want;
            if restricted {
                o.unmet_restricted += want;
            }
            continue;
        };
        let mut u = want.min(q.r.max(0.0));
        if p.class == Class::Restricted {
            u = u.min(q.f.max(0.0));
            q.f -= u;
        }
        q.r -= u;
        o.unmet += want - u;
        if restricted {
            o.unmet_restricted += want - u;
        }
    }
}

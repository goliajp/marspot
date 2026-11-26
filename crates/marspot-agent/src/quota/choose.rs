//! Where a pane that has to move goes: rules (a), (c) and (d).
//!
//! (a) a pane refused, or on an account spent for its class; (c) a new
//! helper, the same rule with a shorter horizon; (d) a pane marked as
//! needing the restricted model is judged as one, so the adapter hands
//! it in as `Class::Restricted`.
//!
//! Order: accounts that last long enough with this pane on them first
//! (for a plain pane, counted on `U` before `R`, so what the restricted
//! model cannot use goes first); among those, accounts short of their
//! target by at least half this pane, earliest renewal first; then the
//! one that lasts longest; then the number.  Stale readings come after
//! every fresh one.  While the restricted demand is more than the
//! accounts can carry, a plain pane being placed goes only where what
//! only the other models can use (`N`) lasts it six hours, and waits
//! otherwise; a pane leaving early avoids only accounts where `U` lasts
//! it under an hour.

use super::plan::Plan;
use super::{Class, EPS, Load, Quant, lasts_of, plain_lasts};

/// How long a must-move target has to last with the pane on it.
pub const MUST_LAST_H: f64 = 6.0;

/// The same for a new helper.
pub const HELPER_LAST_H: f64 = 2.0;

/// While the restricted model is short, a plain pane leaving early is
/// placed only where `U` gives it at least this many hours.
pub const RESERVED_H: f64 = 1.0;

/// What a plain pane may not take while the restricted model is short.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reserve {
    /// Nothing held back.
    None,
    /// An account where `U` lasts it under `RESERVED_H`.
    U,
    /// An account where `N`, what only the other models can use, lasts
    /// it under `MUST_LAST_H`: the rule for a pane being placed, (a) and
    /// (c), which waits rather than take what the restricted panes are
    /// short of.
    N,
}

impl Reserve {
    fn holds(self, q: &Quant, plan: &Plan, i: usize, load: &Load, m: Mover) -> bool {
        if m.class != Class::Plain || plan.unassigned_f <= EPS {
            return false;
        }
        match self {
            Reserve::None => false,
            Reserve::U => plain_lasts(plan.u[i], load, m.b()) < RESERVED_H,
            Reserve::N => plain_lasts(q.n(), load, m.b()) < MUST_LAST_H,
        }
    }
}

/// The pane being placed.
///
/// `class` is where it may sit: a pane that needs the restricted model
/// only goes where that model has room.  What it draws from each window
/// is its two speeds: `a` from the restricted window and the week, `b`
/// from the week only.  A restricted session whose subagents run another
/// model draws mostly `b`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mover {
    pub class: Class,
    /// What it is believed to use an hour, both speeds together.
    pub rate: f64,
    /// Its context, which a move rewrites into the new account's prompt
    /// cache; `None` where it is not known.
    pub tokens: Option<u64>,
    /// Of `rate`, what it uses of the restricted model; `None` where the
    /// two are not told apart, and then all of `rate` is its class's.
    pub fable: Option<f64>,
}

impl Mover {
    /// Its restricted speed, `a`.
    pub fn a(&self) -> f64 {
        match (self.fable, self.class) {
            (Some(a), _) => a.clamp(0.0, self.rate.max(0.0)),
            (None, Class::Restricted) => self.rate,
            (None, Class::Plain) => 0.0,
        }
    }

    /// Its speed on the other models, `b`.
    pub fn b(&self) -> f64 {
        self.rate - self.a()
    }

    /// Its speed of `class`.
    pub fn of(&self, class: Class) -> f64 {
        match class {
            Class::Restricted => self.a(),
            Class::Plain => self.b(),
        }
    }
}

/// Where a pane that has to move goes, or `None` when no account takes
/// it or a plain pane waits for the restricted panes (`Reserve::N`).
///
/// `loads` are the other panes' speeds per account, in `accounts`'
/// order, without this pane; `from` is the account it leaves.
pub fn pick(accounts: &[Quant], plan: &Plan, loads: &[Load], m: Mover, from: Option<u8>, horizon_h: f64) -> Option<u8> {
    pick_where(accounts, plan, loads, m, from, horizon_h, Reserve::N, |_| true)
}

/// Where `pick` would have sent a plain pane it holds back for the
/// restricted panes, when that is why it has nowhere to go.
pub fn held_back(accounts: &[Quant], plan: &Plan, loads: &[Load], m: Mover, from: Option<u8>, horizon_h: f64) -> Option<u8> {
    if pick(accounts, plan, loads, m, from, horizon_h).is_some() {
        return None;
    }
    pick_where(accounts, plan, loads, m, from, horizon_h, Reserve::None, |_| true)
}

/// `pick` among the accounts that pass `allow` only, holding back what
/// `reserve` says.
#[allow(clippy::too_many_arguments)] // one decision's inputs, each a separate piece
pub fn pick_where(accounts: &[Quant], plan: &Plan, loads: &[Load], m: Mover, from: Option<u8>, horizon_h: f64, reserve: Reserve, allow: impl Fn(&Quant) -> bool) -> Option<u8> {
    let mut best: Option<(Key, u8)> = None;
    for (i, q) in accounts.iter().enumerate() {
        if Some(q.id) == from || !q.accepts(m.class) || !allow(q) || reserve.holds(q, plan, i, &loads[i], m) {
            continue;
        }
        let k = key(q, plan, i, &loads[i], m, horizon_h);
        if best.as_ref().is_none_or(|(b, _)| k.before(b)) {
            best = Some((k, q.id));
        }
    }
    best.map(|(_, id)| id)
}

/// The sort key of one candidate.
#[derive(Clone, Copy, Debug)]
struct Key {
    stale: bool,
    tier: u8,
    no_deficit: bool,
    /// Renewal when short of its target; minus how long it lasts when not.
    order: f64,
    id: u8,
}

impl Key {
    fn before(&self, o: &Key) -> bool {
        (self.stale, self.tier, self.no_deficit)
            .cmp(&(o.stale, o.tier, o.no_deficit))
            .then(self.order.total_cmp(&o.order))
            .then(self.id.cmp(&o.id))
            .is_lt()
    }
}

fn key(q: &Quant, plan: &Plan, i: usize, load: &Load, m: Mover, horizon_h: f64) -> Key {
    let total = load.total() + m.rate;
    let tier = match m.class {
        Class::Restricted => {
            if lasts_of(q, load, m) >= horizon_h { 0 } else { 2 }
        }
        Class::Plain => {
            if plain_lasts(plan.u[i], load, m.b()) >= horizon_h {
                0
            } else if q.r / total >= horizon_h {
                1
            } else {
                2
            }
        }
    };
    let target = match m.class {
        Class::Restricted => plan.t_f[i],
        Class::Plain => plan.t_n[i],
    };
    let short = target - load.of(m.class) >= m.of(m.class) / 2.0;
    let order = if short { q.renews_in(m.class) } else { -lasts_of(q, load, m) };
    Key { stale: q.stale, tier, no_deficit: !short, order, id: q.id }
}

/// Each account's load from `(account, class, rate)` per pane, in
/// `accounts`' order, leaving out the pane at `skip`.
pub fn loads_of(accounts: &[Quant], panes: &[(Option<u8>, Class, f64)], skip: Option<usize>) -> Vec<Load> {
    let mut loads = vec![Load::default(); accounts.len()];
    for (j, (on, class, rate)) in panes.iter().enumerate() {
        if Some(j) == skip {
            continue;
        }
        if let Some(i) = on.and_then(|id| accounts.iter().position(|q| q.id == id)) {
            loads[i].add(*class, *rate);
        }
    }
    loads
}

/// The same from each pane's account and two speeds `(a, b)`: the
/// restricted load is what the panes use of the restricted model, not
/// the whole speed of the panes that need it.
pub fn loads_split(accounts: &[Quant], panes: &[(Option<u8>, f64, f64)], skip: Option<usize>) -> Vec<Load> {
    let mut loads = vec![Load::default(); accounts.len()];
    for (j, (on, a, b)) in panes.iter().enumerate() {
        if Some(j) == skip {
            continue;
        }
        if let Some(i) = on.and_then(|id| accounts.iter().position(|q| q.id == id)) {
            loads[i].restricted += a;
            loads[i].plain += b;
        }
    }
    loads
}

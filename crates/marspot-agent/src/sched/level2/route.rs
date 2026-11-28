//! The routing table: where new work goes, by class and expected
//! lifetime, from what the pane plan leaves on each account.
//!
//! Each account's room is its week, restricted window and plain spare
//! (level 1's, which leaves the reserve alone) less what the plan's
//! panes use of them before the account renews or the pane horizon
//! ends, whichever is first.  A newcomer of the restricted class takes
//! from the restricted window and the week; one of the plain class from
//! the plain spare and the week.  A placement takes the newcomer's
//! expected use out of the table at once, so a batch spreads over the
//! entries and no placement plans again.
//!
//! The order, for each class: capacity only that class uses first (the
//! plain class: an account with week beyond its restricted window; the
//! restricted class: every account whose window it can use), then room
//! that lapses at a renewal inside the pane horizon, earliest renewal
//! first, then the account that lasts longest at its planned load.  A
//! long-lived newcomer only goes where the room lasts it `LASTS_H` at
//! its high band, or to the renewal when that comes first.
//!
//! The other agents have a line each with what their best account has
//! left; a request that leaves the agent open goes where the most is
//! left for its use.

use super::{H2, Input, LASTS_H, Plan};
use crate::quota::{Class, EPS};

/// A newcomer under this many hours is short-lived.
pub const SHORT_H: f64 = 0.5;
/// Accounts in a table at most; more are left out.
pub const ACCOUNTS_CAP: usize = 32;
/// Other agents' lines at most.
pub const OTHERS_CAP: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Room {
    id: u8,
    /// Week, restricted window and plain spare left after the plan, weeks.
    week: f64,
    window: f64,
    plain: f64,
    /// The account's own: only the other models can use part of its week.
    plain_only: bool,
    /// Hours until the week and the restricted window renew.
    t: f64,
    tf: f64,
    /// The plan's load on it at the level, weeks an hour.
    load: f64,
}

impl Room {
    fn left(&self, class: Class) -> f64 {
        match class {
            Class::Restricted => self.window.min(self.week),
            Class::Plain => self.plain.min(self.week),
        }
        .max(0.0)
    }

    fn renews(&self, class: Class) -> f64 {
        match class {
            Class::Restricted => self.tf.min(self.t),
            Class::Plain => self.t,
        }
    }

    /// Hours the room lasts at the plan's load.
    fn lasts(&self, class: Class) -> f64 {
        if self.load <= EPS { f64::INFINITY } else { self.left(class) / self.load }
    }
}

/// What a placement reads of the newcomer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Newcomer {
    pub class: Class,
    /// Its level and high band, weeks an hour, in its class.
    pub rate: f64,
    pub high: f64,
    /// Its expected life, hours: a helper's measured remaining life, or
    /// `H2` for a pane.
    pub life_h: f64,
}

impl Newcomer {
    pub fn short(&self) -> bool {
        self.life_h < SHORT_H
    }

    /// What it is expected to use on an account that renews in `renews`.
    fn usage(&self, renews: f64) -> f64 {
        self.rate * self.life_h.min(renews.max(0.0)).min(H2)
    }

    /// What the room must hold for it.
    fn needs(&self, renews: f64) -> f64 {
        if self.short() { self.usage(renews) } else { self.usage(renews).max(self.high * LASTS_H.min(renews.max(0.0))) }
    }
}

/// The table one pass leaves for the placements until the next.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Routing {
    rooms: Vec<Room>,
    /// Other agents: name, what their best account has left, weeks.
    pub others: Vec<(&'static str, f64)>,
}

/// Where a placement went, and its entry's room before and after, weeks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placed {
    pub to: u8,
    pub before: f64,
    pub after: f64,
}

impl Routing {
    /// The table of `plan` over `input`.
    pub fn of(input: &Input, plan: &Plan) -> Routing {
        let n = input.accounts.len().min(ACCOUNTS_CAP);
        let mut use_ = vec![[0.0f64; 2]; n];
        let mut load = vec![0.0f64; n];
        let ends: Vec<[f64; 2]> = input.accounts[..n].iter().map(|a| [a.q.tf.min(a.q.t).clamp(0.0, H2), a.q.t.clamp(0.0, H2)]).collect();
        for (k, p) in input.panes.iter().enumerate() {
            let mut on = input.accounts[..n].iter().position(|a| a.q.id == p.on);
            let mut from: f64 = 0.0;
            let hops = plan.scheds.get(k).map_or(&[][..], |s| s.hops());
            for i in 0..=hops.len() {
                let to = hops.get(i).map_or(H2, |h| plan.cuts.get(h.at as usize).copied().unwrap_or(H2));
                if let Some(j) = on {
                    for c in 0..2 {
                        use_[j][c] += p.speed[c] * (to.min(ends[j][c]) - from.min(ends[j][c])).max(0.0);
                    }
                    if from <= 0.0 {
                        load[j] += p.speed[0] + p.speed[1];
                    }
                }
                if let Some(h) = hops.get(i) {
                    on = Some(h.to as usize).filter(|j| *j < n);
                    from = to;
                }
            }
        }
        let rooms = input.accounts[..n]
            .iter()
            .enumerate()
            .filter(|(_, a)| a.q.open && !a.q.stale && a.q.r > EPS)
            .map(|(j, a)| {
                let all = use_[j][0] + use_[j][1];
                Room {
                    id: a.q.id,
                    week: a.q.r - all,
                    window: a.q.phi() - use_[j][0],
                    plain: a.spare - use_[j][1],
                    plain_only: a.q.n() > EPS,
                    t: a.q.t,
                    tf: a.q.tf,
                    load: load[j],
                }
            })
            .collect();
        Routing { rooms, others: Vec::new() }
    }

    /// The other agents' lines, at most `OTHERS_CAP`.
    pub fn with_others(mut self, others: &[(&'static str, f64)]) -> Routing {
        self.others = others.iter().take(OTHERS_CAP).copied().collect();
        self
    }

    /// The accounts in the order a newcomer of `class` tries them.
    pub fn order(&self, class: Class) -> Vec<u8> {
        let mut v: Vec<&Room> = self.rooms.iter().filter(|r| r.left(class) > EPS).collect();
        let lapses = |r: &Room| r.renews(class) < H2;
        let tier = |r: &Room| match class {
            Class::Plain => u8::from(!r.plain_only),
            Class::Restricted => 0,
        };
        v.sort_by(|a, b| {
            (tier(a), !lapses(a))
                .cmp(&(tier(b), !lapses(b)))
                .then_with(|| if lapses(a) { a.renews(class).total_cmp(&b.renews(class)) } else { b.lasts(class).total_cmp(&a.lasts(class)) })
                .then_with(|| b.left(class).total_cmp(&a.left(class)))
                .then(a.id.cmp(&b.id))
        });
        v.iter().map(|r| r.id).collect()
    }

    /// Where `m` goes, `None` when no entry holds it; the room it takes
    /// is taken out of the table.  A pane that moves names the account
    /// it leaves in `except`.
    pub fn place(&mut self, m: &Newcomer, except: Option<u8>) -> Option<Placed> {
        let to = self.order(m.class).into_iter().filter(|id| Some(*id) != except).find(|id| self.rooms.iter().any(|r| r.id == *id && r.left(m.class) >= m.needs(r.renews(m.class)) - EPS))?;
        self.take(to, m)
    }

    /// `m` placed on `to` (where a placement or someone else put it):
    /// its expected use comes out of that entry.
    pub fn take(&mut self, to: u8, m: &Newcomer) -> Option<Placed> {
        let r = self.rooms.iter_mut().find(|r| r.id == to)?;
        let before = r.left(m.class);
        let u = m.usage(r.renews(m.class));
        r.week -= u;
        match m.class {
            Class::Restricted => r.window -= u,
            Class::Plain => r.plain -= u,
        }
        r.load += m.rate;
        Some(Placed { to, before, after: r.left(m.class) })
    }

    /// The agent a request that leaves it open goes to: `claude` while
    /// its first entry that holds `m` has the most left, else the other
    /// agent with the most among those whose best account holds `m` for
    /// its lifetime.  Another agent's renewal is not read, so its room
    /// must hold `m`'s whole use (and, for a long-lived one, six hours at
    /// its high band).
    pub fn agent_for(&self, m: &Newcomer) -> &'static str {
        let claude = self.order(m.class).into_iter().filter_map(|id| self.rooms.iter().find(|r| r.id == id)).find(|r| r.left(m.class) >= m.needs(r.renews(m.class)) - EPS).map_or(0.0, |r| r.left(m.class));
        let need = m.needs(f64::INFINITY);
        self.others.iter().filter(|o| o.1 >= need - EPS && o.1 > claude + EPS).max_by(|a, b| a.1.total_cmp(&b.1)).map_or("claude", |o| o.0)
    }

    /// One line, in points: each class's entries in order with their room,
    /// then the other agents.
    pub fn line(&self) -> String {
        let mut s = String::new();
        for (word, class) in [("fable", Class::Restricted), ("plain", Class::Plain)] {
            let entries: Vec<String> = self.order(class).iter().filter_map(|id| self.rooms.iter().find(|r| r.id == *id)).map(|r| format!("P{}={:.1}", r.id, 100.0 * r.left(class))).collect();
            s.push_str(&format!("{word}={} ", if entries.is_empty() { "-".into() } else { entries.join(",") }));
        }
        let others: Vec<String> = self.others.iter().map(|(a, l)| format!("{a}={:.1}", 100.0 * l)).collect();
        s.push_str(&format!("others={}", if others.is_empty() { "-".into() } else { others.join(",") }));
        s
    }
}

#[cfg(test)]
#[path = "route_tests.rs"]
mod tests;

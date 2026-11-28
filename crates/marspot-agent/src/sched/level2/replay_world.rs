//! The recorded panes of 2026-11-24 20:00Z to 11-28 07:30Z (`replay_fx`)
//! on the recorded accounts, in five-minute steps.
//!
//! - `Actual`: every pane where it really was; a refused pane waits for
//!   the move that really happened.
//! - `Planner(lim)`: the planner as the shell drives it under `lim` with
//!   the executor live (`replay_drive`): every step a full search when a
//!   trigger is due, else the standing plan re-timed.  New and refused
//!   panes go where the routing table says (refused ones by today's
//!   objective when it has no entry).  From the first recorded boundary
//!   a committed switch is decided at each of its pane's boundaries by
//!   the executor's stopping rule; before it every rest is taken as a
//!   chance (the replay of the planner's first version did so).
//!
//! Counted: Fable window lapsing at a renewal and left on accounts whose
//! week is spent at the end, plain use that took what only Fable could
//! use (`Phi`), week lapsing at a renewal, refusals, every switch.

use std::collections::BTreeMap;

use super::exec::Watched;
use fx::{Ev, Fx};
use super::route::Routing;
use super::standing::Standing;
use super::trigger::Triggers;
use super::{Kind, Limits};
use crate::quota::speed::PaneUse;
use crate::quota::{Class, EPS, KAPPA_FABLE, Quant};

pub(super) const WEEK: i64 = 7 * 86_400;
/// A week this nearly spent at the end strands its Fable window.
const SPENT: f64 = 0.005;
/// The recorded replies price at 504 points over the window, the
/// accounts' readings rose by 540: use the records do not hold (a pane's
/// subagents, a reply never logged) is spread over all of it.
pub(super) const USAGE_SCALE: f64 = 540.0 / 504.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Mode {
    Actual,
    Planner(Limits),
}

pub(super) struct Acc {
    pub q: Quant,
    renew: i64,
    renew_f: i64,
    /// What other models took of its `Phi` this cycle, and what they
    /// took beyond level 1's spare: the reserve Fable was to use.
    taken: f64,
    taken_reserve: f64,
    /// Level 1's spare at the last pass, less the other-model use since.
    spare: f64,
}

pub(super) struct P {
    pub on: u8,
    pub class: Class,
    pub busy: bool,
    pub used: PaneUse,
    pub stalled: bool,
    pub first_at: i64,
    /// Planned switches taken.
    pub planned: Vec<i64>,
    pub moved_at: Option<i64>,
    pub left: Option<(u8, i64)>,
    pub ctx: u64,
    /// Clean rests seen at its recorded boundaries.
    pub clean: u32,
    /// A notice went out for a switch to this account.
    pub noticed: Option<(i64, u8)>,
    /// The last recorded boundary with the user at it.
    pub user_at: Option<i64>,
}

/// What one run did; quota in weeks.
#[derive(Debug, Default)]
pub(super) struct Out {
    pub fable_lapsed: f64,
    pub fable_stranded: f64,
    pub phi_taken: f64,
    /// Other-model use beyond level 1's spare (the reserve Fable was to
    /// use), and `Phi` taken while another open account had other-model room.
    pub reserve_taken: f64,
    /// A Fable pane's other-model use beyond the spare where no account
    /// held both its classes.
    pub unavoidable: f64,
    pub phi_taken_room: f64,
    /// The reserve taken, by account and pane.
    pub taken_by: BTreeMap<(u8, u64), f64>,
    /// Other-model use held back by strict protection's waits.
    pub waited: f64,
    pub week_lapsed: f64,
    /// Planned switches: when, pane, from, to, kind, whether it averts a refusal soon.
    pub planned: Vec<(i64, u64, u8, u8, Kind, bool)>,
    pub refusals: Vec<(i64, u8, u64)>,
    /// Every change of account: when, the pane.
    pub moves: Vec<(i64, u64)>,
    pub notices: usize,
    pub unmet: f64,
    pub full: usize,
    /// Each cycle's end: when, account, week and Fable window left, and
    /// what other models took of its `Phi` in the cycle, all of it and
    /// beyond level 1's spare.
    pub cycles: Vec<(i64, u8, f64, f64, f64, f64)>,
    /// At recorded boundaries: with a watched switch, the user at the
    /// pane, before the window opened, and the executor's answers.
    pub diag: [usize; 6],
}

/// One committed switch whose window the last pass left open.
#[derive(Clone, Copy, Debug)]
pub(super) struct Watch {
    pub sid: u64,
    pub from: u8,
    pub to: u8,
    pub w: Watched,
}

pub(super) struct W<'a> {
    pub fx: &'a Fx,
    pub mode: Mode,
    pub accs: Vec<Acc>,
    pub panes: BTreeMap<u64, P>,
    pub out: Out,
    pub st: Standing,
    pub tr: Triggers,
    pub ended: bool,
    pub refused: bool,
    pub table: Option<Routing>,
    pub watch: Vec<Watch>,
    pub deadlines: super::exec::Deadlines,
    /// Notices sent, for the hourly limit.
    pub notices: Vec<i64>,
    /// Restricted use recorded so far, by hour.
    pub hours_r: Vec<f64>,
    /// The other-model panes strict protection would make wait, and
    /// whether the replay makes the unattended ones wait.
    pub waits: Vec<u64>,
    pub hold: bool,
    /// The use being served is a Fable pane's unavoidable other-model use.
    pub stuck: bool,
}

pub(super) fn run(fx: &Fx, mode: Mode) -> Out {
    run_holding(fx, mode, false)
}

/// `run`, with strict protection's waits made, not only rehearsed.
pub(super) fn run_holding(fx: &Fx, mode: Mode, hold: bool) -> Out {
    let accs = fx.start.iter().map(|&(id, r, f, renew, renew_f)| Acc { q: Quant { id, r, f, t: 0.0, tf: 0.0, open: true, stale: false }, renew, renew_f, taken: 0.0, taken_reserve: 0.0, spare: 0.0 }).collect();
    let mut w = W { fx, mode, accs, panes: BTreeMap::new(), out: Out::default(), st: Standing::default(), tr: Triggers::default(), ended: false, refused: false, table: None, watch: Vec::new(), deadlines: Default::default(), notices: Vec::new(), hours_r: Vec::new(), waits: Vec::new(), hold, stuck: false };
    let mut next = 0;
    for k in 0..fx.len / fx.step {
        let now = fx.t0 + k * fx.step;
        w.renew(now);
        // today's rule ran the planner in shadow: its level 1 still says what Fable was to use
        w.pass(now, if let Mode::Planner(lim) = mode { lim } else { Limits::STEADY });
        if let Mode::Planner(_) = mode {
            if now < fx.bounds_from {
                w.take_at_rest(now);
            }
        }
        while next < fx.events.len() && fx.events[next].0 < now + fx.step {
            let (at, ev) = fx.events[next];
            w.event(at, ev);
            next += 1;
        }
        for &(sid, class, amount, ctx) in fx.usage.get(&k).map_or(&[][..], Vec::as_slice) {
            w.use_(now, sid, class, amount * USAGE_SCALE, ctx);
        }
    }
    let end = fx.t0 + fx.len;
    for a in &w.accs {
        if a.q.r < SPENT {
            w.out.fable_stranded += a.q.f.max(0.0);
            w.out.cycles.push((end, a.q.id, a.q.r, a.q.f.max(0.0), a.taken, a.taken_reserve));
        }
    }
    w.out
}

impl W<'_> {
    fn renew(&mut self, now: i64) {
        for a in &mut self.accs {
            if now >= a.renew_f || now >= a.renew {
                self.out.cycles.push((now, a.q.id, a.q.r.max(0.0), a.q.f.max(0.0), a.taken, a.taken_reserve));
                (a.taken, a.taken_reserve) = (0.0, 0.0);
            }
            if now >= a.renew_f {
                self.out.fable_lapsed += a.q.f.max(0.0);
                a.q.f = KAPPA_FABLE;
                a.renew_f += WEEK;
            }
            if now >= a.renew {
                self.out.week_lapsed += a.q.r.max(0.0);
                a.q.r = 1.0;
                a.renew += WEEK;
            }
            a.q.t = (a.renew - now) as f64 / 3600.0;
            a.q.tf = (a.renew_f - now) as f64 / 3600.0;
            a.q.open = a.q.r > EPS;
        }
    }

    pub(super) fn open(&self, id: u8) -> bool {
        self.accs.iter().any(|a| a.q.id == id && a.q.open)
    }

    fn add(&mut self, at: i64, sid: u64, on: u8, class: Class) {
        let mut used = PaneUse::default();
        used.watched(at);
        self.panes.insert(sid, P { on, class, busy: false, used, stalled: false, first_at: at, planned: Vec::new(), moved_at: None, left: None, ctx: 0, clean: 0, noticed: None, user_at: None });
    }

    fn event(&mut self, at: i64, ev: Ev) {
        let planner = matches!(self.mode, Mode::Planner(_));
        match ev {
            Ev::Bind(sid, to, class) => match self.panes.get_mut(&sid) {
                None => self.add(at, sid, to, class),
                Some(p) => {
                    p.class = class;
                    if !planner && p.on != to {
                        self.switch(at, sid, to, None);
                    }
                }
            },
            Ev::New(sid, to, class) => {
                let to = if planner { self.place_new(class, to).unwrap_or(to) } else { to };
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
            Ev::Boundary(sid, b) => {
                if let Mode::Planner(lim) = self.mode {
                    self.at_boundary(at, sid, b, &lim);
                }
            }
            Ev::Out(..) => {}
        }
    }

    /// `sid` moves to `to`; a planned switch's kind and whether it is exempt.
    pub(super) fn switch(&mut self, at: i64, sid: u64, to: u8, kind: Option<(Kind, bool)>) {
        let p = self.panes.get_mut(&sid).expect("a live pane");
        let from = p.on;
        p.left = Some((from, at));
        p.on = to;
        p.moved_at = Some(at);
        p.stalled = false;
        p.noticed = None;
        if let Some((k, exempt)) = kind {
            p.planned.push(at);
            self.out.planned.push((at, sid, from, to, k, exempt));
        }
        self.out.moves.push((at, sid));
    }
}

#[path = "replay_fx.rs"]
pub(super) mod fx;
#[path = "replay_use.rs"]
mod use_;
#[cfg(test)]
#[path = "replay_tests.rs"]
mod tests;
#[path = "replay_drive.rs"]
mod drive;

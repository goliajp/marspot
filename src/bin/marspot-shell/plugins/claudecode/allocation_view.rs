//! The allocation core's view of one scan, for the choices the main
//! thread makes: where a refused pane goes, where a badge click sends
//! one, which account a pane leaving early goes to, and the resting
//! moves the scan worker decided.
//!
//! The worker builds it with the speeds it measured; where it has none
//! (no scan has carried one yet) the main thread builds the same view
//! from the scan with every pane at the default speed.  What the main
//! thread knows that the scan does not, an account that has refused a
//! pane since the feed was written, is laid over the accounts before
//! each choice, so a pane is not sent back to an account that just
//! refused it.

use std::collections::HashMap;

use super::allocation_inputs::{AccountIn, Snapshot};
use super::fable_need::FABLE;
use super::quota::helper_pick::{Placed, unread_pick};
use super::quota::{Move, Room};
use crate::plugins::allocation::choose::{MUST_LAST_H, Mover, loads_of};
use crate::plugins::allocation::helper::{Placement, Scene, place_by};
use crate::plugins::allocation::notice::stranded;
use crate::plugins::allocation::replay::{Inputs, Kind, Rule, Why};
use crate::plugins::allocation::waste;
use crate::plugins::allocation::speed::default_rate;
use crate::plugins::allocation::{Class, KAPPA_FABLE, Quant};
use crate::host::published::Published;

/// Why a pane that has to move stays where it is.
pub(super) const ALL_OUT: &str = "every account is out";
pub(super) const NOWHERE: &str = "nowhere else to go";

/// The last sweep's view, for a helper started between sweeps.
static LAST: Published<View> = Published::new();

/// One pane in the plan: session, account, class, speed (weeks an
/// hour), and whether it is a helper.
pub(super) type PaneRow = (u64, u8, Class, f64, bool);

#[derive(Clone, Debug, PartialEq)]
pub(super) struct View {
    pub accounts: Vec<Quant>,
    pub b_f: f64,
    pub b_n: f64,
    pub panes: Vec<PaneRow>,
    /// The resting moves the core decided this scan, each with every
    /// input that decided it.
    pub rest: HashMap<u64, (u8, Why, Inputs)>,
    /// The rule the choices are made by, with what it read of the
    /// last day's resting moves.
    pub rule: Rule,
    /// Each pane's context where the scan read one, tokens.
    pub tokens: HashMap<u64, u64>,
}

impl View {
    /// The view of `snap` with each pane's restricted and plain speed
    /// from `rates`, in `snap.panes`' order, and `slope_f` the
    /// restricted use the accounts' readings show (weeks an hour).
    pub(super) fn from_rates(snap: &Snapshot, rates: &[(f64, f64)], slope_f: f64) -> View {
        let panes: Vec<PaneRow> = snap.panes.iter().zip(rates).map(|(p, (f, n))| (p.sid, p.profile, p.class, f + n, p.helper)).collect();
        let accounts = snap
            .accounts
            .iter()
            .map(|ac| ac.quant(snap.now, panes.iter().filter(|r| r.1 == ac.profile).map(|r| r.3).sum()))
            .collect();
        // every pane's restricted use, a plain pane's agents' too; the
        // readings also see what nothing here measured
        let b_f = rates.iter().map(|(f, _)| f).sum::<f64>().max(slope_f);
        let b_n = rates.iter().map(|(_, n)| n).sum();
        View { accounts, b_f, b_n, panes, rest: HashMap::new(), rule: Rule::waste(0), tokens: HashMap::new() }
    }

    /// The same with every pane at the default speed of its class.
    pub(super) fn of_defaults(snap: &Snapshot) -> View {
        let rates: Vec<(f64, f64)> = snap.panes.iter().map(|p| default_rate(p.class)).map(|(f, n, _)| (f, n)).collect();
        View::from_rates(snap, &rates, 0.0)
    }

    /// For a badge click between scans: the accounts as `accounts` read
    /// them now, the panes and their speeds as the last scan had them.
    pub(super) fn for_click(now: i64, accounts: Vec<AccountIn>, last: Option<&View>) -> View {
        let snap = Snapshot { now, accounts, ..Snapshot::default() };
        let mut v = View::of_defaults(&snap);
        if let Some(last) = last {
            let rate_on = |p: u8| last.panes.iter().filter(|r| r.1 == p).map(|r| r.3).sum();
            v.accounts = snap.accounts.iter().map(|ac| ac.quant(now, rate_on(ac.profile))).collect();
            v.panes = last.panes.clone();
            v.b_f = last.b_f;
            v.b_n = last.b_n;
            v.tokens = last.tokens.clone();
        }
        v
    }

    /// Keep this view for the helpers started before the next sweep.
    pub(super) fn publish(&self) {
        LAST.publish(self.clone());
    }

    /// What a helper started now reads: the accounts as `accounts` read
    /// them and `rooms` judge them, the panes and their speeds as the
    /// last sweep had them.  Before any sweep has published a view no
    /// helper pane is known either way, so none is said to be shown.
    pub(super) fn helper_start_scene(now: i64, accounts: Vec<AccountIn>, rooms: &[(u8, Room)]) -> Scene {
        let last = LAST.get();
        let mut scene = View::for_click(now, accounts, last.as_deref()).helper_scene(rooms, None);
        if last.is_none() {
            scene.shown = None;
        }
        scene
    }

    /// What a helper's choice reads: the accounts as the main thread
    /// judges them and the speeds of every pane but `skip`.
    pub(super) fn helper_scene(&self, rooms: &[(u8, Room)], skip: Option<u64>) -> Scene {
        let accounts = self.judged(rooms);
        let table: Vec<(Option<u8>, Class, f64)> = self.panes.iter().filter(|p| Some(p.0) != skip).map(|p| (Some(p.1), p.2, p.3)).collect();
        let loads = loads_of(&accounts, &table, None);
        Scene { accounts, loads, b_f: self.b_f, b_n: self.b_n, kappa: Some(KAPPA_FABLE), shown: Some(self.helpers()) }
    }

    /// The helper panes these loads hold: session, account, class.
    pub(super) fn helpers(&self) -> Vec<(u64, u8, Class)> {
        self.panes.iter().filter(|p| p.4).map(|p| (p.0, p.1, p.2)).collect()
    }

    /// Where a refused helper goes, rule (c), with the helpers `placed`
    /// whose panes these loads do not hold yet counted on their
    /// accounts; with the inputs that chose it when the core chose it.
    pub(super) fn helper_must_move(&self, rooms: &[(u8, Room)], sid: u64, from: u8, class: Class, placed: &Placed) -> (Move, Option<Inputs>) {
        let mover = Mover { class, rate: self.rate_of(sid, class), tokens: self.tokens.get(&sid).copied() };
        let scene = self.helper_scene(rooms, Some(sid));
        match place_by(&scene, mover, Some(from), &placed.counts(), self.rule) {
            Placement::To(to, inputs) => (Move::To(to), Some(inputs)),
            // the one objective holds no helper back
            Placement::HeldBack(..) | Placement::Nowhere => {
                let model = (class == Class::Restricted).then_some(FABLE);
                match unread_pick(rooms, Some(from), model, placed) {
                    Some(to) => (Move::To(to), None),
                    None => (Move::Hold(ALL_OUT), None),
                }
            }
        }
    }

    /// Pane `sid` was sent to `to` this pass: the next choice counts it
    /// there.
    pub(super) fn placed(&mut self, sid: u64, to: u8) {
        if let Some(row) = self.panes.iter_mut().find(|p| p.0 == sid) {
            row.1 = to;
        }
    }

    /// The accounts as the main thread judges them: one it cannot speak
    /// for is left out, one refused shut, one refused Fable without it.
    fn judged(&self, rooms: &[(u8, Room)]) -> Vec<Quant> {
        self.accounts
            .iter()
            .filter_map(|q| {
                let room = rooms.iter().find(|(p, _)| *p == q.id).map(|(_, r)| r)?;
                if *room == Room::Unknown {
                    return None;
                }
                let mut q = q.clone();
                q.open &= !room.refused_for(None);
                if room.refused_for(Some(FABLE)) {
                    q.f = 0.0;
                }
                Some(q)
            })
            .collect()
    }

    fn rate_of(&self, sid: u64, class: Class) -> f64 {
        self.panes.iter().find(|p| p.0 == sid).map_or_else(
            || {
                let (f, n, _) = default_rate(class);
                f + n
            },
            |p| p.3,
        )
    }

    /// What a choice for pane `sid` of `class` on `from` reads.
    fn inputs(&self, rooms: &[(u8, Room)], sid: u64, from: u8, class: Class, horizon_h: f64) -> Inputs {
        let accounts = self.judged(rooms);
        let table: Vec<(Option<u8>, Class, f64)> = self.panes.iter().filter(|p| p.0 != sid).map(|p| (Some(p.1), p.2, p.3)).collect();
        let loads = loads_of(&accounts, &table, None);
        let mover = Mover { class, rate: self.rate_of(sid, class), tokens: self.tokens.get(&sid).copied() };
        Inputs { accounts, b_f: self.b_f, b_n: self.b_n, kappa: Some(KAPPA_FABLE), loads, mover, here: from, kind: Kind::Out { horizon_h } }
    }

    /// Where a pane its account cannot answer goes, rule (a); with the
    /// inputs that chose it when the core chose it.  When no account
    /// read can take it, an account whose reading is missing for it
    /// is last before staying.
    pub(super) fn must_move(&self, rooms: &[(u8, Room)], sid: u64, from: u8, class: Class) -> (Move, Option<Inputs>) {
        let inputs = self.inputs(rooms, sid, from, class, MUST_LAST_H);
        let plan = inputs.plan();
        if let Some((to, _)) = inputs.decide_by(&plan, self.rule) {
            return (Move::To(to), Some(inputs));
        }
        let others: Vec<Quant> = inputs.accounts.iter().filter(|q| q.id != from).cloned().collect();
        let why = if let Some(to) = unread_last(rooms, from, class) {
            return (Move::To(to), None);
        } else if others.is_empty() {
            NOWHERE
        } else if stranded(&others, class).is_some() {
            ALL_OUT
        } else {
            NOWHERE
        };
        (Move::Hold(why), None)
    }

    /// Where a badge click sends a pane: the account where it costs
    /// least, as for a pane that has to move; an account whose reading
    /// is missing last.
    pub(super) fn by_hand(&self, rooms: &[(u8, Room)], sid: u64, from: u8, class: Class) -> Option<(u8, Option<Inputs>)> {
        let inputs = self.inputs(rooms, sid, from, class, MUST_LAST_H);
        match waste::pick(&inputs.accounts, &inputs.loads, inputs.mover, Some(from), inputs.kappa, MUST_LAST_H, &self.rule.params(), |_| true) {
            Some(to) => Some((to, Some(inputs))),
            None => unread_last(rooms, from, class).map(|to| (to, None)),
        }
    }

    /// The cheapest of `allowed` for a pane leaving an account that runs
    /// short, with what chose it.
    pub(super) fn rank_among(&self, rooms: &[(u8, Room)], sid: u64, from: u8, class: Class, allowed: &[u8]) -> Option<(u8, Inputs)> {
        let inputs = self.inputs(rooms, sid, from, class, MUST_LAST_H);
        let to = waste::pick(&inputs.accounts, &inputs.loads, inputs.mover, Some(from), inputs.kappa, MUST_LAST_H, &self.rule.params(), |q| allowed.contains(&q.id))?;
        Some((to, inputs))
    }
}

/// An account the feed covers but gives no reading of what bounds a
/// pane of `class` (its week, or for a Fable pane its Fable cap), and
/// that has not refused it: the core cannot weigh it, so it ranks after
/// every account it can, ahead of staying on one that is out.
fn unread_last(rooms: &[(u8, Room)], from: u8, class: Class) -> Option<u8> {
    let model = (class == Class::Restricted).then_some(FABLE);
    let unread: Vec<(u8, Room)> = rooms.iter().filter(|(p, r)| *p != from && *r != Room::Unknown && !r.known_for(model) && !r.refused_for(model)).cloned().collect();
    super::quota::best_profile(&unread, from, model)
}

/// The class a pane is judged as, from the model it is judged as.
pub(super) fn class_of_judged(model: Option<&str>) -> Class {
    if super::quota::needs_fable(model) { Class::Restricted } else { Class::Plain }
}

#[cfg(test)]
#[path = "allocation_view_tests.rs"]
mod tests;

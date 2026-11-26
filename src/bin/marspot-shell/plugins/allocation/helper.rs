//! Where a helper goes: rule (c).
//!
//! The same order as a pane that has to move, rule (a), with a shorter
//! horizon: the account has to last the helper two hours.  Helpers are
//! started in batches and the readings lag by minutes, so each helper
//! placed since the accounts were last read counts on its account at
//! the default speed, and the next one of the batch is placed with it
//! there.  A new helper adds itself to the demand the plan shares out;
//! a refused one is already in it.  Where nothing is known about a
//! restricted window's worth (`kappa` is `None`), all of the demand is
//! the other models', so nothing is held back for the restricted ones.

use super::choose::{HELPER_LAST_H, Mover, held_back};
use super::replay::{Inputs, Kind, Rule};
use super::{Class, Load, Quant, RHO0};

/// What a helper's choice reads, besides the helper itself.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Scene {
    /// The accounts the readings cover for the helper's class.
    pub accounts: Vec<Quant>,
    /// The other panes' speeds on each account, in `accounts`' order.
    pub loads: Vec<Load>,
    pub b_f: f64,
    pub b_n: f64,
    pub kappa: Option<f64>,
    /// The helper panes `loads` hold (session, account, class), where
    /// they come from a view of the panes; `None` where they hold none.
    pub shown: Option<Vec<(u64, u8, Class)>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Placement {
    /// To this account, with every input that chose it.
    To(u8, Inputs),
    /// The plain helper would take what the restricted panes are short
    /// of; this is where it would go, with what chose it.
    HeldBack(u8, Inputs),
    /// No account the readings cover takes it.
    Nowhere,
}

/// Where helper `mover` goes; `from` is the account a refused helper
/// leaves, `None` for a new one.  `placed` counts the helpers placed on
/// each account since the reading, by class.
pub(crate) fn place(scene: &Scene, mover: Mover, from: Option<u8>, placed: &[(u8, Class, u32)]) -> Placement {
    place_by(scene, mover, from, placed, Rule::Bands)
}

/// `place` under `rule`.
pub(crate) fn place_by(scene: &Scene, mover: Mover, from: Option<u8>, placed: &[(u8, Class, u32)], rule: Rule) -> Placement {
    let inputs = inputs(scene, mover, from, placed);
    let plan = inputs.plan();
    if let Some((to, _)) = inputs.decide_by(&plan, rule) {
        return Placement::To(to, inputs);
    }
    // the one objective holds nothing back: where it finds no account,
    // no account takes the helper
    if matches!(rule, Rule::Waste { .. }) {
        return Placement::Nowhere;
    }
    match held_back(&inputs.accounts, &plan, &inputs.loads, mover, from, HELPER_LAST_H) {
        Some(would) => Placement::HeldBack(would, inputs),
        None => Placement::Nowhere,
    }
}

/// The core's inputs for the choice `place` makes.
pub(crate) fn inputs(scene: &Scene, mover: Mover, from: Option<u8>, placed: &[(u8, Class, u32)]) -> Inputs {
    let mut loads = scene.loads.clone();
    let mut demand = Load { restricted: scene.b_f, plain: scene.b_n };
    for (id, class, n) in placed {
        let rate = RHO0 * f64::from(*n);
        demand.add(*class, rate);
        if let Some(i) = scene.accounts.iter().position(|q| q.id == *id) {
            loads[i].add(*class, rate);
        }
    }
    if from.is_none() {
        demand.add(mover.class, mover.rate);
    }
    let (b_f, b_n) = match scene.kappa {
        Some(_) => (demand.restricted, demand.plain),
        None => (0.0, demand.total()),
    };
    let kind = match from {
        None => Kind::New { horizon_h: HELPER_LAST_H },
        Some(_) => Kind::Out { horizon_h: HELPER_LAST_H },
    };
    // no account is numbered 0 among those a refused helper leaves; a
    // new one leaves none, so `here` is not read
    Inputs { accounts: scene.accounts.clone(), b_f, b_n, kappa: scene.kappa, loads, mover, here: from.unwrap_or(0), kind }
}

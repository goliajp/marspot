//! Which home a new gemini helper starts in: rule (c).
//!
//! With `gemini.allocation` off, the homes take turns, as before.
//! Otherwise the core places the helper among the homes by the
//! refusals the plugin's last pass knew and the gemini panes on each,
//! every helper placed in the last fifteen minutes counted at the
//! default speed.  `dryrun` writes where it would go, or that every
//! home refuses it, and still takes turns; `live` starts it there, or
//! refuses it.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;

use marspot::settings::AgentAllocation;

use super::alloc::{Refused, events_dir, quants, shut};
use super::quota::Scope;
use crate::plugins::allocation::choose::Mover;
use crate::plugins::allocation::helper::{Placement, Scene, place};
use crate::plugins::allocation::{Class, Load, RHO0};
use crate::plugins::claudecode::allocation_events::{Event, EventLog};
use crate::plugins::handoff::Agent;
use crate::plugins::usage::for_helpers::{Pick, Recovery, account, take_turns};

/// Helpers counted as placed for this long; a gemini pane shows up in
/// the plugin's pass well within it.
const PLACED_S: i64 = 15 * 60;
const PLACED_CAP: usize = 64;

#[derive(Default)]
struct Shared {
    refused: Vec<(u8, Scope, i64)>,
    /// The account of each gemini pane.
    panes: Vec<u8>,
    placed: VecDeque<(i64, u8)>,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared { refused: Vec::new(), panes: Vec::new(), placed: VecDeque::new() });

fn shared() -> std::sync::MutexGuard<'static, Shared> {
    SHARED.lock().unwrap_or_else(|p| p.into_inner())
}

/// What the plugin's pass knows, for the next helper.
pub(super) fn publish(refused: Vec<(u8, Scope, i64)>, panes: Vec<u8>) {
    let mut s = shared();
    s.refused = refused;
    s.panes = panes;
}

/// The account a gemini helper starts on, for the helper launcher.
pub(in crate::plugins) fn helper_pick(homes: &[(u8, PathBuf)], model: Option<&str>, now: i64) -> Pick {
    let numbers: Vec<u8> = homes.iter().map(|(n, _)| *n).collect();
    match helper_choice(&numbers, model, now) {
        HelperChoice::To(n) => match homes.iter().find(|(h, _)| *h == n) {
            Some((n, dir)) => Pick::Account(account(Agent::Gemini, *n, dir.clone())),
            None => take_turns(Agent::Gemini, homes),
        },
        HelperChoice::TakeTurns => take_turns(Agent::Gemini, homes),
        HelperChoice::Nowhere(back) => Pick::NoAccount(back.into_iter().map(|(number, at)| Recovery { number, at: Some(at) }).collect()),
    }
}

/// Where a helper goes.
#[derive(Debug, PartialEq)]
pub(crate) enum HelperChoice {
    TakeTurns,
    To(u8),
    /// Every home refuses the model; each with when it is back.
    Nowhere(Vec<(u8, i64)>),
}

/// The choice for a helper on `model` among `homes`, at `now`.
pub(crate) fn helper_choice(homes: &[u8], model: Option<&str>, now: i64) -> HelperChoice {
    choose(marspot::settings::get().gemini_allocation, homes, model, now, &events_dir(&marspot::paths::state_root()))
}

pub(super) fn choose(mode: AgentAllocation, homes: &[u8], model: Option<&str>, now: i64, dir: &std::path::Path) -> HelperChoice {
    if mode == AgentAllocation::Off || homes.is_empty() {
        return HelperChoice::TakeTurns;
    }
    let mut s = shared();
    s.placed.retain(|(at, _)| now - at < PLACED_S);
    let refused: std::collections::HashMap<(u8, Scope), Refused> = s.refused.iter().map(|(id, sc, r)| ((*id, sc.clone()), Refused { at: 0, resets_at: *r })).collect();
    let accounts = quants(&refused, homes, model, now);
    let mut loads = vec![Load::default(); accounts.len()];
    for on in &s.panes {
        if let Some(i) = homes.iter().position(|h| h == on) {
            loads[i].add(Class::Plain, RHO0);
        }
    }
    let mut placed: Vec<(u8, Class, u32)> = Vec::new();
    for (_, id) in &s.placed {
        match placed.iter_mut().find(|(p, _, _)| p == id) {
            Some(x) => x.2 += 1,
            None => placed.push((*id, Class::Plain, 1)),
        }
    }
    let scene = Scene { accounts, loads, b_f: 0.0, b_n: RHO0 * s.panes.len() as f64, kappa: None, shown: None };
    let live = mode == AgentAllocation::Live;
    // a row lost to a full disk does not stop the helper
    let write = |kind: &str, profile: Option<u8>, detail: String| {
        let ev = Event { at: now, sid: 0, kind: kind.into(), profile, class: "plain".into(), detail };
        let _ = EventLog::for_agent(dir.to_path_buf(), "gemini").append(&ev);
    };
    let (to, inputs) = match place(&scene, Mover { class: Class::Plain, rate: RHO0, tokens: None }, None, &placed) {
        Placement::To(to, inputs) | Placement::HeldBack(to, inputs) => (to, inputs),
        Placement::Nowhere => {
            let back: Vec<(u8, i64)> = homes.iter().filter_map(|&h| shut(&refused, h, model, now).map(|t| (h, t))).collect();
            let words: Vec<String> = back.iter().map(|(h, t)| format!("P{h}@{t}")).collect();
            write(if live { "refused_new" } else { "would_refuse_new" }, None, format!("model={} back={}", model.unwrap_or("-"), words.join(",")));
            return if live { HelperChoice::Nowhere(back) } else { HelperChoice::TakeTurns };
        }
    };
    write(if live { "placed" } else { "would_move" }, Some(to), format!("to=P{to} why=out via=new {}", inputs.encode()));
    if !live {
        return HelperChoice::TakeTurns;
    }
    if s.placed.len() >= PLACED_CAP {
        s.placed.pop_front();
    }
    s.placed.push_back((now, to));
    HelperChoice::To(to)
}

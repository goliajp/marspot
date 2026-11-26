//! Which account a helper goes to: the allocation core's rule (c).
//!
//! Helpers are started in batches, and what the choice reads lags: a
//! helper just placed is in neither the feed nor the panes yet.  Each
//! one is counted on its account at the default speed until it is, so a
//! batch is spread over the accounts with a gap instead of all landing
//! on the one ranked first.  Where the choice reads the panes (Claude),
//! that is until a view of them shows the helper's pane; elsewhere,
//! until the next feed.
//!
//! Claude's accounts go to the core as the main thread's view has them
//! (`allocation_view`); the other agents' from their rooms here, with
//! no reserve for a model of their own: only whether it can go.  An
//! account the feed covers but gives no reading for comes after every
//! account it reads, and a batch on such accounts is spread by count.

use std::path::Path;
use std::sync::Mutex;

use marspot::cc_usage::CcUsage;

use super::super::allocation_adapter::class_word;
use super::super::allocation_events::{Event, EventLog};
use super::super::seen::Resets;
use super::super::allocation_inputs::STALE_ACCOUNT_S;
use super::super::allocation_view::View;
use super::{Move, Room, FIVE_HOUR_SPENT, same_model};
use crate::plugins::allocation::choose::Mover;
use crate::plugins::allocation::helper::{Placement, Scene, place};
use crate::plugins::allocation::replay::{Inputs, Rule, Why};
use crate::plugins::allocation::{Class, Quant, RHO0};

#[path = "helper_placed.rs"]
mod placed_tally;
pub(in crate::plugins) use placed_tally::Placed;

/// The agents whose accounts are chosen by their usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::plugins) enum Pool {
    Claude,
    Codex,
    Kimi,
}

/// One tally per pool, shared by the helpers' start and by a refused
/// helper's move, which both place helpers on accounts.
static PLACED: Mutex<[Placed; 3]> = Mutex::new([Placed::new(), Placed::new(), Placed::new()]);

/// Run `f` on the tally of `pool`.
pub(in crate::plugins) fn placed<R>(pool: Pool, f: impl FnOnce(&mut Placed) -> R) -> R {
    let mut all = PLACED.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut all[pool as usize])
}

/// The class a helper on `model` is placed as: restricted where the
/// feed lists a cap of that model's own on some account.
pub(in crate::plugins) fn class_in(rooms: &[(u8, Room)], model: Option<&str>) -> Class {
    let Some(want) = model.map(str::to_ascii_lowercase) else { return Class::Plain };
    let capped = rooms.iter().any(|(_, r)| matches!(r, Room::Known { models, .. } if models.iter().any(|(l, _)| same_model(&want, l))));
    if capped { Class::Restricted } else { Class::Plain }
}

/// The class a Claude helper on `model` is placed as.
pub(in crate::plugins) fn claude_class(model: Option<&str>) -> Class {
    if super::needs_fable(model) { Class::Restricted } else { Class::Plain }
}

/// The Claude profiles `numbers` as the core reads them for a helper
/// started now: the accounts of `usage` judged by `rooms`, the other
/// panes as the last sweep had them.  Reads the profiles' configs.
pub(in crate::plugins) fn claude_scene(numbers: &[u8], rooms: &[(u8, Room)], usage: Option<&CcUsage>, now: i64) -> Scene {
    let (Some(home), Some(usage)) = (std::env::var_os("HOME").map(std::path::PathBuf::from), usage) else { return Scene::default() };
    let mails: Vec<(u8, Option<String>)> = numbers.iter().map(|p| (*p, super::profile_email(&home, *p))).collect();
    let accounts = super::super::allocation_inputs::accounts_of(&mails, usage, now);
    View::helper_start_scene(now, accounts, rooms)
}

/// The Fable model the panes on this machine run, as claude names it on
/// its command line, and how many times the week its work costs against
/// a model with no window of its own: the newest status-line record of
/// a Fable model.  `None` when no pane has run one in the records' three
/// days.
pub(in crate::plugins) fn fable_model() -> Option<(String, f64)> {
    let dir = super::super::statusline::model_push_dir();
    let mut newest: Option<(std::time::SystemTime, String)> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let Ok(when) = e.metadata().and_then(|m| m.modified()) else { continue };
        if newest.as_ref().is_some_and(|(t, _)| *t >= when) {
            continue;
        }
        let text = std::fs::read_to_string(e.path()).unwrap_or_default();
        let id = text.lines().find_map(|l| l.strip_prefix("id=")).map(str::trim).filter(|id| super::super::model::cli_model_ok(id));
        if let Some(id) = id.filter(|id| super::super::allocation_prices::class_of(id) == Class::Restricted) {
            newest = Some((when, id.to_string()));
        }
    }
    newest.map(|(_, id)| {
        let k = super::super::allocation_prices::week_cost_against_plain(&id);
        (id, k)
    })
}

/// A line in `alloc_events` for where a new Claude helper was started:
/// `kind` on `to`, with every input that chose it.  The helper has no
/// pane yet, so its session is 0.
pub(in crate::plugins) fn record_new(dir: &Path, at: i64, kind: &str, to: u8, inputs: &Inputs) {
    let detail = format!("{}{}", inputs.encode(), Rule::waste(0).words());
    let ev = Event { at, sid: 0, kind: kind.into(), profile: Some(to), class: class_word(inputs.mover.class).into(), detail };
    if let Err(e) = EventLog::new(dir.to_path_buf()).append(&ev) {
        marspot::lx_warn!("alloc.events_failed", &format!("{kind} P{to}: {e}"));
    }
}

/// Where the shell keeps `alloc_events`.
pub(in crate::plugins) fn events_dir() -> std::path::PathBuf {
    super::super::allocation_adapter::events_dir(&marspot::paths::state_root())
}

/// The accounts the feed reads for a helper on `model`, as the core
/// sees them, with nothing known of the other panes there.  A model
/// cap counts only as open or spent.
pub(in crate::plugins) fn rooms_scene(rooms: &[(u8, Room)], resets: &[(u8, Resets)], model: Option<&str>, now: i64) -> Scene {
    let hours = |at: i64| (at - now) as f64 / 3600.0;
    let accounts: Vec<Quant> = rooms
        .iter()
        .filter(|(_, r)| r.known_for(model))
        .filter_map(|(n, room)| {
            let Room::Known { util_7d: Some(w), reset_7d, measured_at, .. } = room else { return None };
            let r = (1.0 - w).max(0.0);
            let t = hours(*reset_7d);
            let want = model.map(str::to_ascii_lowercase);
            let tf = want
                .as_deref()
                .and_then(|m| resets.iter().find(|(p, _)| p == n)?.1.models.iter().find(|(l, _)| same_model(m, l)).map(|(_, at)| *at))
                .filter(|at| *at > now)
                .map_or(t, hours);
            let f = if room.refused_for(model) && !room.refused_for(None) { 0.0 } else { r };
            Some(Quant { id: *n, r, f, t, tf, open: !room.refused_for(None), stale: now - measured_at > STALE_ACCOUNT_S })
        })
        .collect();
    let loads = vec![Default::default(); accounts.len()];
    Scene { accounts, loads, b_f: 0.0, b_n: 0.0, kappa: None, shown: None }
}

/// The account a helper goes to where the core weighs none: one the
/// feed covers without a reading for `model`, then, for a new helper
/// (`from` is `None`), one the feed does not cover; the fewest helpers
/// placed there first, then the number.  A refused helper never goes
/// to an account the feed does not cover: nothing says it would not
/// refuse it too.
pub(in crate::plugins) fn unread_pick(rooms: &[(u8, Room)], from: Option<u8>, model: Option<&str>, placed: &Placed) -> Option<u8> {
    let tier = |r: &Room| match r {
        Room::Unknown => 1,
        _ => 0,
    };
    rooms
        .iter()
        .filter(|(n, r)| Some(*n) != from && !r.known_for(model) && !r.refused_for(model) && (from.is_none() || *r != Room::Unknown))
        .min_by_key(|(n, r)| (tier(r), placed.count(*n), *n))
        .map(|(n, _)| *n)
}

/// Where a refused helper goes instead of where `next_move` would send
/// it, for an agent whose rooms are all there is: rule (c) among the
/// accounts other than the one that refused it.  `next_move`'s holds
/// stand.  The account chosen is counted as placed.
pub(in crate::plugins) fn helper_move(pool: Pool, feed_at: i64, decision: Move, rooms: &[(u8, Room)], current: u8, model: Option<&str>) -> Move {
    let Move::To(_) = decision else { return decision };
    let now = super::super::rooms::unix_now();
    let class = class_in(rooms, model);
    let mut scene = rooms_scene(rooms, &[], model, now);
    // the helper is the one pane these rooms know to be using them
    scene.b_n = RHO0;
    placed(pool, |p| {
        p.feed(feed_at, now);
        let to = match place(&scene, Mover { class, rate: RHO0, tokens: None }, Some(current), &p.counts()) {
            Placement::To(to, inputs) | Placement::HeldBack(to, inputs) => {
                let agent = match pool {
                    Pool::Claude => "claude",
                    Pool::Codex => "codex",
                    Pool::Kimi => "kimi",
                };
                crate::agentsvc::sched::once(agent, 0, &inputs, Rule::Bands, Some((to, Why::Out)), None);
                Some(to)
            }
            Placement::Nowhere => unread_pick(rooms, Some(current), model, p),
        };
        match to {
            Some(to) => {
                p.note(to, class, now);
                Move::To(to)
            }
            None => Move::Hold("every account is out"),
        }
    })
}

/// What is known about a pool's accounts at one moment.
#[derive(Debug, Clone, Default)]
pub(in crate::plugins) struct Accounts {
    pub rooms: Vec<(u8, Room)>,
    /// When each account's windows end; empty for an account the feed
    /// does not cover.
    pub resets: Vec<(u8, Resets)>,
    /// When the feed was written; 0 without one.
    pub feed_at: i64,
}

/// The Claude profiles `profiles` as the refused-pane sweep sees them:
/// the feed, with the refusals panes have met; and the feed as read.
/// Reads files.
pub(in crate::plugins) fn claude_accounts(profiles: &[u8], now: i64) -> (Accounts, Option<CcUsage>) {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else { return (Accounts::default(), None) };
    let usage = marspot::cc_usage::read();
    let accounts = usage.as_ref().map_or(&[][..], |u| &u.accounts[..]);
    let email = |p: u8| super::profile_email(&home, p);
    let mut rooms = super::rooms_for(profiles, accounts, email, now);
    let resets = super::super::seen::resets_for(profiles, accounts, email);
    let refused = marspot::paths::state_root().join("plugins/claudecode/refused.tsv");
    if let Ok(text) = std::fs::read_to_string(refused) {
        super::super::seen::Seen::decode(&text).mark(&mut rooms, &resets, now);
    }
    let feed_at = usage.as_ref().map_or(0, |u| u.generated_at);
    (Accounts { rooms, resets, feed_at }, usage)
}

/// When a refused account can take a helper on `model` again, as far
/// as the feed says: the latest end among the windows that are spent.
/// `None` where no spent window has a known end, or the account only
/// needs logging in again.
pub(in crate::plugins) fn recovers_at(room: &Room, resets: &Resets, model: Option<&str>, now: i64) -> Option<i64> {
    let Room::Known { status, util_5h, util_7d, models, .. } = room else { return None };
    if status == "auth_error" {
        return None;
    }
    let mut ends = Vec::new();
    if util_7d.is_some_and(|u| u >= 1.0) {
        ends.push(resets.week);
    }
    if util_5h.is_some_and(|u| u >= FIVE_HOUR_SPENT) {
        ends.push(resets.five_hour);
    }
    if let Some(want) = model.map(str::to_ascii_lowercase) {
        let spent = models.iter().any(|(label, u)| same_model(&want, label) && u.is_some_and(|u| u >= 1.0));
        if spent {
            ends.extend(resets.models.iter().filter(|(label, _)| same_model(&want, label)).map(|(_, t)| *t));
        }
    }
    ends.into_iter().filter(|t| *t > now).max()
}

#[cfg(test)]
#[path = "helper_pick_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "helper_start_tests.rs"]
mod start_tests;

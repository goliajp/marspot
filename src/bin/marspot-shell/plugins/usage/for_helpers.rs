//! Which account a new helper runs on, for each of the four agents.
//!
//! Claude, Kimi and Codex by their usage feeds, with the allocation's
//! rule for helpers (`quota::helper_pick`); Gemini writes no usage
//! anywhere, so the gemini plugin places its helpers.  Every call reads
//! the profiles and feeds from disk: off the shell's main loop.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use marspot::cc_usage::CxAccount;

use crate::plugins::allocation::choose::{HELPER_LAST_H, Mover};
use crate::plugins::allocation::helper::{Placement, Scene, place_by};
use crate::agentsvc::sched::Made;
use crate::plugins::allocation::replay::{Inputs, Rule, Why};
use crate::plugins::allocation::{Class, RHO0, waste};
use crate::plugins::claudecode::quota::helper_pick::{self, Accounts, Placed, Pool};
use crate::plugins::claudecode::quota::Room;
use crate::plugins::claudecode::rooms::unix_now;
use crate::plugins::claudecode::seen::Resets;
use crate::plugins::handoff::{self, Agent};

#[path = "for_helpers_text.rs"]
mod text;
pub(in crate::plugins) use text::{earliest_text, recoveries_text};

/// The account a helper is started on.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::plugins) struct HelperAccount {
    /// The profile number; 0 for the agent's default home.
    pub number: u8,
    pub dir: PathBuf,
    /// What to set in the helper's environment to run on it; empty for
    /// the default home.
    pub env: Vec<(&'static str, String)>,
}

pub(in crate::plugins) use super::claude::Login;

/// The login of the claude account at `dir`, as the keychain has it
/// when it answers `within`.
pub(in crate::plugins) fn claude_login(dir: &Path, within: std::time::Duration) -> Option<Login> {
    super::claude::read_login_within(dir, &PathBuf::from(std::env::var_os("HOME")?), within)
}

/// When an account can take a helper again.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::plugins) struct Recovery {
    pub number: u8,
    /// Unix seconds; `None` when the feed does not say.
    pub at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub(in crate::plugins) enum Pick {
    Account(HelperAccount),
    /// Every account refuses the model, or the agent has none.
    NoAccount(Vec<Recovery>),
}

/// Pick an account for a helper of `agent` on `model` (as the agent
/// names it; `None` for its default), and count the helper as placed
/// there until the feed is next written.
pub(in crate::plugins) fn pick_account(agent: Agent, model: Option<&str>) -> Pick {
    pick_among(agent, handoff::profiles(agent), model)
}

/// [`pick_account`] among `homes` only.
pub(in crate::plugins) fn pick_among(agent: Agent, homes: Vec<(u8, PathBuf)>, model: Option<&str>) -> Pick {
    let now = unix_now();
    if homes.is_empty() && matches!(agent, Agent::Claude | Agent::Codex) {
        return default_home(agent);
    }
    let numbers: Vec<u8> = homes.iter().map(|(n, _)| *n).collect();
    let model = feed_model(agent, model);
    let model = model.as_deref();
    let (pool, accounts, scene) = match agent {
        Agent::Gemini => return crate::plugins::gemini::helper_pick(&homes, model, now),
        Agent::Claude => {
            let (a, usage) = helper_pick::claude_accounts(&numbers, now);
            let scene = helper_pick::claude_scene(&numbers, &a.rooms, usage.as_ref(), now);
            (Pool::Claude, a, scene)
        }
        Agent::Kimi => {
            let a = crate::plugins::kimi::accounts::accounts_now(&numbers, now);
            let scene = helper_pick::rooms_scene(&a.rooms, &a.resets, model, now);
            (Pool::Kimi, a, scene)
        }
        Agent::Codex => {
            let a = codex_accounts(&numbers, marspot::cc_usage::read_codex(), now);
            let scene = helper_pick::rooms_scene(&a.rooms, &a.resets, model, now);
            (Pool::Codex, a, scene)
        }
    };
    // only Claude's choices are logged: `alloc_events` is Claude's
    let events = (agent == Agent::Claude).then(helper_pick::events_dir);
    helper_pick::placed(pool, |placed| choose(agent, &homes, &accounts, &scene, model, placed, now, events.as_deref()))
}

/// The pick among `homes` by what `accounts` says and what the core
/// reads of them, `scene`, counted in `placed`; with a line in the
/// `alloc_events` of `events`, where given, for each choice the core
/// made.  Where `scene`'s loads come from a view of the panes, the
/// helpers placed are settled against the helper panes it shows first:
/// the loads have those.
///
/// A plain helper the core would hold back for the Fable panes, were it
/// a pane that had to move, is started where it would go: a new helper
/// asked for now is not turned away for a shortfall that may never
/// come.  Its line, `held_back_start`, keeps what it cost.
#[allow(clippy::too_many_arguments)] // one choice, each input a separate reading
pub(in crate::plugins) fn choose(
    agent: Agent,
    homes: &[(u8, PathBuf)],
    accounts: &Accounts,
    scene: &Scene,
    model: Option<&str>,
    placed: &mut Placed,
    now: i64,
    events: Option<&Path>,
) -> Pick {
    match (&scene.shown, agent) {
        (Some(shown), _) => placed.settle(shown, now),
        // no view yet: a later feed does not say which helpers it counts
        (None, Agent::Claude) => placed.expire(now),
        (None, _) => placed.feed(accounts.feed_at, now),
    }
    let class = class_of(agent, &accounts.rooms, model);
    // Claude's helpers go by the one objective; the others know no restricted window
    let rule = if agent == Agent::Claude { Rule::waste(0) } else { Rule::Bands };
    let chosen = |kind: &str, n: u8, inputs: &Inputs| {
        if let Some(dir) = events {
            helper_pick::record_new(dir, now, kind, n, inputs);
        }
        crate::agentsvc::sched::once(&Made { agent: agent.key(), sid: None, inputs, rule, did: Some((n, Why::Out.word())) }, None);
        Some(n)
    };
    let rate = match (agent, class) {
        (Agent::Claude, Class::Restricted) => RHO0 * helper_pick::fable_model().map_or(1.0, |(_, k)| k),
        _ => RHO0,
    };
    let to = match place_by(scene, Mover { class, rate, tokens: None }, None, &placed.counts(), rule) {
        Placement::To(n, inputs) => chosen("placed", n, &inputs),
        // only the bands, which no Claude helper is placed by, hold one back
        Placement::HeldBack(n, inputs) => chosen("held_back_start", n, &inputs),
        Placement::Nowhere => helper_pick::unread_pick(&accounts.rooms, None, model, placed),
    };
    match to {
        Some(n) => {
            placed.note(n, class, now);
            let dir = homes.iter().find(|(h, _)| *h == n).map(|(_, d)| d.clone()).unwrap_or_default();
            Pick::Account(account(agent, n, dir))
        }
        None => {
            let none = Resets::default();
            let at = |n: u8, room: &Room| {
                let r = accounts.resets.iter().find(|(h, _)| *h == n).map_or(&none, |(_, r)| r);
                helper_pick::recovers_at(room, r, model, now)
            };
            Pick::NoAccount(accounts.rooms.iter().map(|(n, room)| Recovery { number: *n, at: at(*n, room) }).collect())
        }
    }
}

/// The model a Claude helper started with none asked for runs: the
/// Fable model this machine's Fable panes run, where a Fable helper on
/// the account it would be placed on costs less than a plain one on
/// its own, else none (the account's own default).  Only for helpers: a
/// pane the person started keeps the model it was started with.
pub(in crate::plugins) fn claude_helper_model() -> Option<String> {
    let numbers: Vec<u8> = handoff::profiles(Agent::Claude).iter().map(|(n, _)| *n).collect();
    if numbers.is_empty() {
        return None;
    }
    let (model, week_cost) = helper_pick::fable_model()?;
    let now = unix_now();
    let (accounts, usage) = helper_pick::claude_accounts(&numbers, now);
    let scene = helper_pick::claude_scene(&numbers, &accounts.rooms, usage.as_ref(), now);
    fable_saves(&scene, week_cost).then_some(model)
}

/// How long a helper is expected to run, hours: its work is a task, not
/// a pane people keep for days.
const HELPER_STAY_H: f64 = 2.0;

/// A Fable helper, whose work costs `week_cost` times the week a plain
/// helper's does, takes no more from the other panes than a plain one.
pub(in crate::plugins) fn fable_saves(scene: &Scene, week_cost: f64) -> bool {
    let p = waste::Params { stay_h: HELPER_STAY_H, ..waste::PARAMS };
    waste::restricted_helper(&scene.accounts, &scene.loads, RHO0, RHO0 * week_cost, scene.kappa, HELPER_LAST_H, &p)
}

/// The class a helper of `agent` on `model` is placed as.
fn class_of(agent: Agent, rooms: &[(u8, Room)], model: Option<&str>) -> Class {
    match agent {
        Agent::Claude => helper_pick::claude_class(model),
        _ => helper_pick::class_in(rooms, model),
    }
}

/// The recoveries of the claude accounts when every one of them
/// refuses `model`; None while one can take it.  Reads the feed and
/// places nothing.
pub(in crate::plugins) fn claude_all_refused(model: Option<&str>) -> Option<Vec<Recovery>> {
    let homes = handoff::profiles(Agent::Claude);
    let numbers: Vec<u8> = homes.iter().map(|(n, _)| *n).collect();
    if numbers.is_empty() {
        return None;
    }
    let now = unix_now();
    let (accounts, usage) = helper_pick::claude_accounts(&numbers, now);
    let scene = helper_pick::claude_scene(&numbers, &accounts.rooms, usage.as_ref(), now);
    let model = feed_model(Agent::Claude, model);
    match choose(Agent::Claude, &homes, &accounts, &scene, model.as_deref(), &mut Placed::default(), now, None) {
        Pick::NoAccount(r) => Some(r),
        Pick::Account(_) => None,
    }
}

pub(in crate::plugins) fn account(agent: Agent, number: u8, dir: PathBuf) -> HelperAccount {
    let var = match agent {
        Agent::Claude => "CLAUDE_CONFIG_DIR",
        Agent::Codex => "CODEX_HOME",
        Agent::Gemini => "GEMINI_CLI_HOME",
        Agent::Kimi => "KIMI_CODE_HOME",
    };
    // gemini's and kimi's number 0 is the default home, which takes no
    // variable; claude and codex list numbered profiles only
    let default = number == 0 && matches!(agent, Agent::Gemini | Agent::Kimi);
    let env = match dir.to_str() {
        Some(d) if !default => vec![(var, d.to_string())],
        _ => Vec::new(),
    };
    HelperAccount { number, dir, env }
}

/// No profile of `agent` exists: its own default home, which is where
/// the user's plain `claude` or `codex` runs.
fn default_home(agent: Agent) -> Pick {
    let dir = match agent {
        Agent::Claude => crate::plugins::claudecode::config_dir_or_default(None),
        Agent::Codex => std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(".codex"),
        Agent::Gemini | Agent::Kimi => return Pick::NoAccount(Vec::new()),
    };
    Pick::Account(HelperAccount { number: 0, dir, env: Vec::new() })
}

static GEMINI_NEXT: AtomicUsize = AtomicUsize::new(0);

pub(in crate::plugins) fn take_turns(agent: Agent, homes: &[(u8, PathBuf)]) -> Pick {
    if homes.is_empty() {
        return Pick::NoAccount(Vec::new());
    }
    let i = GEMINI_NEXT.fetch_add(1, Ordering::Relaxed) % homes.len();
    let (n, dir) = homes[i].clone();
    Pick::Account(account(agent, n, dir))
}

/// The model as the feed labels its cap.  Codex buckets are named by
/// the model's last word (`gpt-5.3-codex-spark` is `spark`).
pub(in crate::plugins) fn feed_model(agent: Agent, model: Option<&str>) -> Option<String> {
    let m = model?.to_ascii_lowercase();
    Some(match agent {
        Agent::Codex => m.rsplit('-').next().unwrap_or(&m).to_string(),
        Agent::Claude => m.strip_prefix("claude-").unwrap_or(&m).to_string(),
        _ => m,
    })
}

/// The Codex profiles `numbers` by the Codex feed.  Its account-wide
/// weekly is the week; each other bucket is a model's own cap, with
/// the five-hour bucket, which meters Spark alone, folded into Spark's.
pub(in crate::plugins) fn codex_accounts(numbers: &[u8], usage: Option<marspot::cc_usage::CodexUsage>, now: i64) -> Accounts {
    let feed_at = usage.as_ref().map_or(0, |u| u.generated_at);
    let rows = usage.map(|u| u.accounts).unwrap_or_default();
    let named = |n: u8| if n == 0 { "Codex".to_string() } else { format!("Codex {n}") };
    let mut accounts = Accounts { feed_at, ..Accounts::default() };
    for &n in numbers {
        let row = rows.iter().find(|a| a.name == named(n));
        accounts.rooms.push((n, row.map_or(Room::Unknown, |a| codex_room(a, feed_at, now))));
        accounts.resets.push((n, row.map(codex_resets).unwrap_or_default()));
    }
    accounts
}

fn codex_room(a: &CxAccount, measured_at: i64, now: i64) -> Room {
    let window = |label: &str| a.windows.iter().find(|w| w.label == label);
    let live = |w: &marspot::cc_usage::CcWindow| if w.reset.is_some_and(|r| r > 0 && r <= now) { Some(0.0) } else { w.util };
    let week = window("astra");
    let five = window("5h").and_then(live);
    let models = a
        .windows
        .iter()
        .filter(|w| w.label != "astra" && w.label != "5h")
        .map(|w| {
            let util = live(w);
            let util = if w.label == "spark" { five.map_or(util, |f| Some(util.map_or(f, |u| u.max(f)))) } else { util };
            (w.label.clone(), util)
        })
        .collect();
    Room::Known {
        status: a.status.clone(),
        util_5h: None,
        util_7d: week.and_then(live),
        reset_7d: week.and_then(|w| w.reset).filter(|r| *r > now).unwrap_or(now + 7 * 86_400),
        models,
        models_measured_at: measured_at,
        measured_at,
    }
}

fn codex_resets(a: &CxAccount) -> Resets {
    let window = |label: &str| a.windows.iter().find(|w| w.label == label);
    // Spark's cap is whichever of its two windows is fuller
    let five = window("5h");
    let spark_by_five = |w: &marspot::cc_usage::CcWindow| five.is_some_and(|f| f.util.unwrap_or(0.0) >= w.util.unwrap_or(0.0));
    let models = a
        .windows
        .iter()
        .filter(|w| w.label != "astra" && w.label != "5h")
        .filter_map(|w| {
            let by = if w.label == "spark" && spark_by_five(w) { five? } else { w };
            Some((w.label.clone(), by.reset?))
        })
        .collect();
    let reset = |label: &str| window(label).and_then(|w| w.reset).unwrap_or(0);
    Resets { five_hour: 0, week: reset("astra"), models }
}

#[cfg(test)]
#[path = "for_helpers_tests.rs"]
mod tests;

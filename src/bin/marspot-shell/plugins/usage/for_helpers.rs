//! Which account a new helper runs on, for each of the four agents.
//!
//! Claude, Kimi and Codex by their usage feeds, with the roomiest rule
//! (`quota::roomiest`); Gemini writes no usage anywhere, so its homes
//! take turns.  Every call reads the profiles and feeds from disk, so
//! it belongs off the shell's main loop.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use marspot::cc_usage::CxAccount;

use crate::plugins::claudecode::quota::roomiest::{self, Accounts, Pool};
use crate::plugins::claudecode::quota::Room;
use crate::plugins::claudecode::rooms::unix_now;
use crate::plugins::claudecode::seen::Resets;
use crate::plugins::handoff::{self, Agent};

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
    let (pool, accounts) = match agent {
        Agent::Gemini => return take_turns(agent, &homes),
        Agent::Claude => (Pool::Claude, roomiest::claude_accounts(&numbers, now)),
        Agent::Kimi => (Pool::Kimi, crate::plugins::kimi::accounts::accounts_now(&numbers, now)),
        Agent::Codex => (Pool::Codex, codex_accounts(&numbers, marspot::cc_usage::read_codex(), now)),
    };
    let model = feed_model(agent, model);
    roomiest::placed(pool, |placed| choose(agent, &homes, &accounts, model.as_deref(), placed, now))
}

/// The pick among `homes` by what `accounts` says, counted in `placed`.
pub(in crate::plugins) fn choose(
    agent: Agent,
    homes: &[(u8, PathBuf)],
    accounts: &Accounts,
    model: Option<&str>,
    placed: &mut roomiest::Placed,
    now: i64,
) -> Pick {
    placed.feed(accounts.feed_at, now);
    match roomiest::roomiest(&accounts.rooms, placed, model) {
        Some(n) => {
            placed.note(n);
            let dir = homes.iter().find(|(h, _)| *h == n).map(|(_, d)| d.clone()).unwrap_or_default();
            Pick::Account(account(agent, n, dir))
        }
        None => {
            let none = Resets::default();
            let at = |n: u8, room: &Room| {
                let r = accounts.resets.iter().find(|(h, _)| *h == n).map_or(&none, |(_, r)| r);
                roomiest::recovers_at(room, r, model, now)
            };
            Pick::NoAccount(accounts.rooms.iter().map(|(n, room)| Recovery { number: *n, at: at(*n, room) }).collect())
        }
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
    let accounts = roomiest::claude_accounts(&numbers, now);
    let model = feed_model(Agent::Claude, model);
    match choose(Agent::Claude, &homes, &accounts, model.as_deref(), &mut roomiest::Placed::default(), now) {
        Pick::NoAccount(r) => Some(r),
        Pick::Account(_) => None,
    }
}

/// `P1 at 2026-11-23T14:00:00+00:00, P2 at an unknown time`.
pub(in crate::plugins) fn recoveries_text(r: &[Recovery]) -> String {
    let one = |x: &Recovery| {
        let name = if x.number == 0 { "default".to_string() } else { format!("P{}", x.number) };
        x.at.map_or_else(|| format!("{name} at an unknown time"), |t| format!("{name} at {}", super::feed::iso_utc(t)))
    };
    r.iter().map(one).collect::<Vec<_>>().join(", ")
}

/// The soonest known recovery among `r`, as text.
pub(in crate::plugins) fn earliest_text(r: &[Recovery]) -> Option<String> {
    r.iter().filter_map(|x| x.at).min().map(super::feed::iso_utc)
}

fn account(agent: Agent, number: u8, dir: PathBuf) -> HelperAccount {
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

fn take_turns(agent: Agent, homes: &[(u8, PathBuf)]) -> Pick {
    if homes.is_empty() {
        return Pick::NoAccount(Vec::new());
    }
    let i = GEMINI_NEXT.fetch_add(1, Ordering::Relaxed) % homes.len();
    let (n, dir) = homes[i].clone();
    Pick::Account(account(agent, n, dir))
}

/// The model as the feed labels its cap.  Codex buckets are named by
/// the model's last word (`gpt-5.3-codex-spark` is `spark`).
fn feed_model(agent: Agent, model: Option<&str>) -> Option<String> {
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

//! claude's and codex's login for a container: the account's access
//! token, read and never refreshed, handed over in a file only this user
//! can read.  A refresh from outside the command line would log the
//! account out, so only an account whose token outlives the helper's
//! budget is taken.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marspot_peer::control::{AgentKind, ErrorCode, Reject};

use super::super::launch::{Account, agent_of};
use super::{codex_auth, records};
use crate::plugins::handoff;
use crate::plugins::usage::for_helpers::{self, Pick};

pub(super) use crate::plugins::usage::for_helpers::Login as ClaudeLogin;

/// What a container is handed of an account.
pub(super) enum Login {
    Claude(ClaudeLogin),
    Codex(codex_auth::Auth),
}

impl Login {
    pub fn expires_at(&self) -> Option<i64> {
        match self {
            Login::Claude(l) => l.expires_at,
            Login::Codex(a) => a.expires_at,
        }
    }
}

/// What a token has to have left past the budget.
const MARGIN_S: i64 = 10 * 60;
/// How long the keychain may take: a locked one asks on the screen.
const KEYCHAIN: Duration = Duration::from_secs(10);

/// How long each account's token lasts, in seconds; None: no login, or
/// one that does not say.
pub(super) type Left = Vec<(u8, Option<i64>)>;

/// The seconds a login that ends at `expires_at` has left at `now_ms`,
/// `cap` at most.
pub(super) fn left_s(expires_at: Option<i64>, now_ms: u64, cap: Option<i64>) -> Option<i64> {
    let left = (expires_at? - now_ms as i64) / 1000;
    Some(cap.map_or(left, |c| left.min(c)))
}

/// For tests: the most a token is taken to have left.
fn cap() -> Option<i64> {
    std::env::var("MARSPOT_TEST_TOKEN_LEFT_S").ok()?.parse().ok()
}

pub(super) fn lasts(left: Option<i64>, budget_min: u32) -> bool {
    left.is_some_and(|s| s >= i64::from(budget_min) * 60 + MARGIN_S)
}

/// Why none will do, with what each has left.
pub(super) fn none_text(agent: AgentKind, left: &Left, budget_min: u32) -> String {
    let one = |(n, l): &(u8, Option<i64>)| {
        let name = if *n == 0 { "default".to_string() } else { format!("P{n}") };
        match l {
            None => format!("{name} has no login that can be read"),
            Some(s) if *s <= 0 => format!("{name} has expired"),
            Some(s) => format!("{name} has {} min left", s / 60),
        }
    };
    let name = agent.as_str();
    let each = if left.is_empty() { format!("there is no {name} account") } else { left.iter().map(one).collect::<Vec<_>>().join(", ") };
    let min = budget_min + 10;
    format!(
        "a {name} helper in a container needs an account whose login lasts {min} min (the budget and 10 min): {each}. \
         A login is renewed when {name} runs on that account on the host; or use isolation host, or another agent"
    )
}

/// The numbered accounts, or the default one when there are none.
fn homes(agent: AgentKind) -> Vec<(u8, PathBuf)> {
    let homes = handoff::profiles(agent_of(agent));
    if !homes.is_empty() {
        return homes;
    }
    match agent {
        AgentKind::Claude => vec![(0, crate::plugins::claudecode::config_dir_or_default(None))],
        AgentKind::Codex => std::env::var_os("HOME").map(|h| (0, PathBuf::from(h).join(".codex"))).into_iter().collect(),
        _ => homes,
    }
}

fn read(agent: AgentKind, home: &Path, within: Duration) -> Option<Login> {
    match agent {
        AgentKind::Codex => codex_auth::read(home).map(Login::Codex),
        _ => for_helpers::claude_login(home, within).map(Login::Claude),
    }
}

/// The account for a helper of `agent` (claude or codex) of `budget_min`
/// on `model`, and its login.
pub(super) fn pick(agent: AgentKind, asked: Option<u8>, model: Option<&str>, budget_min: u32, until: Instant) -> Result<(Account, Login), Reject> {
    let name = agent.as_str();
    let mut homes = homes(agent);
    if let Some(n) = asked {
        homes.retain(|(p, _)| *p == n);
        if homes.is_empty() {
            return Err(Reject::new(ErrorCode::NoAccount, format!("{name} has no account P{n} on this machine")));
        }
    }
    let now = records::now_ms();
    let mut logins: Vec<(u8, Option<Login>)> =
        homes.iter().map(|(n, home)| (*n, read(agent, home, KEYCHAIN.min(until.saturating_duration_since(Instant::now()))))).collect();
    // those after the one that took all of the time were not asked
    let late = if agent == AgentKind::Claude && Instant::now() >= until { " (the keychain did not answer in time: it may be locked)" } else { "" };
    let left: Left = logins.iter().map(|(n, l)| (*n, l.as_ref().and_then(|l| left_s(l.expires_at(), now, cap())))).collect();
    let none = || Reject::new(ErrorCode::NoAccount, none_text(agent, &left, budget_min) + late);
    homes.retain(|(n, _)| left.iter().any(|(m, l)| m == n && lasts(*l, budget_min)));
    let account = match homes.first() {
        None => return Err(none()),
        Some((n, home)) if asked.is_some() || *n == 0 => Account { n: *n, home: home.clone() },
        Some(_) => match for_helpers::pick_among(agent_of(agent), homes, model) {
            Pick::Account(a) => Account { n: a.number, home: a.dir },
            Pick::NoAccount(r) => {
                let what = model.unwrap_or("the default model");
                let when = for_helpers::recoveries_text(&r);
                return Err(Reject::new(ErrorCode::NoAccount, format!("every {name} account whose login lasts refuses {what}; they take work again: {when}")));
            }
        },
    };
    let at = logins.iter().position(|(n, _)| *n == account.n).ok_or_else(none)?;
    logins.swap_remove(at).1.map(|l| (account, l)).ok_or_else(none)
}

/// The login of the account a helper of `agent` recorded as `label` is
/// on.
pub(super) fn of_label(agent: AgentKind, label: &str) -> Option<Login> {
    let n: u8 = label.strip_prefix('P').map_or(Some(0), |n| n.parse().ok())?;
    homes(agent).into_iter().find(|(p, _)| *p == n).and_then(|(_, home)| read(agent, &home, KEYCHAIN))
}

/// Hand `l` to the helper at `dir`: claude's in its env file, codex's in
/// its home; and write down when it ends.
pub(super) fn hand_over(dir: &Path, l: &Login) -> Result<(), Reject> {
    match l {
        Login::Claude(l) => write_env(dir, l),
        Login::Codex(a) => {
            let ends = ends(a.expires_at)?;
            let wrote = codex_auth::write(&dir.join("home"), a).and_then(|()| std::fs::write(dir.join("expires"), ends.to_string()));
            wrote.map_err(|e| Reject::new(ErrorCode::Internal, format!("the login file: {e}")))
        }
    }
}

/// When a login to be handed over ends: one that does not say cannot be
/// told apart from one that has ended.
fn ends(expires_at: Option<i64>) -> Result<i64, Reject> {
    expires_at.filter(|ms| *ms > 0).ok_or_else(|| Reject::new(ErrorCode::Internal, "the account's token has no end"))
}

/// Write `l` where `docker run --env-file` reads it, and when it ends.
pub(super) fn write_env(dir: &Path, l: &ClaudeLogin) -> Result<(), Reject> {
    let internal = |m: String| Reject::new(ErrorCode::Internal, m);
    if l.token.is_empty() || !l.token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(internal("the account's login cannot go into an env file".into()));
    }
    let ends = ends(l.expires_at)?;
    let file = dir.join("env");
    let write = || {
        if file.exists() {
            std::fs::remove_file(&file)?;
        }
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&file)?;
        f.write_all(format!("CLAUDE_CODE_OAUTH_TOKEN={}\n", l.token).as_bytes())?;
        std::fs::write(dir.join("expires"), ends.to_string())
    };
    write().map_err(|e| internal(format!("the env file: {e}")))
}

/// When the login last handed to the helper at `dir` ends (ms after the
/// epoch).
pub(super) fn expires_ms(dir: &Path) -> Option<i64> {
    std::fs::read_to_string(dir.join("expires")).ok()?.trim().parse().ok().filter(|ms| *ms > 0)
}

/// Whether the login handed to the helper at `dir` still outlives
/// `budget_min`, by [`lasts`].
pub(super) fn still_lasts(dir: &Path, budget_min: u32) -> bool {
    lasts(left_s(expires_ms(dir), records::now_ms(), cap()), budget_min)
}

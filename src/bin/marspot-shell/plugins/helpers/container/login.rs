//! claude's login for a container: the account's access token, read and
//! never refreshed, handed over in a file only this user can read.  A
//! refresh from outside the command line would log the account out, so
//! only an account whose token outlives the helper's budget is taken.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marspot_peer::control::{ErrorCode, Reject};

use super::super::launch::Account;
use super::records;
use crate::plugins::handoff::{self, Agent};
use crate::plugins::usage::for_helpers::{self, Pick};

pub(super) use crate::plugins::usage::for_helpers::Login;

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
pub(super) fn none_text(left: &Left, budget_min: u32) -> String {
    let one = |(n, l): &(u8, Option<i64>)| {
        let name = if *n == 0 { "default".to_string() } else { format!("P{n}") };
        match l {
            None => format!("{name} has no login that can be read"),
            Some(s) if *s <= 0 => format!("{name} has expired"),
            Some(s) => format!("{name} has {} min left", s / 60),
        }
    };
    let each = if left.is_empty() { "there is no claude account".to_string() } else { left.iter().map(one).collect::<Vec<_>>().join(", ") };
    let min = budget_min + 10;
    format!(
        "a claude helper in a container needs an account whose login lasts {min} min (the budget and 10 min): {each}. \
         A login is renewed when claude runs on that account on the host; or use isolation host, or another agent"
    )
}

fn homes() -> Vec<(u8, PathBuf)> {
    let homes = handoff::profiles(Agent::Claude);
    if homes.is_empty() { vec![(0, crate::plugins::claudecode::config_dir_or_default(None))] } else { homes }
}

/// The account for a helper of `budget_min` on `model`, and its login.
pub(super) fn pick(asked: Option<u8>, model: Option<&str>, budget_min: u32, until: Instant) -> Result<(Account, Login), Reject> {
    let mut homes = homes();
    if let Some(n) = asked {
        homes.retain(|(p, _)| *p == n);
        if homes.is_empty() {
            return Err(Reject::new(ErrorCode::NoAccount, format!("claude has no account P{n} on this machine")));
        }
    }
    let now = records::now_ms();
    let mut logins: Vec<(u8, Option<Login>)> = homes
        .iter()
        .map(|(n, home)| (*n, for_helpers::claude_login(home, KEYCHAIN.min(until.saturating_duration_since(Instant::now())))))
        .collect();
    // those after the one that took all of the time were not asked
    let late = if Instant::now() >= until { " (the keychain did not answer in time: it may be locked)" } else { "" };
    let left: Left = logins.iter().map(|(n, l)| (*n, l.as_ref().and_then(|l| left_s(l.expires_at, now, cap())))).collect();
    let none = || Reject::new(ErrorCode::NoAccount, none_text(&left, budget_min) + late);
    homes.retain(|(n, _)| left.iter().any(|(m, l)| m == n && lasts(*l, budget_min)));
    let account = match homes.first() {
        None => return Err(none()),
        Some((n, home)) if asked.is_some() || *n == 0 => Account { n: *n, home: home.clone() },
        Some(_) => match for_helpers::pick_among(Agent::Claude, homes, model) {
            Pick::Account(a) => Account { n: a.number, home: a.dir },
            Pick::NoAccount(r) => {
                let what = model.unwrap_or("the default model");
                let when = for_helpers::recoveries_text(&r);
                return Err(Reject::new(ErrorCode::NoAccount, format!("every claude account whose login lasts refuses {what}; they take work again: {when}")));
            }
        },
    };
    let at = logins.iter().position(|(n, _)| *n == account.n).ok_or_else(none)?;
    logins.swap_remove(at).1.map(|l| (account, l)).ok_or_else(none)
}

/// The login of the account a helper recorded as `label` is on.
pub(super) fn of_label(label: &str) -> Option<Login> {
    let n: u8 = label.strip_prefix('P').map_or(Some(0), |n| n.parse().ok())?;
    homes().into_iter().find(|(p, _)| *p == n).and_then(|(_, home)| for_helpers::claude_login(&home, KEYCHAIN))
}

/// Write `l` where `docker run --env-file` reads it, and when it ends.
pub(super) fn write_env(dir: &Path, l: &Login) -> Result<(), Reject> {
    let internal = |m: String| Reject::new(ErrorCode::Internal, m);
    if l.token.is_empty() || !l.token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(internal("the account's login cannot go into an env file".into()));
    }
    let file = dir.join("env");
    let write = || {
        if file.exists() {
            std::fs::remove_file(&file)?;
        }
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&file)?;
        f.write_all(format!("CLAUDE_CODE_OAUTH_TOKEN={}\n", l.token).as_bytes())?;
        std::fs::write(dir.join("expires"), l.expires_at.unwrap_or(0).to_string())
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

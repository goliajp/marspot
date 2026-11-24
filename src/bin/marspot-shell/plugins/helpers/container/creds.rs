//! What of an account a gemini or kimi container is given: its
//! credential files, mounted to be written as they are renewed, and
//! nothing else of the account's directory.
//!
//! How each renews its login, measured with stand-in credentials:
//! - gemini writes a new file and renames it over the old one; where that
//!   is refused (a file mounted by itself is busy) it writes in place.
//!   There is no lock.  A renewal on the host replaces the file and the
//!   container keeps the one it has; google's refresh tokens do not
//!   change, so both go on.
//! - kimi renames within `credentials/`, under a lock directory in
//!   `oauth/`.  Both are mounted, the second because a refresh token is
//!   taken once: without the lock the slower of two renewals would log
//!   the account out.
//! - codex renames over `auth.json` and has no other way: mounted by
//!   itself, it loses the new refresh token after the old one was used.
//!   So it gets no credential file of the account: like claude, only its
//!   access token, in a login of its own (`codex_auth`).

use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use marspot_peer::control::{AgentKind, ErrorCode, Reject};

use super::super::launch::{Account, agent_of};
use crate::plugins::handoff;
use crate::plugins::usage::for_helpers::{self, Pick};

/// A path of the host, and where the container has it.
#[derive(Debug, PartialEq)]
pub(super) struct Bind {
    pub from: PathBuf,
    pub to: &'static str,
}

fn none(m: String) -> Reject {
    Reject::new(ErrorCode::NoAccount, m)
}

fn mine(m: &Metadata) -> bool {
    // SAFETY: geteuid has no preconditions.
    m.uid() == unsafe { libc::geteuid() }
}

/// A file with no other name: nothing else is written through it.
fn plain(m: &Metadata) -> bool {
    m.is_file() && mine(m) && m.nlink() == 1
}

/// The real path of directory `dir`, when every entry is `fit`.
fn only(dir: &Path, fit: impl Fn(&str, &Metadata) -> bool) -> Result<PathBuf, Reject> {
    let gone = |e: std::io::Error| none(format!("{}: {e}; log in, or use the account once on the host", dir.display()));
    let real = std::fs::canonicalize(dir).map_err(gone)?;
    if !std::fs::symlink_metadata(&real).is_ok_and(|m| m.is_dir() && mine(&m)) {
        return Err(none(format!("{} is not a directory of this user", dir.display())));
    }
    for e in std::fs::read_dir(&real).map_err(gone)? {
        let e = e.map_err(gone)?;
        if !e.metadata().is_ok_and(|m| fit(&e.file_name().to_string_lossy(), &m)) {
            let m = format!("{} holds {:?}, which is not part of a login; use isolation host", dir.display(), e.file_name());
            return Err(Reject::new(ErrorCode::NotSupported, m));
        }
    }
    Ok(real)
}

/// The credentials of `agent`'s account at `home`.
pub(super) fn of(agent: AgentKind, home: &Path) -> Result<Vec<Bind>, Reject> {
    match agent {
        AgentKind::Claude | AgentKind::Codex => Ok(Vec::new()),
        AgentKind::Gemini => {
            let file = home.join(".gemini/oauth_creds.json");
            let real = std::fs::canonicalize(&file).map_err(|e| none(format!("gemini has no login there: {}: {e}", file.display())))?;
            if !std::fs::symlink_metadata(&real).is_ok_and(|m| plain(&m)) {
                return Err(none(format!("{} does not lead to a plain file of this user", file.display())));
            }
            Ok(vec![Bind { from: real, to: "/home/node/.gemini/oauth_creds.json" }])
        }
        AgentKind::Kimi => {
            let dir = home.join("credentials");
            let creds = only(&dir, |n, m| plain(m) && (n.ends_with(".json") || n.contains(".json.tmp.")))?;
            let any = std::fs::read_dir(&creds).is_ok_and(|rd| rd.flatten().any(|e| e.file_name().to_string_lossy().ends_with(".json")));
            if !any {
                return Err(none(format!("kimi has no login in {}", dir.display())));
            }
            let locks = only(&home.join("oauth"), |n, m| mine(m) && (m.is_dir() && n.ends_with(".lock") || plain(m) && m.len() == 0))?;
            Ok(vec![Bind { from: creds, to: "/home/node/.kimi-code/credentials" }, Bind { from: locks, to: "/home/node/.kimi-code/oauth" }])
        }
    }
}

/// The account for a gemini or kimi helper among those whose login can
/// be mounted, and that login.
pub(super) fn pick(agent: AgentKind, asked: Option<u8>, model: Option<&str>) -> Result<(Account, Vec<Bind>), Reject> {
    let mut homes = handoff::profiles(agent_of(agent));
    let mut why = none(match asked {
        Some(n) => format!("{} has no account P{n} on this machine", agent.as_str()),
        None => format!("{} has no account on this machine", agent.as_str()),
    });
    if let Some(n) = asked {
        homes.retain(|(p, _)| *p == n);
    }
    let mut usable = Vec::new();
    for (n, home) in homes {
        match of(agent, &home) {
            Ok(binds) => usable.push((Account { n, home }, binds)),
            Err(r) => why = r,
        }
    }
    let Some((first, _)) = usable.first() else { return Err(why) };
    let n = if asked.is_some() {
        first.n
    } else {
        match for_helpers::pick_among(agent_of(agent), usable.iter().map(|(a, _)| (a.n, a.home.clone())).collect(), model) {
            Pick::Account(a) => a.number,
            Pick::NoAccount(r) => {
                let (what, when) = (model.unwrap_or("the default model"), for_helpers::recoveries_text(&r));
                return Err(none(format!("every {} account with a login refuses {what}; they take work again: {when}", agent.as_str())));
            }
        }
    };
    usable.into_iter().find(|(a, _)| a.n == n).ok_or(why)
}

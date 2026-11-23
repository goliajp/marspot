//! Helpers in a container: the pane runs `docker run -it …` and the
//! agent is inside it, with its working directory, a directory for its
//! result, a home made for it and its login, and nothing else of the
//! host.
//!
//! What the container may write is never run or followed as a link on
//! the host: its result directory is `share/` and its home is `home/` in
//! the helper's directory, and the script, the task and the token file
//! stay outside both.
//!
//! What remains open, and is accepted:
//! - The working directory is written to: a build script, a package hook
//!   or an editor's settings left there run on the host when someone runs
//!   them.  git's config and hooks are read-only where they were there
//!   when the container started, and so is the `.git` file of a worktree,
//!   whose repository is not in the container, so git does not work in it.
//!   A `.git` made or moved aside inside, a `.git`, config or `hooks` that
//!   is a link or was not there, and the other files git reads in it
//!   (`config.worktree`, `info/`) are not.  Refusing those would close
//!   nothing: `.git` itself can be moved aside.
//! - The script checks that the working directory's path still leads to
//!   the directory that was looked at; docker opens it again a moment
//!   later, by which time a program on the host may have changed it.
//! - The login itself: claude's access token; gemini's and kimi's whole
//!   login, refresh token included, which the container may use or spoil.
//!   Of kimi's, what is no regular file in `credentials/` and `oauth/` is
//!   moved beside them when the helper ends (`pending`).  Regular files
//!   stay whatever they hold and are called, hard links between them and
//!   `.lock` directories too: an odd one keeps the account from other
//!   container helpers, and a kept lock holds up a renewal, until someone
//!   looks.  Until the helper ends nothing is moved.
//! - The network.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marspot_peer::control::{AgentKind, ErrorCode, HelperStartArgs, Isolation, Reject};

use super::launch::{self, Account};
use super::table::Helper;
use super::{Shared, records};

mod cleanup;
mod creds;
mod docker;
mod expired;
mod guard;
mod home;
mod image;
mod login;
mod pending;
mod prepare;
mod script;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_away;
#[cfg(test)]
mod tests_gone;

pub(super) use cleanup::{remove_later, sweep};
pub(super) use expired::{at_stop, restart};
pub(super) use pending::pending;
pub(super) use prepare::prepare;

/// Where the working directory is mounted.
const WORK: &str = "/work";
/// Where the helper's result directory is mounted.
const SHARE: &str = "/marspot";

/// How long the checks before a pane opens may take together.
const CHECKS: Duration = Duration::from_secs(15);

/// A container helper about to be started.
pub(super) struct Ready {
    docker: PathBuf,
    pub image_there: bool,
    pub account: Account,
    login: Option<login::Login>,
    binds: Vec<creds::Bind>,
    /// The working directory's real path.
    cwd: PathBuf,
}

/// What has to be so before a pane is opened for it.
pub(super) fn check(a: &HelperStartArgs, cwd: &str) -> Result<Ready, Reject> {
    if a.resume_last {
        let why = "a helper in a container cannot go on with a conversation of the host; leave `resume` out, or use isolation host";
        return Err(Reject::new(ErrorCode::NotSupported, why));
    }
    creds::supported(a.agent)?;
    let until = Instant::now() + CHECKS;
    let cwd = guard::cwd(Path::new(cwd))?;
    script::mountable(&cwd)?;
    let docker = docker::up(until)?;
    let (account, login, binds) = match a.agent {
        AgentKind::Claude => {
            let (account, login) = login::pick(a.profile, a.model.as_deref(), a.budget_minutes, until)?;
            (account, Some(login), Vec::new())
        }
        agent => {
            let (account, binds) = creds::pick(agent, a.profile, a.model.as_deref())?;
            (account, None, binds)
        }
    };
    let image_there = image::present(&docker, until)?;
    Ok(Ready { image_there, docker, account, login, binds, cwd })
}

/// Get helper `sid`'s files ready; from here on it may have a container.
pub(super) fn begin(s: &Shared, sid: u64, a: &HelperStartArgs, parent: &str, mode: &str, r: &Ready) -> Result<(), Reject> {
    let given: Vec<&Path> = r.binds.iter().map(|b| b.from.as_path()).filter(|p| p.is_dir()).collect();
    pending::add(&s.root(), sid, &given).map_err(|e| Reject::new(ErrorCode::Internal, e.to_string()))?;
    write_files(&s.dir_of(sid), sid, a, parent, mode, r)
}

/// Write what the helper's pane and container read, into `dir`.
fn write_files(dir: &Path, sid: u64, a: &HelperStartArgs, parent: &str, mode: &str, r: &Ready) -> Result<(), Reject> {
    let io = |e: std::io::Error| Reject::new(ErrorCode::Internal, e.to_string());
    std::fs::create_dir_all(dir.join("share")).map_err(io)?;
    let spec = script::Spec { docker: &r.docker, dir, sid, agent: a.agent, binds: &r.binds, model: a.model.as_deref(), mode, cwd: &r.cwd };
    home::write(a.agent, &dir.join("home"), &r.account.home).map_err(io)?;
    for b in &r.binds {
        // where the runtime puts a directory's mount
        if let Some(inner) = b.to.strip_prefix(script::home_at(a.agent)).filter(|_| b.from.is_dir()) {
            std::fs::create_dir_all(dir.join("home").join(inner.trim_start_matches('/'))).map_err(io)?;
        }
    }
    std::fs::write(dir.join("run.sh"), script::run(&spec, false)?).map_err(io)?;
    image::name_in(dir).map_err(io)?;
    if let Some(l) = &r.login {
        std::fs::write(dir.join("resume.sh"), script::run(&spec, true)?).map_err(io)?;
        login::write_env(dir, l)?;
    }
    let result = Path::new(SHARE).join("result.md");
    // it has no SendMessage: its questions go by the result file
    std::fs::write(dir.join("task.md"), launch::task_text(parent, WORK, a.budget_minutes, &result, &a.task, true)).map_err(io)
}

impl Helper {
    pub fn contained(&self) -> bool {
        self.started.isolation == Isolation::Container
    }
}

impl Shared {
    pub(super) fn contained(&self, sid: u64) -> bool {
        self.table().map.get(&sid).is_some_and(Helper::contained)
    }

    /// The file helper `sid` writes its result to.
    pub(super) fn result_path(&self, sid: u64) -> PathBuf {
        let dir = self.dir_of(sid);
        if self.contained(sid) { dir.join("share/result.md") } else { dir.join("result.md") }
    }

    /// The directory the agent of helper `sid` was given besides its
    /// working directory, as the agent names it.
    pub(super) fn own_dir_shown(&self, sid: u64) -> PathBuf {
        if self.contained(sid) { PathBuf::from(SHARE) } else { self.dir_of(sid) }
    }
}

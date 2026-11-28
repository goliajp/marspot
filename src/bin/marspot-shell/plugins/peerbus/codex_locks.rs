//! Every Codex home's `thread-writer-locks` as a link to one directory
//! in `~/.codex-shared`, the way `sessions` already is.
//!
//! Codex holds a writer lock per thread (`flock` on
//! `thread-writer-locks/<thread>.lock`), but only within one home: with
//! a directory per home, two homes' app-servers both load a thread and
//! both write its rollout.  With the directory shared the second one is
//! refused (`thread <id> already has an active writer`).
//!
//! A home is converted only while nothing of it runs (`busy`): the
//! directory is replaced by the link, then its lock files are moved
//! into the shared directory, one the shared directory already has
//! staying as it is.  A home that is busy is skipped and looked at again on the
//! next pass of the configuration.  `restore` puts a directory of the
//! home's own back and marks the home so that no pass converts it again.

use std::path::{Path, PathBuf};

mod migrate;
mod probe;
use migrate::{leftovers, move_into, recover};
use probe::{daemon_pid, held, running_under};
#[cfg(test)]
use migrate::{Step, move_into_seen};

pub(super) const DIR: &str = "thread-writer-locks";
/// In a home that keeps its own locks; `restore` writes it, `share`
/// removes it.
pub(super) const KEEP_OWN: &str = ".marspot-own-thread-writer-locks";
/// Where entries that could not be merged are left; a later `share`
/// that leaves some too numbers its directory one above the highest
/// there (`LEFT.2`, ...).
pub(super) const LEFT: &str = "thread-writer-locks.before-shared";
/// Codex takes it, in the lock directory, before it makes or takes a
/// thread's lock file.
const COORD: &str = ".coordination.lock";
const SOCKET: &str = "app-server-control/app-server-control.sock";
const PID_FILE: &str = "app-server-daemon/daemon.pid";

/// Convert every home under `~/.codex-shared`.  Returns lines worth
/// logging: a home converted, one skipped and why.
pub(super) fn reconcile() -> Vec<String> {
    // a sandbox shares the developer's HOME, and these are the
    // developer's own Codex homes
    if std::env::var_os("MARSPOT_DEV_SANDBOX").is_some() {
        return Vec::new();
    }
    let Some(home) = std::env::var_os("HOME") else { return Vec::new() };
    let shared = Path::new(&home).join(".codex-shared");
    if !shared.is_dir() {
        return Vec::new();
    }
    homes(Path::new(&home))
        .iter()
        .filter_map(|h| share(h, &shared).note().map(|n| format!("{}: {n}", h.display())))
        .collect()
}

/// `~/.codex` and every `~/.codex-profile-N`, each real directory once.
pub(super) fn homes(home: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in super::codex_config::home_dirs(home) {
        if let Ok(real) = dir.canonicalize()
            && real.is_dir()
            && !out.contains(&real)
        {
            out.push(real);
        }
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// The home's sessions are not the shared ones: not a profile.
    NotShared,
    /// Restored by hand; left with its own directory.
    KeepsOwn,
    Already,
    Busy(String),
    /// Something of the home this does not replace.
    LeftAlone(String),
    /// Converted: entries moved, entries the shared directory already
    /// had, entries left in a `LEFT` directory.
    Shared { moved: usize, duplicates: usize, left: usize },
    Failed(String),
}

impl Outcome {
    fn note(&self) -> Option<String> {
        match self {
            Outcome::NotShared | Outcome::KeepsOwn | Outcome::Already => None,
            Outcome::Busy(why) => Some(format!("{DIR} not shared yet: {why}")),
            Outcome::LeftAlone(why) => Some(format!("{DIR} left alone: {why}")),
            Outcome::Shared { moved, duplicates, left } => Some(format!(
                "{DIR} shared: {moved} moved, {duplicates} already there, {left} left in {LEFT}"
            )),
            Outcome::Failed(why) => Some(format!("{DIR} not shared: {why}")),
        }
    }
}

/// Why `home` must not be changed now, if it must not: its daemon's
/// pid file names a process that is alive and started when the file
/// says, its app-server answers on the control socket, or a lock in it
/// is held (a codex with an app-server of its own, outside the daemon).
/// The socket is tried only without a live daemon, so a busy home's
/// app-server is not sent a connection on every pass.  On a home whose
/// directory is already the link only the first two are looked at: the
/// shared lock files are every home's.
pub(super) fn busy(home: &Path) -> Option<String> {
    if let Some(pid) = daemon_pid(home) {
        return Some(format!("its daemon (pid {pid}) is alive"));
    }
    let sock = home.join(SOCKET);
    // codex makes it a link into /tmp; a socket address has room for
    // about a hundred bytes, and the home's path may be longer
    let real = sock.canonicalize().unwrap_or_else(|_| sock.clone());
    if marspot_term::uds_path::connect(&real).is_ok() {
        return Some(format!("its app-server answers on {}", sock.display()));
    }
    let dir = home.join(DIR);
    let real_dir = std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir());
    if real_dir && let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            if held(&e.path()) {
                return Some(format!("{} is held", e.path().display()));
            }
        }
    }
    None
}

/// Both name the same existing file or directory.
fn same(a: &Path, b: &Path) -> bool {
    matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

/// The link `link` names `target`, which need not exist: a relative
/// link is taken from the link's directory, and where the target is
/// missing the two are compared by their parents' real paths.
fn points_to(link: &Path, target: &Path) -> bool {
    let Ok(to) = std::fs::read_link(link) else { return false };
    let to = link.parent().map_or_else(|| to.clone(), |d| d.join(&to));
    same(&to, target) || resolved(&to) == resolved(target)
}

/// `p` without `.` and `..`, under its parent's real path when there
/// is one.
fn resolved(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    match (out.parent().and_then(|d| d.canonicalize().ok()), out.file_name()) {
        (Some(d), Some(n)) => d.join(n),
        _ => out,
    }
}

/// Convert `home` to the shared directory of `shared` (`~/.codex-shared`).
pub(super) fn share(home: &Path, shared: &Path) -> Outcome {
    if !same(&home.join("sessions"), &shared.join("sessions")) {
        return Outcome::NotShared;
    }
    if home.join(KEEP_OWN).exists() {
        return Outcome::KeepsOwn;
    }
    let dir = home.join(DIR);
    let target = shared.join(DIR);
    let meta = std::fs::symlink_metadata(&dir).ok();
    if meta.as_ref().is_some_and(|m| m.file_type().is_symlink()) {
        if points_to(&dir, &target) {
            // a shared directory removed by hand comes back: codex makes
            // no lock file through a link to nothing
            if let Err(e) = std::fs::create_dir_all(&target) {
                return Outcome::Failed(format!("{}: {e}", target.display()));
            }
            return match recover(home, &target) {
                Ok(None) => Outcome::Already,
                Ok(Some((moved, duplicates, left))) => Outcome::Shared { moved, duplicates, left },
                Err(o) => o,
            };
        }
        return match std::fs::read_link(&dir) {
            Ok(to) => Outcome::LeftAlone(format!("a link to {}", to.display())),
            Err(e) => Outcome::Failed(e.to_string()),
        };
    }
    if meta.as_ref().is_some_and(|m| !m.is_dir()) {
        return Outcome::LeftAlone("not a directory".into());
    }
    if let Some(why) = busy(home) {
        return Outcome::Busy(why);
    }
    if let Err(e) = std::fs::create_dir_all(&target) {
        return Outcome::Failed(format!("{}: {e}", target.display()));
    }
    let (m, d, l) = match recover(home, &target) {
        Ok(c) => c.unwrap_or_default(),
        Err(o) => return o,
    };
    match move_into(home, &target, meta.is_some()) {
        Outcome::Shared { moved, duplicates, left } => Outcome::Shared { moved: moved + m, duplicates: duplicates + d, left: left + l },
        o => o,
    }
}

/// Give `home` a directory of its own again, marked so that no pass
/// shares it.  Its locks start empty: Codex makes a lock file when it
/// loads a thread.  On the link `busy` sees only the daemon and the
/// socket, not the shared lock files, which may be another home's, so
/// any process running under the home refuses it too.
/// Entries an earlier `share` could not merge stay where it left them,
/// and so does a directory one stopped in the middle of; the answer
/// names those directories.
pub(super) fn restore(home: &Path, shared: &Path) -> Result<String, String> {
    let dir = home.join(DIR);
    let meta = std::fs::symlink_metadata(&dir).ok();
    let linked = meta.as_ref().is_some_and(|m| m.file_type().is_symlink());
    if linked && !points_to(&dir, &shared.join(DIR)) {
        return Err(format!("{} is a link this did not make; left alone", dir.display()));
    }
    if let Some(why) = busy(home).or_else(|| running_under(home)) {
        return Err(format!("not restored: {why}"));
    }
    std::fs::write(home.join(KEEP_OWN), "").map_err(|e| e.to_string())?;
    let done = if !linked && meta.is_some() {
        "already has its own directory"
    } else {
        if linked {
            std::fs::remove_file(&dir).map_err(|e| e.to_string())?;
        }
        std::fs::create_dir(&dir).map_err(|e| e.to_string())?;
        "restored: a directory of its own"
    };
    let left: Vec<String> = leftovers(home).iter().map(|p| p.display().to_string()).collect();
    if left.is_empty() {
        return Ok(done.into());
    }
    Ok(format!("{done}; entries not merged earlier are in {}", left.join(", ")))
}

/// `marspot-shell --codex-locks share|restore|check <CODEX_HOME>…`, for
/// `$HOME/.codex-shared`.  Exit status 1 when any home did not end
/// where it was asked to.
pub(crate) fn cli_main() -> i32 {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let (Some(op), Some(home)) = (args.first(), std::env::var_os("HOME")) else {
        eprintln!("usage: marspot-shell --codex-locks share|restore|check <CODEX_HOME>...");
        return 2;
    };
    let shared = Path::new(&home).join(".codex-shared");
    let mut rc = 0;
    for h in &args[1..] {
        let h = Path::new(h);
        let line = match op.as_str() {
            "share" => {
                let _ = std::fs::remove_file(h.join(KEEP_OWN));
                let o = share(h, &shared);
                if !matches!(o, Outcome::Shared { .. } | Outcome::Already) {
                    rc = 1;
                }
                o.note().unwrap_or_else(|| format!("{o:?}"))
            }
            "restore" => restore(h, &shared).unwrap_or_else(|e| {
                rc = 1;
                e
            }),
            "check" => busy(h).map_or_else(|| "not busy".into(), |w| format!("busy: {w}")),
            _ => return 2,
        };
        println!("{}: {line}", h.display());
    }
    rc
}

#[cfg(test)]
#[path = "codex_locks_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "codex_locks_race_tests.rs"]
mod race_tests;

#[cfg(test)]
#[path = "codex_locks_recover_tests.rs"]
mod recover_tests;

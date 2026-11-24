//! What `busy` and `restore` look at in a home: its daemon's pid file,
//! the lock files and the processes that run under it.

use super::PID_FILE;
use std::path::Path;

/// The pid of the daemon the pid file names, if that process is alive
/// and is the one that wrote it.  `startSeconds` is the start time in
/// whole unix seconds, as the kernel keeps it; a second either way is
/// allowed, a reused pid differs by more.
pub(super) fn daemon_pid(home: &Path) -> Option<i32> {
    let text = std::fs::read_to_string(home.join(PID_FILE)).ok()?;
    let v = marspot_peer::json::parse(&text).ok()?;
    let pid = i32::try_from(v.get("pid")?.i64()?).ok()?;
    let row = marspot::pidtree::proc_row(pid)?;
    match v.at(&["processIdentity", "startSeconds"]).and_then(|s| s.i64()) {
        Some(start) if (start - row.start_unix as i64).abs() > 1 => None,
        _ => Some(pid),
    }
}

/// Whether some process holds a lock on `file`.  `F_GETLK` asks
/// without taking one, so a codex starting at this moment is never
/// refused because of the look.  That it also reports `flock` locks,
/// which are what codex takes, is a macOS fact (one lock covers both
/// there); on Linux it would not.  A look that fails counts as held.
pub(super) fn held(file: &Path) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(f) = std::fs::File::open(file) else { return false };
    if !f.metadata().is_ok_and(|m| m.is_file()) {
        return false;
    }
    let mut fl: libc::flock = unsafe { std::mem::zeroed() };
    fl.l_type = libc::F_WRLCK as libc::c_short;
    fl.l_whence = libc::SEEK_SET as libc::c_short;
    // SAFETY: a valid fd and a flock the call fills in
    let r = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETLK, &mut fl) };
    r != 0 || fl.l_type != libc::F_UNLCK as libc::c_short
}

/// A process other than this one that runs under `home`: its
/// `CODEX_HOME` names it, or it is a `codex` without one and `home` is
/// `~/.codex`.  The environment is the one the process was started
/// with, and only processes of this user can be read; codex is one.
pub(super) fn running_under(home: &Path) -> Option<String> {
    let me = std::process::id() as i32;
    let default = std::env::var_os("HOME").map(|h| Path::new(&h).join(".codex"));
    let is_default = default.is_some_and(|d| super::same(&d, home));
    let found = marspot::pidtree::list_all_procs().into_iter().find(|p| {
        p.pid != me
            && match marspot::pidtree::proc_env_value(p.pid, "CODEX_HOME") {
                Some(v) => super::same(Path::new(&v), home),
                None => is_default && p.comm == "codex",
            }
    });
    marspot::pidtree::release_room();
    found.map(|p| format!("{} (pid {}) runs under it", p.comm, p.pid))
}


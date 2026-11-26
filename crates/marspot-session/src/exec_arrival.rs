//! What this image was handed by the exec that started it: a core's
//! spawn or the image before it, each against what that exec carries.

use std::fs::File;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::fs::MetadataExt;

use marspot_term::exec_contract::{Arrival, ExecKind};

use crate::handoff::L3Handoff;

/// A cold start's check, once logging is up.  Never on a resume: an
/// older image that a core started and that did not know the mark
/// keeps the core's `session-spawn` mark through its own update, and
/// the image it execs would take that mark for its own.
pub(crate) fn check_spawn(arrival: Option<&Arrival>) {
    if let Some(a) = arrival.filter(|a| a.kind() == ExecKind::SessionSpawn) {
        a.verify(&[]);
    }
}

/// A resumed image's check: what the handoff names, and the session
/// directory's lock.
pub(crate) fn check_resume(arrival: Option<&Arrival>, h: &L3Handoff, id: u64) {
    if let Some(a) = arrival.filter(|a| a.kind() == ExecKind::SessionUpdate) {
        let lock = dir_lock_fd(id).unwrap_or(-1);
        a.verify(&[h.master_fd, h.listen_fd, h.control_stream_fd, lock]);
    }
}

/// The descriptor of the session directory's lock that the image
/// before this one held across the exec.  The handoff does not name
/// it, and no image before this one kept its number: it is the open
/// descriptor on that file.
fn dir_lock_fd(id: u64) -> Option<RawFd> {
    let path = marspot_term::session_registry::session_dir(id).join(".lock");
    let want = std::fs::metadata(&path).ok()?;
    marspot_term::exec_contract::open_without_cloexec().into_iter().find(|&fd| {
        // SAFETY: an fstat into a zeroed struct of ours
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let ok = unsafe { libc::fstat(fd, &mut st) } == 0;
        ok && st.st_dev as u64 == want.dev() && st.st_ino == want.ino()
    })
}

/// Take the lock's descriptor over, so that the next update carries it
/// by name.
pub(crate) fn inherited_dir_lock(id: u64) -> Option<File> {
    // SAFETY: open in this process, and owned by nothing else in this image
    dir_lock_fd(id).map(|fd| unsafe { File::from_raw_fd(fd) })
}

/// Debug builds only: hand the next image something the contract does
/// not carry, for the tests.  Called before the sweep (`swept` false)
/// and after it.  `fd`, `var`: a descriptor without close-on-exec, an
/// unlisted variable, after the sweep, which the new image catches;
/// `fd-before`, `var-before`: the same before it, which the sweep
/// removes; `old-mark`: the mark an image that does not know the
/// contract leaves when a core started it.
#[cfg(debug_assertions)]
pub(crate) fn leak_for_test(swept: bool) {
    use std::os::fd::IntoRawFd;
    let what = std::env::var("MARSPOT_TEST_EXEC_LEAK").unwrap_or_default();
    match (what.as_str(), swept) {
        ("fd", true) | ("fd-before", false) => {
            let Ok(f) = File::open("/dev/null") else { return };
            let fd = f.into_raw_fd();
            // SAFETY: fcntl on the descriptor just opened
            unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
        }
        // SAFETY (all three): the loop has ended; the threads left do not read the environment
        ("var", true) | ("var-before", false) => unsafe { std::env::set_var("MARSPOT_LEAKED_BY_TEST", "1") },
        ("old-mark", true) => {
            // SAFETY: getppid cannot fail
            let core = unsafe { libc::getppid() };
            unsafe { std::env::set_var(marspot_term::exec_contract::KIND_ENV, format!("session-spawn:{core}")) }
        }
        _ => {}
    }
}

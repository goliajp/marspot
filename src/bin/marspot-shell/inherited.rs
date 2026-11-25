//! Children an earlier image of this process left behind.  `exec` keeps
//! the pid, so whatever the old image started and had not waited for
//! is this image's child, and no `Child` here owns it: without this it
//! stays a zombie for as long as the app runs.
//!
//! Waited for by pid only, never with `waitpid(-1)`: that would also
//! take the children whose owners wait for them (the core connection's
//! shutdown, an inbox's relay start, the madcawl host), and an owner
//! that gets ECHILD fails.  One inherited child has another owner: the
//! previous image's madcawl host, which the host's start ends and waits
//! for.  It takes the pid out of here with [`disown`] first, under the
//! lock this module waits under, so no inherited pid is waited for
//! twice, and a pid this module already waited for (which may name
//! another process by then) is not signalled.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;

struct Inherited {
    /// Still running at the last look.
    live: Vec<i32>,
    /// Waited for here, until [`disown`] asks about one.
    reaped: Vec<i32>,
}

/// Both lists together never hold more pids than `adopt` found.
static STATE: Mutex<Inherited> = Mutex::new(Inherited { live: Vec::new(), reaped: Vec::new() });
/// `live` is not empty: what a pass reads before taking the lock.
static ANY: AtomicBool = AtomicBool::new(false);
/// What `adopt` found, for `report` once the log is open.
static FOUND: AtomicU32 = AtomicU32::new(0);
static REAPED: AtomicU32 = AtomicU32::new(0);

/// Whether to look for inherited children: every image but the
/// short-lived per-render subcommands, which start none and must stay
/// cheap.  An image cannot tell reliably from its environment whether an
/// exec made it (the identity exec carries no mark), so it looks always.
pub(crate) fn after_exec() -> bool {
    !matches!(
        std::env::args().nth(1).as_deref(),
        Some("--cc-statusline" | "--submit-receipt" | "--codex-locks")
    )
}

/// Take every child this process already has; run before it starts any
/// of its own, so all of them are inherited.  Before the log is open.
pub(crate) fn adopt() {
    let me = std::process::id() as i32;
    let children = marspot::pidtree::child_pids(me);
    let (reaped, live): (Vec<i32>, Vec<i32>) = children.iter().partition(|&&pid| reap(pid));
    FOUND.store(children.len() as u32, Ordering::Relaxed);
    REAPED.store(reaped.len() as u32, Ordering::Relaxed);
    ANY.store(!live.is_empty(), Ordering::Release);
    *STATE.lock().unwrap() = Inherited { live, reaped };
}

/// Log what [`adopt`] found, once the log is open.
pub(crate) fn report() {
    let found = FOUND.load(Ordering::Relaxed);
    if found == 0 {
        return;
    }
    marspot::lx_event!(
        "SHELL_INHERITED",
        "children of an earlier image of this process",
        found = found,
        reaped = REAPED.load(Ordering::Relaxed),
        live = found - REAPED.load(Ordering::Relaxed)
    );
}

/// Wait for those of the inherited children that have ended since the
/// last look; on every supervisor pass.  How many are still running.
pub(crate) fn reap_pending() -> usize {
    if !ANY.load(Ordering::Acquire) {
        return 0;
    }
    let mut s = STATE.lock().unwrap();
    let Inherited { live, reaped } = &mut *s;
    live.retain(|&pid| {
        let gone = reap(pid);
        if gone {
            marspot::lx_info!("shell.inherited.reaped", "an inherited child ended", pid = pid);
            reaped.push(pid);
        }
        !gone
    });
    ANY.store(!live.is_empty(), Ordering::Release);
    live.len()
}

/// Leave `pid` to a caller that waits for it itself.  True when this
/// module has waited for it already: the caller must then neither wait
/// for nor signal that pid.
pub(crate) fn disown(pid: i32) -> bool {
    let mut s = STATE.lock().unwrap();
    if let Some(i) = s.reaped.iter().position(|&p| p == pid) {
        s.reaped.swap_remove(i);
        return true;
    }
    s.live.retain(|&p| p != pid);
    ANY.store(!s.live.is_empty(), Ordering::Release);
    false
}

/// Waited for, or not a child of this process any more.
fn reap(pid: i32) -> bool {
    let mut status = 0;
    match unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } {
        0 => false,
        r if r == pid => true,
        _ => std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD),
    }
}

#[cfg(test)]
#[path = "inherited_tests.rs"]
mod tests;

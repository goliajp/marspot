//! Closing a session: its L3 is told to end and removes the session's
//! directory itself, since only it knows when it writes no more.  The
//! mark in the directory is what tells it that a hangup is a close.
//!
//! What an L3 cannot do itself is done here, off the caller's thread,
//! once its process has exited: an L3 from before it removed its
//! directory, and one that died of the signal (while booting, or between
//! two images).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use marspot_term::grid_shm::SessionRegions;
use marspot_term::session_registry::{
    SessionEntry, closed_session_ids, delete_dead_session, delete_session, entry_is_live, mark_closed,
    read_session_entry, retire_dead_session,
};
use marspot_term::{lx_event, lx_warn};

use crate::pidtree::{self, ProcId};
use crate::session_signal::signal_entry;

/// How long an L3 that was told to close gets to exit.  It takes a
/// fraction of a second; past this it is stuck, and its directory is
/// taken away from it, which it takes as being evicted.
const EXIT_WITHIN: Duration = Duration::from_secs(10);

/// Close session `sid`.  Never blocks on its L3.
pub fn close_session(sid: u64) {
    let entry = read_session_entry(sid).ok();
    let live = entry.as_ref().filter(|e| entry_is_live(e));
    if live.is_some()
        && let Err(e) = mark_closed(sid)
    {
        // it keeps its history, which goes once it has exited
        lx_warn!("session.close.mark_failed", &format!("{e}"), session = sid);
    }
    // SIGHUP is a close and nothing else: SIGTERM also asks an L3 to
    // update, and with another build staged it would re-exec instead of
    // ending
    match live.and_then(|e| signal_entry(e, libc::SIGHUP).ok()) {
        Some(id) => clean_up_after(sid, id),
        None => {
            if let Err(e) = delete_session(sid) {
                lx_warn!("session.close.delete_failed", &format!("{e}"), session = sid);
            }
        }
    }
    delete_regions(entry.as_ref());
}

/// The names the session published under, so the kernel frees the pages
/// once every holder has closed them.
fn delete_regions(entry: Option<&SessionEntry>) {
    if let Some(regions) = entry.and_then(SessionRegions::of_entry) {
        regions.delete();
    }
}

/// Close a slot that has no L3 as far as the caller knows: what is left
/// of its session goes to the recycle dir.  A live L3 that holds it
/// after all is closed.
pub fn close_dead_session(sid: u64) {
    let entry = read_session_entry(sid).ok();
    match retire_dead_session(sid) {
        Ok(true) => delete_regions(entry.as_ref()),
        Ok(false) if entry.as_ref().is_some_and(entry_is_live) => close_session(sid),
        Ok(false) => {}
        Err(e) => lx_warn!("session.close.retire_failed", &format!("{e}"), session = sid),
    }
}

/// Finish the closes an earlier core did not see to their end.
pub fn finish_closed() {
    for sid in closed_session_ids() {
        let entry = read_session_entry(sid).ok();
        if entry.as_ref().is_some_and(entry_is_live) {
            close_session(sid);
            continue;
        }
        match delete_dead_session(sid) {
            Ok(true) => delete_regions(entry.as_ref()),
            Ok(false) => {}
            Err(e) => lx_warn!("session.close.delete_failed", &format!("{e}"), session = sid),
        }
    }
}

fn clean_up_after(sid: u64, id: ProcId) {
    let spawned = std::thread::Builder::new()
        .name(format!("l3-close-{sid}"))
        .spawn(move || clean_up(sid, id, EXIT_WITHIN));
    if let Err(e) = spawned {
        lx_warn!("session.close.no_thread", &format!("{e}"), session = sid);
    }
}

fn clean_up(sid: u64, id: ProcId, within: Duration) {
    // SAFETY: plain libc call; `exits_within` closes the queue.
    let kq = unsafe { libc::kqueue() };
    clean_up_on(kq, sid, id, within);
}

fn clean_up_on(kq: libc::c_int, sid: u64, id: ProcId, within: Duration) {
    let exited = match exits_within(kq, id, within) {
        Ok(exited) => exited,
        Err(e) => {
            // it may be writing still; a later core finishes the close
            lx_warn!("session.close.exit_unknown", &format!("{e}"), session = sid, pid = id.pid);
            return;
        }
    };
    // an entry another L3 wrote is not this one's to take
    if !read_session_entry(sid).is_ok_and(|e| e.pid == id.pid) {
        return;
    }
    match delete_session(sid) {
        Ok(()) => lx_event!(
            "SESSION_DIR_CLEANED",
            "a closed session's L3 did not remove its directory",
            session = sid,
            pid = id.pid,
            exited = exited as u32
        ),
        Err(e) => lx_warn!("session.close.delete_failed", &format!("{e}"), session = sid),
    }
}

/// Wait on the queue `kq`, which is closed, for `id` to exit; false when
/// it has not `within`.
fn exits_within(kq: libc::c_int, id: ProcId, within: Duration) -> io::Result<bool> {
    if kq < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the queue is the caller's to give away.
    let kq = unsafe { OwnedFd::from_raw_fd(kq) };
    let deadline = Instant::now() + within;
    // SAFETY: plain kevent calls on an open queue; an all-zero kevent is
    // a valid one to fill in.
    unsafe {
        let mut ev: libc::kevent = std::mem::zeroed();
        ev.ident = id.pid as usize;
        ev.filter = libc::EVFILT_PROC;
        ev.flags = libc::EV_ADD;
        ev.fflags = libc::NOTE_EXIT;
        if libc::kevent(kq.as_raw_fd(), &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) != 0 {
            let e = io::Error::last_os_error();
            return if e.raw_os_error() == Some(libc::ESRCH) { Ok(true) } else { Err(e) };
        }
        // a pid taken since by another process is not the one to wait for
        if pidtree::proc_id(id.pid) != Some(id) {
            return Ok(true);
        }
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let ts = libc::timespec { tv_sec: left.as_secs() as libc::time_t, tv_nsec: left.subsec_nanos().into() };
            let mut got: libc::kevent = std::mem::zeroed();
            match libc::kevent(kq.as_raw_fd(), std::ptr::null(), 0, &mut got, 1, &ts) {
                0 => return Ok(false),
                1 => return Ok(true),
                _ => {
                    let e = io::Error::last_os_error();
                    if e.kind() != io::ErrorKind::Interrupted {
                        return Err(e);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "session_close_tests.rs"]
mod tests;

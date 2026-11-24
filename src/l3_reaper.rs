//! The L3s this process started and holds no handle on any more: the
//! pane was closed, its window put away, or its spawn landed with no
//! pane to take it.  Each is reaped when it exits; nothing else would,
//! and it would stay a zombie for as long as this process runs, which
//! `kill(pid, 0)` takes for a live session.
//!
//! One thread waits on a kqueue for all of them.  It reaps only a pid in
//! the list: an exit reported for a pid that was reaped already, and
//! taken since by another child, must not reap that child.  The list
//! holds L3s that still run, so the session cap bounds it.

use std::sync::{Mutex, MutexGuard, OnceLock};

use marspot_term::lx_warn;

static WATCHED: Mutex<Vec<i32>> = Mutex::new(Vec::new());
static QUEUE: OnceLock<Option<libc::c_int>> = OnceLock::new();

/// Reap `pid`, a child of this process that no handle waits for, once
/// it exits.
pub fn reap_when_exited(pid: u32) {
    let pid = pid as i32;
    let mut watched = watched();
    if reaped(pid) {
        return;
    }
    let Some(kq) = queue() else { return };
    // SAFETY: plain kevent call on an open queue; an all-zero kevent is
    // a valid one to fill in.
    let added = unsafe {
        let mut ev: libc::kevent = std::mem::zeroed();
        ev.ident = pid as usize;
        ev.filter = libc::EVFILT_PROC;
        ev.flags = libc::EV_ADD | libc::EV_ONESHOT;
        ev.fflags = libc::NOTE_EXIT;
        libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null())
    };
    if added != 0 {
        let e = std::io::Error::last_os_error();
        // it may have exited since the first look
        if !reaped(pid) {
            lx_warn!("l3.reap.unwatched", &format!("{e}"), pid = pid);
        }
        return;
    }
    watched.push(pid);
    // an exit between the first look and the watch may go unreported
    if reaped(pid) {
        watched.retain(|p| *p != pid);
    }
}

fn watched() -> MutexGuard<'static, Vec<i32>> {
    WATCHED.lock().unwrap_or_else(|e| e.into_inner())
}

fn reaped(pid: i32) -> bool {
    // SAFETY: plain libc call; `pid` is a child of this process
    unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) == pid }
}

fn queue() -> Option<libc::c_int> {
    *QUEUE.get_or_init(|| {
        // SAFETY: plain libc call
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            lx_warn!("l3.reap.no_queue", &format!("{}", std::io::Error::last_os_error()));
            return None;
        }
        match std::thread::Builder::new().name("l3-reaper".into()).spawn(move || run(kq)) {
            Ok(_) => Some(kq),
            Err(e) => {
                lx_warn!("l3.reap.no_thread", &format!("{e}"));
                None
            }
        }
    })
}

fn run(kq: libc::c_int) {
    loop {
        // SAFETY: an all-zero kevent is a valid one to fill in, and the
        // queue stays open for the life of the process
        let mut evs: [libc::kevent; 16] = unsafe { std::mem::zeroed() };
        let n = unsafe { libc::kevent(kq, std::ptr::null(), 0, evs.as_mut_ptr(), 16, std::ptr::null()) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            lx_warn!("l3.reap.queue_failed", &format!("{e}"));
            return;
        }
        let mut watched = watched();
        for ev in &evs[..n as usize] {
            let pid = ev.ident as i32;
            if let Some(i) = watched.iter().position(|p| *p == pid)
                && reaped(pid)
            {
                watched.swap_remove(i);
            }
        }
    }
}

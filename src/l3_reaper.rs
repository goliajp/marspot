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
//!
//! An exiting process is not yet one to reap: XNU reports its exit, and
//! refuses a watch on it with ESRCH, a moment before `waitpid` with
//! WNOHANG finds it.  So a pid known to be exiting is waited for.

use std::sync::{Mutex, MutexGuard, OnceLock};

use marspot_term::lx_warn;

static WATCHED: Mutex<Vec<i32>> = Mutex::new(Vec::new());
static QUEUE: OnceLock<Option<libc::c_int>> = OnceLock::new();

/// Reap `pid`, a child of this process that no handle waits for, once
/// it exits.
pub fn reap_when_exited(pid: u32) {
    let pid = pid as i32;
    let mut watched = watched();
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
        if e.raw_os_error() == Some(libc::ESRCH) {
            // exited, or exiting
            reap_exiting(pid);
        } else {
            lx_warn!("l3.reap.unwatched", &format!("{e}"), pid = pid);
        }
        return;
    }
    watched.push(pid);
}

fn watched() -> MutexGuard<'static, Vec<i32>> {
    WATCHED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Wait for `pid`, a child known to be exiting, to be reaped.  Only
/// this process reaps it, and its pid is not reused before, so the wait
/// is brief and for that process alone.
fn reap_exiting(pid: i32) {
    loop {
        // SAFETY: plain libc call; `pid` is a child of this process
        if unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) } >= 0 {
            return;
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            lx_warn!("l3.reap.wait_failed", &format!("{e}"), pid = pid);
            return;
        }
    }
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
        for ev in &evs[..n as usize] {
            let pid = ev.ident as i32;
            let mut watched = watched();
            let Some(i) = watched.iter().position(|p| *p == pid) else { continue };
            watched.swap_remove(i);
            drop(watched);
            reap_exiting(pid);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn gone(pid: i32) -> bool {
        // SAFETY: plain libc call; a zombie still takes a signal
        unsafe { libc::kill(pid, 0) != 0 }
    }

    /// Children handed over while they exit, and others that exit
    /// later, are all reaped: none is left a zombie.
    #[test]
    fn every_child_handed_over_is_reaped() {
        use std::io::Read;
        let mut pids = Vec::new();
        for i in 0..300 {
            let mut child = match i % 3 {
                0 => Command::new("/bin/sleep").arg("0.2").spawn().unwrap(),
                // its output ends as it exits: handed over in the middle of that
                _ => Command::new("/bin/echo").stdout(std::process::Stdio::piped()).spawn().unwrap(),
            };
            if let Some(mut out) = child.stdout.take() {
                out.read_to_end(&mut Vec::new()).unwrap();
            }
            pids.push(child.id() as i32);
            super::reap_when_exited(child.id());
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        while let Some(&pid) = pids.iter().find(|p| !gone(**p)) {
            assert!(Instant::now() < deadline, "pid {pid} was not reaped");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

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
//! WNOHANG finds it, and that moment lasts as long as the exit does (it
//! waits for a terminal's output to drain, for one).  So a pid known to
//! be exiting is waited for, by the reaper thread: the caller is the
//! core's main loop.

use std::sync::{Mutex, MutexGuard, OnceLock};

use marspot_term::lx_warn;

struct Lists {
    /// watched for their exit
    watched: Vec<i32>,
    /// refused a watch as exiting, for the reaper thread to wait for
    exiting: Vec<i32>,
}

static LISTS: Mutex<Lists> = Mutex::new(Lists { watched: Vec::new(), exiting: Vec::new() });
static QUEUE: OnceLock<Option<libc::c_int>> = OnceLock::new();

/// The user event that tells the reaper thread of new exiting pids.
const WAKE: usize = 0;

/// Reap `pid`, a child of this process that no handle waits for, once
/// it exits.  Never waits for it.
pub fn reap_when_exited(pid: u32) {
    let pid = pid as i32;
    // held across the watch: an exit reported before the pid is listed
    // would be dropped
    let mut lists = lists();
    let Some(kq) = queue() else { return };
    let added = change(kq, pid as usize, libc::EVFILT_PROC, libc::EV_ADD | libc::EV_ONESHOT, libc::NOTE_EXIT);
    match added {
        Ok(()) => lists.watched.push(pid),
        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {
            // exited, or exiting
            lists.exiting.push(pid);
            drop(lists);
            if let Err(e) = change(kq, WAKE, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER) {
                lx_warn!("l3.reap.no_wake", &format!("{e}"), pid = pid);
            }
        }
        Err(e) => lx_warn!("l3.reap.unwatched", &format!("{e}"), pid = pid),
    }
}

fn lists() -> MutexGuard<'static, Lists> {
    LISTS.lock().unwrap_or_else(|e| e.into_inner())
}

fn change(kq: libc::c_int, ident: usize, filter: i16, flags: u16, fflags: u32) -> std::io::Result<()> {
    // SAFETY: plain kevent call on an open queue; an all-zero kevent is
    // a valid one to fill in
    let r = unsafe {
        let mut ev: libc::kevent = std::mem::zeroed();
        ev.ident = ident;
        ev.filter = filter;
        ev.flags = flags;
        ev.fflags = fflags;
        libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null())
    };
    if r == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

/// Wait for `pid`, a child known to be exiting, to be reaped.  Only
/// this process reaps it, and its pid is not reused before, so the wait
/// is for that process alone.
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
        if let Err(e) = change(kq, WAKE, libc::EVFILT_USER, libc::EV_ADD | libc::EV_CLEAR, 0) {
            lx_warn!("l3.reap.no_wake", &format!("{e}"));
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
            if ev.filter == libc::EVFILT_USER {
                let exiting = std::mem::take(&mut lists().exiting);
                exiting.into_iter().for_each(reap_exiting);
                continue;
            }
            let pid = ev.ident as i32;
            let mut lists = lists();
            let Some(i) = lists.watched.iter().position(|p| *p == pid) else { continue };
            lists.watched.swap_remove(i);
            drop(lists);
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

    fn await_gone(pid: i32) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !gone(pid) {
            assert!(Instant::now() < deadline, "pid {pid} was not reaped");
            std::thread::sleep(Duration::from_millis(20));
        }
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
        pids.into_iter().for_each(await_gone);
    }

    /// A child whose exit waits for its terminal's output to drain is
    /// handed over while it exits: the caller goes on at once, and the
    /// child is reaped when its exit ends.
    #[test]
    fn a_child_slow_to_exit_does_not_hold_the_caller() {
        use std::io::Read;
        use std::os::fd::{FromRawFd, OwnedFd};
        let (mut master, mut slave) = (0, 0);
        // SAFETY: plain libc calls on the two fds openpty fills in
        unsafe {
            use std::ptr::null_mut;
            assert_eq!(libc::openpty(&mut master, &mut slave, null_mut(), null_mut(), null_mut()), 0);
            libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(slave, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        let (mut exiting, exiting_w) = std::io::pipe().unwrap();
        // fill the terminal nobody reads, then exit without closing it first
        let fill = "use Fcntl; my $f = fcntl(STDOUT, F_GETFL, 0); fcntl(STDOUT, F_SETFL, $f | O_NONBLOCK); \
                    1 while defined(syswrite(STDOUT, 'x' x 4096)); fcntl(STDOUT, F_SETFL, $f); POSIX::_exit(0)";
        // the reaper under test waits for it
        #[allow(clippy::zombie_processes)]
        let child = Command::new("/usr/bin/perl")
            .args(["-MPOSIX", "-e", fill])
            .stdin(std::process::Stdio::null())
            .stdout(slave)
            .stderr(exiting_w)
            .spawn()
            .unwrap();
        // its stderr is closed by the exit, before the terminal is drained
        exiting.read_to_end(&mut Vec::new()).unwrap();
        let pid = child.id() as i32;
        // SAFETY: plain libc call; WNOWAIT leaves the child to be reaped
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
        assert_eq!(unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, flags) }, 0);
        super::queue();
        if info.si_pid != 0 {
            // a loaded runner let the exit finish first: nothing to measure
            eprintln!("the child finished its exit before it was handed over; skipped");
            super::reap_when_exited(child.id());
            await_gone(pid);
            return;
        }
        let t = Instant::now();
        super::reap_when_exited(child.id());
        let held = t.elapsed();
        assert!(held < Duration::from_millis(200), "the caller waited {held:?} for the child");
        await_gone(pid);
        drop(master);
    }
}

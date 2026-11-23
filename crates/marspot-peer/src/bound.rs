//! Waiting for marspot to bind the calling session to its pane.
//!
//! An agent can call a tool within a second or two of its start, before
//! the app has seen its process and, for Codex, its thread.  Such a
//! call waits for the record instead of failing: on the registry
//! directories changing, up to a limit, or until it is cancelled.

use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::control::client::Cancel;

/// The longest a call waits: well inside the shortest time a client
/// gives a tool call (60 s).
pub const LIMIT: Duration = Duration::from_secs(20);

/// One server's waiting, shared by its calls.
#[derive(Clone)]
pub struct Waiter {
    limit: Duration,
    /// A wait ran out: this session is one nothing binds, and its later
    /// calls are answered at once.
    gave_up: Arc<AtomicBool>,
}

impl Waiter {
    pub fn new(limit: Duration) -> Self {
        Self { limit, gave_up: Arc::default() }
    }

    /// Wait until `found`, asked again whenever something in `dirs`
    /// changes.  What it last said.
    pub fn until(&self, dirs: &[PathBuf], cancel: &Cancel, found: impl Fn() -> bool) -> bool {
        if self.gave_up.load(Ordering::Relaxed) || dirs.is_empty() || found() {
            return found();
        }
        let Some((watch, cancelled)) = cancel.watch().and_then(|c| Some((Watch::new(dirs, c.as_raw_fd())?, c))) else {
            return found();
        };
        let until = Instant::now() + self.limit;
        loop {
            // after the watch is up, so that no change falls between
            if found() {
                return true;
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                self.gave_up.store(true, Ordering::Relaxed);
                return false;
            }
            if watch.next(left) == Some(cancelled.as_raw_fd()) {
                return false;
            }
        }
    }
}

/// A kqueue on some directories and one socket.
struct Watch {
    kq: OwnedFd,
    _dirs: Vec<File>,
}

impl Watch {
    fn new(dirs: &[PathBuf], socket: RawFd) -> Option<Watch> {
        // SAFETY: kqueue takes no arguments; a descriptor it returns is ours
        let kq = unsafe {
            let fd = libc::kqueue();
            (fd >= 0).then(|| OwnedFd::from_raw_fd(fd))?
        };
        let w = Watch { kq, _dirs: dirs.iter().filter_map(|d| File::open(d).ok()).collect() };
        w.add(socket, libc::EVFILT_READ, 0)?;
        for d in &w._dirs {
            w.add(d.as_raw_fd(), libc::EVFILT_VNODE, libc::NOTE_WRITE)?;
        }
        Some(w)
    }

    fn add(&self, fd: RawFd, filter: i16, fflags: u32) -> Option<()> {
        // SAFETY: an all-zero kevent is valid; one change is read and no
        // event is written
        let rc = unsafe {
            let mut ev: libc::kevent = std::mem::zeroed();
            ev.ident = fd as usize;
            ev.filter = filter;
            ev.flags = libc::EV_ADD | libc::EV_CLEAR;
            ev.fflags = fflags;
            libc::kevent(self.kq.as_raw_fd(), &ev, 1, std::ptr::null_mut(), 0, std::ptr::null())
        };
        (rc == 0).then_some(())
    }

    /// The descriptor of the next event, None when `within` ran out.
    fn next(&self, within: Duration) -> Option<RawFd> {
        let ts = libc::timespec { tv_sec: within.as_secs() as libc::time_t, tv_nsec: within.subsec_nanos().into() };
        // SAFETY: room for the one event asked for
        unsafe {
            let mut ev: libc::kevent = std::mem::zeroed();
            let n = libc::kevent(self.kq.as_raw_fd(), std::ptr::null(), 0, &mut ev, 1, &ts);
            (n == 1).then_some(ev.ident as RawFd)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("peer-bound-{}", crate::rand::hex(6)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Written and renamed into place, as a record is.
    #[test]
    fn a_record_that_appears_ends_the_wait() {
        let d = dir();
        let (tmp, rec) = (d.join(".r.tmp"), d.join("1.json"));
        let t = std::thread::spawn({
            let (tmp, rec) = (tmp.clone(), rec.clone());
            move || {
                std::thread::sleep(Duration::from_millis(300));
                std::fs::write(&tmp, "x").unwrap();
                std::fs::rename(&tmp, &rec).unwrap();
            }
        });
        let began = Instant::now();
        let w = Waiter::new(LIMIT);
        assert!(w.until(std::slice::from_ref(&d), &Cancel::default(), || rec.exists()));
        assert!(began.elapsed() < Duration::from_secs(10), "{:?}", began.elapsed());
        t.join().unwrap();
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_wait_that_ran_out_is_the_last() {
        let d = dir();
        let w = Waiter::new(Duration::from_millis(200));
        let dirs = std::slice::from_ref(&d);
        let began = Instant::now();
        assert!(!w.until(dirs, &Cancel::default(), || false));
        assert!(began.elapsed() >= Duration::from_millis(200));
        let long = Waiter { limit: LIMIT, ..w.clone() };
        let began = Instant::now();
        assert!(!long.until(dirs, &Cancel::default(), || false));
        assert!(began.elapsed() < Duration::from_secs(5));
        assert!(long.until(dirs, &Cancel::default(), || true), "found all the same");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_cancelled_call_stops_waiting() {
        let d = dir();
        let cancel = Arc::new(Cancel::default());
        let t = std::thread::spawn({
            let cancel = cancel.clone();
            move || {
                std::thread::sleep(Duration::from_millis(300));
                cancel.cancel();
            }
        });
        let began = Instant::now();
        let w = Waiter::new(LIMIT);
        assert!(!w.until(std::slice::from_ref(&d), &cancel, || false));
        assert!(began.elapsed() < Duration::from_secs(10), "{:?}", began.elapsed());
        assert!(!w.gave_up.load(Ordering::Relaxed), "that says nothing about the session");
        t.join().unwrap();
        std::fs::remove_dir_all(&d).unwrap();
    }
}

//! What the pty reader sleeps on.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

pub(super) enum Event {
    /// The PTY holds this many bytes, or has ended: at least one, so
    /// the read that follows reports the end.
    Data(usize),
    Stop,
    Neither,
}

/// The two things the reader waits for, in one kqueue.  Unlike `poll`
/// it says how much the PTY holds, so nothing is read to find it dry.
pub(super) struct Wakes {
    kq: OwnedFd,
    stop: RawFd,
}

impl Wakes {
    pub(super) fn new(master: RawFd, stop: RawFd) -> Self {
        let watch = |fd: RawFd| libc::kevent {
            ident: fd as usize,
            filter: libc::EVFILT_READ,
            flags: libc::EV_ADD,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        let changes = [watch(master), watch(stop)];
        // SAFETY: a new kqueue, owned from here on; two live changes
        // in and no events out.
        let kq = unsafe {
            let kq = libc::kqueue();
            assert!(kq >= 0, "a kqueue for the pty reader");
            libc::kevent(kq, changes.as_ptr(), 2, std::ptr::null_mut(), 0, std::ptr::null());
            OwnedFd::from_raw_fd(kq)
        };
        Self { kq, stop }
    }

    /// Until the PTY has something, `stop` has, or `timeout` has passed
    /// (never, when `None`).  `stop` wins.
    pub(super) fn wait(&self, timeout: Option<Duration>) -> Event {
        let timeout = timeout.map(|t| libc::timespec {
            tv_sec: t.as_secs() as libc::time_t,
            tv_nsec: t.subsec_nanos() as libc::c_long,
        });
        // SAFETY: an all-zero kevent is a valid value to be overwritten;
        // room for both events, and the timeout lives through the call.
        let (n, events) = unsafe {
            let mut events: [libc::kevent; 2] = std::mem::zeroed();
            let n = libc::kevent(
                self.kq.as_raw_fd(),
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                2,
                timeout.as_ref().map_or(std::ptr::null(), |t| t as *const libc::timespec),
            );
            (n, events)
        };
        if n <= 0 {
            // a timeout, or a signal: the caller waits again
            return Event::Neither;
        }
        let events = &events[..n as usize];
        if events.iter().any(|e| e.ident == self.stop as usize) {
            return Event::Stop;
        }
        Event::Data((events[0].data.max(0) as usize).max(1))
    }
}

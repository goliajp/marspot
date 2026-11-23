//! Noticing a caller that hung up while its connection's thread sleeps
//! on a wait.
//!
//! One kqueue for every waiting connection, and one thread blocked in
//! it: a socket is watched only while its connection waits, and the
//! thread wakes only when one of them has news.  The end of a socket
//! posts [`Reply::HungUp`] into that connection's channel, which is what
//! its thread is asleep on.
//!
//! The write side is watched, not the read side.  A caller that only
//! shut down its writing half (`shutdown(SHUT_WR)`) ends what this side
//! reads, but still reads the answer; only a caller that closed the
//! socket also ends what this side can write.

use std::collections::HashMap;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use super::Reply;

struct Watcher {
    kq: OwnedFd,
    /// Socket -> the token of its current watch and who to tell.  An
    /// event is delivered only when its token is the one in the table:
    /// one taken off the queue just before its socket's watch ended
    /// must not reach a later connection that got the same fd.
    socks: Mutex<HashMap<RawFd, (usize, Sender<Reply>)>>,
    next: AtomicUsize,
}

static WATCHER: OnceLock<Option<Watcher>> = OnceLock::new();

fn watcher() -> Option<&'static Watcher> {
    WATCHER
        .get_or_init(|| {
            // SAFETY: kqueue takes no arguments and returns a new fd or -1
            let fd = unsafe { libc::kqueue() };
            if fd < 0 {
                marspot::lx_warn!("shell.control.kqueue_failed", &format!("{}", std::io::Error::last_os_error()));
                return None;
            }
            // SAFETY: fd is a kqueue this function just opened and nothing else owns
            let kq = unsafe { OwnedFd::from_raw_fd(fd) };
            Some(Watcher { kq, socks: Mutex::new(HashMap::new()), next: AtomicUsize::new(1) })
        })
        .as_ref()
        .inspect(|_| start_thread())
}

fn start_thread() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let spawned = std::thread::Builder::new().name("control-hangup".into()).spawn(|| {
            if let Some(w) = WATCHER.get().and_then(Option::as_ref) {
                w.run();
            }
        });
        if let Err(e) = spawned {
            marspot::lx_warn!("shell.control.hangup_thread_failed", &format!("{e}"));
        }
    });
}

fn change(kq: RawFd, fd: RawFd, flags: u16, token: usize) -> bool {
    let ev = libc::kevent {
        ident: fd as usize,
        filter: libc::EVFILT_WRITE,
        flags,
        fflags: 0,
        data: 0,
        udata: token as *mut libc::c_void,
    };
    // SAFETY: one change in, no events out, no timeout
    unsafe { libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) == 0 }
}

impl Watcher {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<RawFd, (usize, Sender<Reply>)>> {
        self.socks.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Tell the connection watching `fd` under `token` that it hung up.
    fn deliver(&self, fd: RawFd, token: usize) -> bool {
        let socks = self.lock();
        socks.get(&fd).is_some_and(|(t, tx)| *t == token && tx.send(Reply::HungUp).is_ok())
    }

    fn run(&self) {
        // SAFETY: kevent is plain data; all zeroes is a valid value
        let mut evs: [libc::kevent; 16] = unsafe { std::mem::zeroed() };
        loop {
            // SAFETY: room for 16 events, blocking until one comes
            let n = unsafe {
                libc::kevent(self.kq.as_raw_fd(), std::ptr::null(), 0, evs.as_mut_ptr(), evs.len() as i32, std::ptr::null())
            };
            if n < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                marspot::lx_warn!("shell.control.hangup_wait_failed", &format!("{}", std::io::Error::last_os_error()));
                return;
            }
            for ev in &evs[..n as usize] {
                // a writable socket is reported once when armed; edge-triggered,
                // so it does not repeat while nothing is written
                if ev.flags & libc::EV_EOF == 0 {
                    continue;
                }
                self.deliver(ev.ident as RawFd, ev.udata as usize);
            }
        }
    }
}

/// Watch `fd` for its far end closing until the guard drops.  A socket
/// already closed at the far end is reported at once.
pub(super) fn watch(fd: RawFd, tx: Sender<Reply>) -> Option<Armed> {
    let w = watcher()?;
    let token = w.next.fetch_add(1, Ordering::Relaxed);
    w.lock().insert(fd, (token, tx));
    if !change(w.kq.as_raw_fd(), fd, libc::EV_ADD | libc::EV_CLEAR, token) {
        w.lock().remove(&fd);
        return None;
    }
    Some(Armed { fd, token })
}

/// A socket being watched; dropped before the socket is closed.
pub(super) struct Armed {
    fd: RawFd,
    token: usize,
}

impl Drop for Armed {
    fn drop(&mut self) {
        if let Some(w) = WATCHER.get().and_then(Option::as_ref) {
            change(w.kq.as_raw_fd(), self.fd, libc::EV_DELETE, self.token);
            let mut socks = w.lock();
            if socks.get(&self.fd).is_some_and(|(t, _)| *t == self.token) {
                socks.remove(&self.fd);
            }
        }
    }
}

#[cfg(test)]
pub(super) fn deliver_for_test(fd: RawFd, token: usize) -> bool {
    WATCHER.get().and_then(Option::as_ref).is_some_and(|w| w.deliver(fd, token))
}

#[cfg(test)]
impl Armed {
    pub(super) fn token(&self) -> usize {
        self.token
    }
}

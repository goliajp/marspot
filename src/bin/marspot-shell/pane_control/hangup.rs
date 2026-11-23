//! Noticing a caller that hung up while its connection's thread sleeps
//! on a wait.
//!
//! One kqueue for every waiting connection, and one thread blocked in
//! it: a socket is watched only while its connection waits, and the
//! thread wakes only when one of them has news.  The end of a socket
//! posts [`Reply::HungUp`] into that connection's channel, which is what
//! its thread is asleep on.

use std::collections::HashMap;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::io::AsRawFd;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use super::Reply;

struct Watcher {
    kq: OwnedFd,
    socks: Mutex<HashMap<RawFd, Sender<Reply>>>,
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
            let w = Watcher { kq: unsafe { OwnedFd::from_raw_fd(fd) }, socks: Mutex::new(HashMap::new()) };
            Some(w)
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

fn change(kq: RawFd, fd: RawFd, flags: u16) -> bool {
    let ev = libc::kevent {
        ident: fd as usize,
        filter: libc::EVFILT_READ,
        flags,
        fflags: 0,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    // SAFETY: one change in, no events out, no timeout
    unsafe { libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) == 0 }
}

impl Watcher {
    fn run(&self) {
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
                // a request sent ahead is not an end; edge-triggered, so it does not repeat
                if ev.flags & libc::EV_EOF == 0 {
                    continue;
                }
                let socks = self.socks.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(tx) = socks.get(&(ev.ident as RawFd)) {
                    let _ = tx.send(Reply::HungUp);
                }
            }
        }
    }
}

/// Watch `fd` for its far end closing until the guard drops.  A socket
/// already closed at the far end is reported at once.
pub(super) fn watch(fd: RawFd, tx: Sender<Reply>) -> Option<Armed> {
    let w = watcher()?;
    w.socks.lock().unwrap_or_else(|p| p.into_inner()).insert(fd, tx);
    if !change(w.kq.as_raw_fd(), fd, libc::EV_ADD | libc::EV_CLEAR) {
        w.socks.lock().unwrap_or_else(|p| p.into_inner()).remove(&fd);
        return None;
    }
    Some(Armed { fd })
}

/// A socket being watched; dropped before the socket is closed, so a
/// reused fd is never mistaken for it.
pub(super) struct Armed {
    fd: RawFd,
}

impl Drop for Armed {
    fn drop(&mut self) {
        if let Some(w) = WATCHER.get().and_then(Option::as_ref) {
            change(w.kq.as_raw_fd(), self.fd, libc::EV_DELETE);
            w.socks.lock().unwrap_or_else(|p| p.into_inner()).remove(&self.fd);
        }
    }
}

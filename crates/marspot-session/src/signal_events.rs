//! The signals an L3 takes, turned into events for its main loop.
//!
//! - SIGTERM: an update from a sender that predates `SessionUpdate`
//!   when `current/` holds another build, otherwise "end" (logout,
//!   `kill`).
//! - SIGHUP: the app retired this session.  Exit, whatever is staged.
//!   An L3 has no controlling terminal, so nothing else sends it.
//!
//! The handlers only write a byte to a self-pipe, the one thing a
//! handler can safely do; a thread turns each byte into an event.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::Sender;

use crate::SessionEvent;

static PIPE_W: AtomicI32 = AtomicI32::new(-1);

const TERM: u8 = 1;
const HUP: u8 = 2;

extern "C" fn on_signal(sig: libc::c_int) {
    let fd = PIPE_W.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = [if sig == libc::SIGHUP { HUP } else { TERM }];
        unsafe { libc::write(fd, byte.as_ptr().cast(), 1) };
    }
}

pub(crate) fn install(ev_tx: Sender<SessionEvent>) -> std::io::Result<()> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let (r_fd, w_fd) = (fds[0], fds[1]);
    for fd in [r_fd, w_fd] {
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
    PIPE_W.store(w_fd, Ordering::Relaxed);

    std::thread::Builder::new()
        .name("l3-signal-watcher".to_string())
        .spawn(move || {
            let mut buf = [0u8; 1];
            loop {
                let n = unsafe { libc::read(r_fd, buf.as_mut_ptr().cast(), 1) };
                if n <= 0 {
                    break;
                }
                let ev = if buf[0] == HUP {
                    SessionEvent::CloseRequested
                } else {
                    SessionEvent::ShutdownRequested
                };
                if ev_tx.send(ev).is_err() {
                    break;
                }
            }
        })?;

    for sig in [libc::SIGTERM, libc::SIGHUP] {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as *const () as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            sa.sa_flags = libc::SA_RESTART;
            if libc::sigaction(sig, &sa, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

/// Consume a SIGTERM that is pending while SIGTERM is blocked; true if
/// there was one.
///
/// After an execv: SIGTERM stays blocked across the handover, so one
/// sent meanwhile is still pending in the new image.  It was sent for
/// the update that just happened -- two senders asking for the same
/// update -- and delivered now, with this build in `current/`, it would
/// end the pane it was meant to move.
pub(crate) fn drop_pending_sigterm() -> bool {
    unsafe {
        let mut pending: libc::sigset_t = std::mem::zeroed();
        libc::sigpending(&mut pending);
        if libc::sigismember(&pending, libc::SIGTERM) != 1 {
            return false;
        }
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        let mut got = 0;
        libc::sigwait(&set, &mut got);
        true
    }
}

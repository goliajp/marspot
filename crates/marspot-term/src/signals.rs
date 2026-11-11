//! Signal-mask primitives for the execv handoff.
//!
//! An L3 replaces its own image with `execv` to take an update without
//! restarting the PTY.  Between that call and the new image arming its
//! own SIGTERM handler there are a few milliseconds where the process
//! has no handler at all — and the default action for SIGTERM is to
//! terminate.
//!
//! That window is not theoretical.  On 2026-09-07 one install lost
//! five panes to it: L2 fans SIGTERM out to every reattached L3 when
//! it comes up, install-local fans out again ~350 ms later, and
//! neither knows about the other.  The five that died were exactly the
//! five that execv'd LAST in the first wave, so their unarmed window
//! overlapped the second fan-out.
//!
//! A signal mask survives `execv`, and a signal raised while blocked
//! stays PENDING instead of being lost.  Blocking SIGTERM across the
//! handoff therefore turns "killed mid-swap" into "still pending when
//! the new image is ready", and the new image drops it: it asked for
//! the update that just happened.  Updates now go out once, by the
//! core, as a `SessionUpdate` frame that is harmless to repeat; SIGTERM
//! is only what senders from before that frame use.

/// Block or unblock `SIGTERM` for this process.
pub fn set_sigterm_blocked(block: bool) {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        let how = if block { libc::SIG_BLOCK } else { libc::SIG_UNBLOCK };
        libc::sigprocmask(how, &set, std::ptr::null_mut());
    }
}

/// True iff `SIGTERM` is currently blocked for this process.
pub fn sigterm_blocked() -> bool {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigprocmask(libc::SIG_BLOCK, std::ptr::null(), &mut set);
        libc::sigismember(&set, libc::SIGTERM) == 1
    }
}

static WATCH_W: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

extern "C" fn on_watched(sig: libc::c_int, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    let fd = WATCH_W.load(std::sync::atomic::Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    // SAFETY: the kernel hands an SA_SIGINFO handler a valid siginfo;
    // write(2) is async-signal-safe and 8 bytes to a pipe are atomic.
    unsafe {
        let from: i32 = (*info).si_pid();
        let mut msg = [0u8; 8];
        msg[..4].copy_from_slice(&sig.to_ne_bytes());
        msg[4..].copy_from_slice(&from.to_ne_bytes());
        libc::write(fd, msg.as_ptr().cast(), msg.len());
    }
}

/// Call `on_signal(signal, sender pid)` on a thread of its own for each
/// of `sigs` this process receives.  Once per process.
///
/// The sender is the kernel's `si_pid`: the pid that called `kill(2)`.
/// It may already be gone when the thread looks at it.
pub fn watch(
    sigs: &[libc::c_int],
    mut on_signal: impl FnMut(libc::c_int, i32) + Send + 'static,
) -> std::io::Result<()> {
    let mut fds = [0i32; 2];
    // SAFETY: plain libc calls on descriptors this function just made.
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        for fd in fds {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
    let (r_fd, w_fd) = (fds[0], fds[1]);
    WATCH_W.store(w_fd, std::sync::atomic::Ordering::Relaxed);
    std::thread::Builder::new().name("signal-watcher".to_string()).spawn(move || {
        let mut msg = [0u8; 8];
        // SAFETY: reads into a buffer of the length passed.
        while unsafe { libc::read(r_fd, msg.as_mut_ptr().cast(), msg.len()) } == msg.len() as isize {
            let sig = i32::from_ne_bytes([msg[0], msg[1], msg[2], msg[3]]);
            let from = i32::from_ne_bytes([msg[4], msg[5], msg[6], msg[7]]);
            on_signal(sig, from);
        }
    })?;
    for &sig in sigs {
        // SAFETY: installs a handler that only calls write(2).
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_watched as *const () as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            sa.sa_flags = libc::SA_RESTART | libc::SA_SIGINFO;
            if libc::sigaction(sig, &sa, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

/// `TERM` for `SIGTERM` and so on; the number for anything else.
pub fn name(sig: libc::c_int) -> String {
    match sig {
        libc::SIGTERM => "TERM".to_string(),
        libc::SIGHUP => "HUP".to_string(),
        libc::SIGINT => "INT".to_string(),
        n => n.to_string(),
    }
}

/// The executable name of a signal's sender, `?` once it is gone.
pub fn sender_name(pid: i32) -> String {
    crate::platform::pid_exe_name(pid).unwrap_or_else(|| "?".to_string())
}

/// Record under `tag` who sends this process SIGTERM, SIGHUP or SIGINT,
/// then die of the signal exactly as a process without a handler does.
pub fn log_fatal_senders(tag: &'static str) {
    let watching = watch(&[libc::SIGTERM, libc::SIGHUP, libc::SIGINT], move |sig, from| {
        crate::lx_event!(
            tag,
            "ending on a signal",
            sig = name(sig),
            from_pid = from,
            from = sender_name(from)
        );
        // SAFETY: restores the default action and re-sends the signal
        // to this process, so the exit status is the signal's.
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::kill(libc::getpid(), sig);
        }
    });
    if let Err(e) = watching {
        crate::lx_warn!("signals.watch_failed", &format!("{e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test for the whole watcher: its pipe is one per process.
    #[test]
    fn a_watched_signal_carries_its_sender() {
        let (tx, rx) = std::sync::mpsc::channel();
        watch(&[libc::SIGUSR1], move |sig, from| tx.send((sig, from)).unwrap()).unwrap();
        let me = std::process::id().to_string();
        let mut child =
            std::process::Command::new("/bin/kill").args(["-USR1", &me]).spawn().unwrap();
        let (sig, from) = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert_eq!(sig, libc::SIGUSR1);
        assert_eq!(from, child.id() as i32, "the sender is the process that called kill");
        child.wait().unwrap();
        assert_eq!(name(libc::SIGHUP), "HUP");
        assert_eq!(sender_name(std::process::id() as i32).is_empty(), false);
    }

    #[test]
    fn blocking_is_observable_and_reversible() {
        assert!(!sigterm_blocked());
        set_sigterm_blocked(true);
        assert!(sigterm_blocked());
        set_sigterm_blocked(false);
        assert!(!sigterm_blocked());
    }
}

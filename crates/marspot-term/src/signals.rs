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
//! handoff therefore turns "killed mid-swap" into "delivered as soon
//! as the new image is ready" — without needing the fan-outs to be
//! deduplicated, which is the fix that would have to be re-done every
//! time someone adds a third sender.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocking_is_observable_and_reversible() {
        assert!(!sigterm_blocked());
        set_sigterm_blocked(true);
        assert!(sigterm_blocked());
        set_sigterm_blocked(false);
        assert!(!sigterm_blocked());
    }
}

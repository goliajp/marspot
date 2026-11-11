//! The signals an L3 takes, turned into events for its main loop.
//!
//! - SIGHUP: the app retired this session.  Exit, whatever is staged.
//!   An L3 has no controlling terminal, so nothing else sends it.
//! - SIGTERM: an update from a sender that predates `SessionUpdate`
//!   when `current/` holds another build, otherwise nothing.  It is not
//!   a close: every process started from a pane shares the app's
//!   coalition, and macOS sends SIGTERM to the members of a coalition
//!   when the user force-quits any one of them from the Dock.  On
//!   2026-11-11 that ended six panes whose app was never asked to close
//!   them.  Only launchd's SIGTERM ends the session: the machine is
//!   shutting down, and the exit saves the screen first.
//!
//! Every signal is logged with its sender, so an unexpected one names
//! its source.

use std::sync::mpsc::Sender;

use marspot_term::signals;

use crate::SessionEvent;

const LAUNCHD: i32 = 1;

/// What `sig` from `from_pid` asks of the session, and the word the
/// log uses for it.
fn event_for(sig: libc::c_int, from_pid: i32) -> (SessionEvent, &'static str) {
    if sig == libc::SIGHUP {
        (SessionEvent::CloseRequested, "close")
    } else if from_pid == LAUNCHD {
        (SessionEvent::ShutdownRequested, "end")
    } else {
        (SessionEvent::UpdateRequested, "update")
    }
}

pub(crate) fn install(ev_tx: Sender<SessionEvent>) -> std::io::Result<()> {
    signals::watch(&[libc::SIGTERM, libc::SIGHUP], move |sig, from_pid| {
        let (ev, taken_as) = event_for(sig, from_pid);
        marspot_term::lx_event!(
            "L3_SIGNAL",
            "signal received",
            sig = signals::name(sig),
            from_pid = from_pid,
            from = signals::sender_name(from_pid),
            taken_as = taken_as
        );
        let _ = ev_tx.send(ev);
    })
}

/// Lead a session of our own, so a signal sent to the process group of
/// the app that started this L3 does not reach it.  Fails, harmlessly,
/// after an execv and under a parent that already made us a group
/// leader.
pub(crate) fn leave_the_spawner_group() {
    // SAFETY: setsid takes no arguments and only changes this process.
    unsafe { libc::setsid() };
}

/// Consume a SIGTERM that is pending while SIGTERM is blocked; true if
/// there was one.
///
/// After an execv: SIGTERM stays blocked across the handover, so one
/// sent meanwhile is still pending in the new image.  It was sent for
/// the update that just happened -- two senders asking for the same
/// update.
pub(crate) fn drop_pending_sigterm() -> bool {
    // SAFETY: signal-set calls on local sets; sigwait returns at once
    // because the signal is known to be pending.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_hangup_or_launchd_ends_the_session() {
        let stranger = 4242;
        assert!(matches!(event_for(libc::SIGHUP, stranger).0, SessionEvent::CloseRequested));
        assert!(matches!(event_for(libc::SIGTERM, stranger).0, SessionEvent::UpdateRequested));
        assert!(matches!(event_for(libc::SIGTERM, 0).0, SessionEvent::UpdateRequested));
        assert!(matches!(event_for(libc::SIGTERM, LAUNCHD).0, SessionEvent::ShutdownRequested));
    }
}

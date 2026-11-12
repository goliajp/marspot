//! The supervisor's clock, and when it may rest.
//!
//! The supervisor polls: once a second it asks the kernel who owns each
//! pane's terminal, looks at each pane's output log, ticks the plugins,
//! and every five seconds pings the core.  While something is going on
//! that is how it finds out.  While every pane is a shell sitting at its
//! prompt it found out nothing, sixty times a minute, and that was most
//! of what an idle app cost.
//!
//! So the clock rests when there is nothing a poll could find:
//!
//! - every pane's state machine has settled on [`PaneStatus::Empty`] —
//!   its shell owns the terminal, holds no jobs, no program is bound to
//!   it and its output has been quiet for `PTY_QUIET_AFTER`;
//! - the supervisor itself has nothing in flight (no update being
//!   probed, no restart pending, no pane being woken, no autorun, no op
//!   or pane session running, no dev seam armed);
//! - nothing has knocked for [`REST_AFTER`].
//!
//! A pane in that state cannot change without one of the things that
//! end the rest: a frame from the core (output was drawn), anything the
//! shell sends the core (a key, the mouse, focus, a resize), a knock
//! from a plugin thread or a background job ([`knock`]), the command
//! socket or a second launch, a signal, the settings file changing, the
//! system's appearance changing, the core's socket closing.  Each of those runs a pass at
//! once and puts the clock back to once a second, so nothing that
//! happens while resting is seen later than it would have been.
//!
//! At rest the clock does not tick at all.  What a tick would have
//! looked for has an event instead: the core exiting closes its socket,
//! which knocks; a core that is alive but hung is found when there is
//! something to deliver to it, since sending it anything ends the rest
//! and the next pass pings with the usual deadline; the interface
//! language is read on the pass that the app regaining focus asks for.
//! With any program bound to any pane the clock never rests, and
//! everything about agents runs exactly as before.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use marspot::app::EventProxy;
use marspot::fs_watch::Watcher;
use marspot::pane_state::PaneStatus;

use super::{ShellInbox, SupervisorState};

/// How long after the last knock the clock may rest.
const REST_AFTER: Duration = Duration::from_secs(5);

struct Shared {
    /// Resting, and a count of the changes of it.
    resting: Mutex<(bool, u64)>,
    changed: Condvar,
    /// Set by the clock before it wakes the main thread, so a pass can
    /// tell its beat from a knock.
    ticked: AtomicBool,
    /// Set by `touch`; taken by the next pass.
    touched: AtomicBool,
    /// `resting.0`, for `touch` to read without the lock.
    at_rest: AtomicBool,
}

/// The running clock's way to the main thread, for [`knock`].
static DOOR: OnceLock<EventProxy> = OnceLock::new();

/// Ask for a pass now, from any thread: what a background job that has
/// finished, or a watch that has seen a change, does so that the pass
/// which acts on it is not left to a clock that may be at rest.  Does
/// nothing before the clock is started.
pub(crate) fn knock() {
    if let Some(p) = DOOR.get() {
        p.wake();
    }
}

pub(crate) struct Pump {
    shared: Arc<Shared>,
    proxy: EventProxy,
    last_knock: Instant,
    rested: bool,
    /// A dev seam drives the app on the clock, so with one armed the
    /// clock never rests; nor does it when what would end a rest could
    /// not be set up.
    never_rests: bool,
    _settings: Option<Watcher>,
}

impl Pump {
    /// Start the clock, and the watches that knock for what used to be
    /// polled on it: the settings file, and SIGUSR1.
    pub(crate) fn start(proxy: EventProxy) -> Pump {
        let _ = DOOR.set(proxy.clone());
        Self::start_with(proxy, Duration::from_millis(super::REDRAW_INTERVAL_MS))
    }

    fn start_with(proxy: EventProxy, beat: Duration) -> Pump {
        let shared = Arc::new(Shared {
            resting: Mutex::new((false, 0)),
            changed: Condvar::new(),
            ticked: AtomicBool::new(false),
            touched: AtomicBool::new(false),
            at_rest: AtomicBool::new(false),
        });
        let (theirs, wake) = (Arc::clone(&shared), proxy.clone());
        std::thread::Builder::new()
            .name("l1-pump".into())
            .spawn(move || {
                let mut g = theirs.resting.lock().unwrap_or_else(PoisonError::into_inner);
                loop {
                    let (resting, seen) = *g;
                    if resting {
                        g = theirs.changed.wait_while(g, |s| s.1 == seen).unwrap_or_else(PoisonError::into_inner);
                        continue;
                    }
                    let (next, timeout) = theirs
                        .changed
                        .wait_timeout_while(g, beat, |s| s.1 == seen)
                        .unwrap_or_else(PoisonError::into_inner);
                    g = next;
                    if timeout.timed_out() {
                        drop(g);
                        theirs.ticked.store(true, Ordering::Release);
                        wake.wake();
                        g = theirs.resting.lock().unwrap_or_else(PoisonError::into_inner);
                    }
                }
            })
            .expect("spawn the pump thread");
        let knock = proxy.clone();
        let settings = Watcher::spawn(&[marspot::settings::path()], move || knock.wake());
        if let Err(e) = &settings {
            marspot::lx_warn!("shell.pump.settings_unwatched", &e.to_string());
        }
        let signalled = knock_on_signal(libc::SIGUSR1, proxy.clone());
        Pump {
            shared,
            proxy,
            last_knock: Instant::now(),
            rested: false,
            // the sandbox marker is set for every sandbox run and arms nothing
            never_rests: !signalled
                || settings.is_err()
                || std::env::vars_os().any(|(k, _)| {
                    k.to_str().is_some_and(|k| k.starts_with("MARSPOT_DEV_") && k != "MARSPOT_DEV_SANDBOX")
                }),
            _settings: settings.ok(),
        }
    }

    /// Something happened that no frame from the core may ever report:
    /// the shell sent the core a key and the core is hung.  At rest
    /// that asks for a pass at once, which puts the clock back to a
    /// second and the healthcheck with it.
    pub(crate) fn touch(&self) {
        self.shared.touched.store(true, Ordering::Release);
        if self.shared.at_rest.load(Ordering::Acquire) {
            self.proxy.wake();
        }
    }

    fn rest(&mut self, resting: bool) {
        self.rested = resting;
        let mut g = self.shared.resting.lock().unwrap_or_else(PoisonError::into_inner);
        if g.0 != resting {
            self.shared.at_rest.store(resting, Ordering::Release);
            *g = (resting, g.1 + 1);
            self.shared.changed.notify_all();
            marspot::lx_debug!("shell.pump", if resting { "resting" } else { "awake" });
        }
    }
}

/// Wake the main thread when `sig` is sent to this process.  The handler
/// already installed for it still runs; this only makes the pass that
/// reads its flag happen now.
fn knock_on_signal(sig: libc::c_int, proxy: EventProxy) -> bool {
    // SAFETY: kqueue returns a new descriptor or -1.
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return false;
    }
    // SAFETY: an all-zero kevent is a valid value.
    let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
    ev.ident = sig as libc::uintptr_t;
    ev.filter = libc::EVFILT_SIGNAL;
    ev.flags = libc::EV_ADD;
    // SAFETY: `kq` is ours; one change read from `ev`, no events asked for.
    let armed = unsafe {
        libc::fcntl(kq, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) == 0
    };
    armed
        && std::thread::Builder::new()
            .name("l1-signal-knock".into())
            .spawn(move || loop {
                // SAFETY: an all-zero kevent is a valid value.
                let mut got: libc::kevent = unsafe { std::mem::zeroed() };
                // SAFETY: room for the one event asked for; no timeout.
                let n = unsafe { libc::kevent(kq, std::ptr::null(), 0, &mut got, 1, std::ptr::null()) };
                if n < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    return;
                }
                // the handler may run a moment after this is queued: the
                // knock keeps the clock at a second, which reads the flag
                proxy.wake();
            })
            .is_ok()
}

impl super::ShellApp {
    /// Whether this pass was asked for by something other than the
    /// clock and the core's answer to a ping.
    pub(super) fn knocked(&self, inbox: &[ShellInbox]) -> bool {
        let Some(p) = self.pump.as_ref() else { return true };
        let ticked = p.shared.ticked.swap(false, Ordering::AcqRel);
        let touched = p.shared.touched.swap(false, Ordering::AcqRel);
        touched || inbox.iter().any(|m| !matches!(m, ShellInbox::Pong(_))) || (!ticked && inbox.is_empty())
    }

    /// After a pass: rest the clock or put it back to a second.
    pub(super) fn settle_pump(&mut self, knocked: bool) {
        let now = Instant::now();
        let idle = !knocked && self.nothing_to_poll_for();
        let Some(p) = self.pump.as_mut() else { return };
        if knocked {
            p.last_knock = now;
        }
        let rest = idle && !p.never_rests && now.duration_since(p.last_knock) >= REST_AFTER;
        p.rest(rest);
    }

    /// Whether the clock has rested since this was last asked: a long
    /// gap between two passes is then the rest, not a stall.
    pub(super) fn pump_rested(&mut self) -> bool {
        self.pump.as_mut().is_some_and(|p| std::mem::replace(&mut p.rested, false))
    }

    fn nothing_to_poll_for(&self) -> bool {
        matches!(self.sup_state, SupervisorState::Idle)
            && self.probe_rx.is_none()
            && self.active.as_ref().is_some_and(|c| c.hello_acked)
            && !self.auto_restart_disabled
            && self.wake_queue.is_empty()
            && self.autorun_panes.is_empty()
            && self.active_pane_sessions.is_empty()
            // a present that failed is tried again on the clock
            && self.windows.iter().all(|w| !w.frame_pending)
            && {
                let panes = self.pane_status.snapshot(Instant::now());
                !panes.is_empty()
                    && panes.iter().all(|(sid, (status, _, settled))| {
                        *settled && *status == PaneStatus::Empty && !self.pty_ops.is_busy(*sid)
                    })
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether the clock ticks within `within`.
    fn ticks(p: &Pump, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if p.shared.ticked.swap(false, Ordering::AcqRel) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        false
    }

    #[test]
    fn the_clock_stops_at_rest_and_starts_again() {
        let mut p = Pump::start_with(EventProxy::new(), Duration::from_millis(20));
        assert!(ticks(&p, Duration::from_secs(2)), "no tick while awake");
        p.rest(true);
        // a beat that was already on its way when the rest began
        std::thread::sleep(Duration::from_millis(50));
        p.shared.ticked.store(false, Ordering::Release);
        assert!(!ticks(&p, Duration::from_millis(300)), "ticked at rest");
        p.rest(false);
        assert!(ticks(&p, Duration::from_secs(2)), "no tick after the rest ended");
    }

    /// At rest a touch asks for a pass; awake it only leaves its mark.
    #[test]
    fn a_touch_is_seen_by_the_next_pass() {
        let mut p = Pump::start_with(EventProxy::new(), Duration::from_secs(3600));
        p.touch();
        assert!(p.shared.touched.swap(false, Ordering::AcqRel));
        p.rest(true);
        assert!(p.shared.at_rest.load(Ordering::Acquire));
        p.touch();
        assert!(p.shared.touched.swap(false, Ordering::AcqRel));
    }
}

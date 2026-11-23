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
//! system's appearance or the user's language list changing, the core's
//! socket closing.  Each of those runs a pass at once and puts the clock
//! back to once a second, so nothing that happens while resting is seen
//! later than it would have been.
//!
//! At rest the clock does not tick at all.  What a tick would have
//! looked for has an event instead: the core exiting closes its socket,
//! which knocks; a core that is alive but hung is found when there is
//! something to deliver to it, since sending it anything ends the rest
//! and the next pass pings with the usual deadline; the interface
//! language is read on the pass the system's notice of a changed locale
//! asks for.
//!
//! Awake, the clock does not beat once a second either, unless
//! something is in flight that is only ever looked at on the beat (an
//! update, a restart, a pane being woken, an autorun, an op or a pane
//! session, a frame owed to a window, a dev seam), or what replaces the
//! beat could not be set up.  It wakes for the next moment anything is
//! due: a pane state machine's next sweep (`PaneStateTracker::due`), a
//! plugin's next tick (`PluginRegistry::next_due`), the next ping to the
//! core, and the end of `REST_AFTER`.  Everything that happens between
//! those moments asks for a pass itself, as it does at rest: a pane's
//! session reports output and its terminal changing hands, the observer
//! knocks when the panes or the programs in front of them change, a
//! plugin's report knocks.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use marspot::app::EventProxy;
use marspot::fs_watch::Watcher;
use marspot::pane_state::PaneStatus;

use super::{ShellInbox, SupervisorState};

/// How long after the last knock the clock may rest.
const REST_AFTER: Duration = Duration::from_secs(5);
/// Dev variables that leave the clock free to rest: the sandbox marker, set for every sandbox run, and seams only the core reads.
const RESTING_SEAMS: [&str; 3] = ["MARSPOT_DEV_SANDBOX", "MARSPOT_DEV_SHADOWS", super::dev_window::VAR];

/// What the clock does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plan {
    Rest,
    /// Tick at this moment, and from then on once a beat until told
    /// otherwise.
    At(Instant),
}

struct Shared {
    /// The plan, and a count of the changes of it.
    plan: Mutex<(Plan, u64)>,
    changed: Condvar,
    /// Set by the clock before it wakes the main thread, so a pass can
    /// tell its beat from a knock.
    ticked: AtomicBool,
    /// Set by `touch`; taken by the next pass.
    touched: AtomicBool,
    /// Resting, for `touch` to read without the lock.
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
    beat: Duration,
    last_knock: Instant,
    rested: bool,
    /// When the clock was last told to tick; None at rest.  A pass that
    /// comes long after the last is a stall only when it is late for
    /// this.
    planned: Option<Instant>,
    /// The next beat, while the clock beats.
    next_beat: Option<Instant>,
    /// `pane_wakes::woke` and `observe::changes` as of the last pass.
    woke_seen: (u64, u64),
    /// Whether the observer was last told to keep walking.
    observer_awake: Option<bool>,
    /// A dev seam drives the app on the clock, so with one armed the
    /// clock never rests and always beats; nor does it when what would
    /// end a rest could not be set up.
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
            plan: Mutex::new((Plan::At(Instant::now() + beat), 0)),
            changed: Condvar::new(),
            ticked: AtomicBool::new(false),
            touched: AtomicBool::new(false),
            at_rest: AtomicBool::new(false),
        });
        let (theirs, wake) = (Arc::clone(&shared), proxy.clone());
        std::thread::Builder::new()
            .name("l1-pump".into())
            .spawn(move || run(&theirs, &wake, beat))
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
            beat,
            last_knock: Instant::now(),
            rested: false,
            planned: None,
            next_beat: None,
            woke_seen: (0, 0),
            observer_awake: None,
            never_rests: !signalled
                || settings.is_err()
                || std::env::vars_os().any(|(k, _)| k.to_str().is_some_and(|k| k.starts_with("MARSPOT_DEV_") && !RESTING_SEAMS.contains(&k))),
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

    /// Tell the clock what to do next.  It is woken for any change of
    /// plan; an unchanged plan costs nothing.
    fn plan(&mut self, plan: Plan) {
        let resting = plan == Plan::Rest;
        self.rested = resting;
        self.planned = match plan {
            Plan::Rest => None,
            Plan::At(t) => Some(t),
        };
        let mut g = self.shared.plan.lock().unwrap_or_else(PoisonError::into_inner);
        if g.0 == plan {
            return;
        }
        if (g.0 == Plan::Rest) != resting {
            self.shared.at_rest.store(resting, Ordering::Release);
            marspot::lx_debug!("shell.pump", if resting { "resting" } else { "awake" });
        }
        *g = (plan, g.1 + 1);
        // a later plan wakes the clock too: having just fired, it sleeps
        // a beat past the old plan and would wake late for the new one
        self.shared.changed.notify_all();
    }

    #[cfg(test)]
    fn rest(&mut self, resting: bool) {
        self.plan(if resting { Plan::Rest } else { Plan::At(Instant::now()) });
    }

    /// The next beat at or after `now`.
    fn beat_at(&mut self, now: Instant) -> Instant {
        let next = self.next_beat.filter(|t| *t > now).unwrap_or(now + self.beat);
        self.next_beat = Some(next);
        next
    }
}

/// The clock's thread.  Once a planned tick has fired it goes on
/// ticking once a beat until the plan changes, as the clock did before
/// it had plans: a main thread that never settles still gets its passes.
fn run(shared: &Shared, wake: &EventProxy, beat: Duration) {
    let mut fired: Option<(u64, Instant)> = None;
    let mut g = shared.plan.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        let (plan, seen) = *g;
        let target = match plan {
            Plan::Rest => None,
            Plan::At(t) => Some(fired.filter(|(s, _)| *s == seen).map_or(t, |(_, next)| next)),
        };
        let Some(target) = target else {
            g = shared.changed.wait_while(g, |s| s.1 == seen).unwrap_or_else(PoisonError::into_inner);
            continue;
        };
        let now = Instant::now();
        if now < target {
            g = shared
                .changed
                .wait_timeout_while(g, target - now, |s| s.1 == seen)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            continue;
        }
        drop(g);
        shared.ticked.store(true, Ordering::Release);
        wake.wake();
        fired = Some((seen, now + beat));
        g = shared.plan.lock().unwrap_or_else(PoisonError::into_inner);
    }
}

impl Drop for Pump {
    /// The app is gone and the process may stay for its sessions: the
    /// clock's thread is not ended, it rests for good.
    fn drop(&mut self) {
        self.plan(Plan::Rest);
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
    /// clock and the core's answer to a ping.  Input sent to the core,
    /// a pane's session asking for a pass, and the observer finding the
    /// panes changed have the pane state machines look again.
    pub(super) fn knocked(&mut self, inbox: &[ShellInbox]) -> bool {
        let Some(p) = self.pump.as_mut() else { return true };
        let ticked = p.shared.ticked.swap(false, Ordering::AcqRel);
        let touched = p.shared.touched.swap(false, Ordering::AcqRel);
        let woke = (crate::control_reader::pane_wakes::woke(), crate::observe::changes());
        if touched || std::mem::replace(&mut p.woke_seen, woke) != woke {
            self.pane_status.poke();
        }
        touched || inbox.iter().any(|m| !matches!(m, ShellInbox::Pong(_))) || (!ticked && inbox.is_empty())
    }

    /// After a pass: rest the clock, or tell it when the next pass is
    /// due.  `owed` = a window still had a frame to present when the
    /// pass began: the present failed, and is tried again on the beat.
    pub(super) fn settle_pump(&mut self, knocked: bool, owed: bool) {
        let now = Instant::now();
        let panes = self.pane_status.snapshot(now);
        let settled = !owed && self.supervisor_settled() && !panes.keys().any(|sid| self.pty_ops.is_busy(*sid));
        let empty = settled
            && !panes.is_empty()
            && panes.values().all(|(status, _, quiet)| *quiet && *status == PaneStatus::Empty);
        let due = if settled { self.next_due() } else { None };
        let Some(p) = self.pump.as_mut() else { return };
        if knocked {
            p.last_knock = now;
        }
        let grace_ends = p.last_knock + REST_AFTER;
        let rest = !knocked && empty && !p.never_rests && now >= grace_ends;
        // awake, the observer keeps to its interval with no reader
        if p.observer_awake != Some(!rest) {
            p.observer_awake = Some(!rest);
            crate::observe::keep_walking(!rest);
        }
        if rest {
            return p.plan(Plan::Rest);
        }
        let at = match due {
            Some(t) if !p.never_rests => {
                p.next_beat = None;
                if empty { t.min(grace_ends) } else { t }
            }
            _ => p.beat_at(now),
        };
        p.plan(Plan::At(at));
    }

    /// Whether the clock has rested since this was last asked: a long
    /// gap between two passes is then the rest, not a stall.
    pub(super) fn pump_rested(&mut self) -> bool {
        self.pump.as_mut().is_some_and(|p| std::mem::replace(&mut p.rested, false))
    }

    /// Were *we* running?  The core's deadlines judge it by how long it
    /// has been quiet, which is only evidence if somebody was listening
    /// the whole time.  A pass later than `SUPERVISOR_STALL_GAP` past the
    /// one before, and past the moment the clock was told to tick, means
    /// the shell itself was descheduled (the machine slept, a build
    /// storm took the cores), and the core's deadlines get that time
    /// handed back instead of charged.  A rest is not a stall either, but
    /// nobody pinged through it, so it is handed back without a word.
    /// `next_ping_at` is left alone: the next pass pings at once, which
    /// is how we find out what actually happened.
    pub(super) fn witness_stall(&mut self, now: Instant) {
        let planned = self.pump.as_ref().and_then(|p| p.planned);
        let Some(last) = self.last_tick_at.replace(now) else { return };
        let gap = now.duration_since(last);
        if gap <= super::SUPERVISOR_STALL_GAP {
            return;
        }
        let rested = self.pump_rested();
        let late = now.saturating_duration_since(planned.map_or(last, |t| t.max(last)));
        if !rested && late <= super::SUPERVISOR_STALL_GAP {
            return;
        }
        if let Some(c) = self.active.as_mut() {
            c.spawned_at = super::forgive_stall(c.spawned_at, gap, now);
            c.last_pong_at = super::forgive_stall(c.last_pong_at, gap, now);
        }
        if !rested {
            marspot::lx_event!(
                "SUPERVISOR_STALL",
                "supervisor tick was late — deadlines forgiven, not charged",
                gap_ms = gap.as_millis() as u64,
                late_ms = late.as_millis() as u64
            );
        }
    }

    /// Nothing in flight that is only looked at on the beat.
    fn supervisor_settled(&self) -> bool {
        matches!(self.sup_state, SupervisorState::Idle)
            && self.probe_rx.is_none()
            && self.active.as_ref().is_some_and(|c| c.hello_acked)
            && !self.auto_restart_disabled
            && self.wake_queue.is_empty()
            && self.autorun_panes.is_empty()
            && self.active_pane_sessions.is_empty()
    }

    /// A window has a frame it has not presented yet.
    pub(super) fn frame_owed(&self) -> bool {
        self.windows.iter().any(|w| w.frame_pending)
    }

    /// The soonest of what the next pass is for: a sweep of the pane
    /// state machines, a plugin's tick, the next ping.  None when there
    /// is none of these to wait for, which has the clock beat.
    fn next_due(&self) -> Option<Instant> {
        let obs = crate::observe::shared().latest();
        let sweep = self.pane_status.due(obs.sessions.iter().map(|s| s.entry.id));
        let ping = self.active.as_ref().filter(|c| c.hello_acked).map(|c| c.next_ping_at);
        [sweep, self.plugin_registry.next_due(), ping].into_iter().flatten().min()
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
        p.rest(false);
        assert!(ticks(&p, Duration::from_secs(2)), "no tick while awake");
        p.rest(true);
        // a beat that was already on its way when the rest began
        std::thread::sleep(Duration::from_millis(50));
        p.shared.ticked.store(false, Ordering::Release);
        assert!(!ticks(&p, Duration::from_millis(300)), "ticked at rest");
        p.rest(false);
        assert!(ticks(&p, Duration::from_secs(2)), "no tick after the rest ended");
    }

    /// A planned tick comes at its moment and not before, and a plan
    /// for a later tick does not wake the clock early.
    #[test]
    fn the_clock_ticks_when_it_was_told_to() {
        let mut p = Pump::start_with(EventProxy::new(), Duration::from_secs(3600));
        p.plan(Plan::At(Instant::now() + Duration::from_millis(300)));
        assert!(!ticks(&p, Duration::from_millis(200)), "ticked before it was due");
        assert!(ticks(&p, Duration::from_secs(2)), "no tick when it was due");
        p.plan(Plan::At(Instant::now() + Duration::from_secs(3600)));
        p.plan(Plan::At(Instant::now() + Duration::from_millis(100)));
        assert!(ticks(&p, Duration::from_secs(2)), "a sooner plan wakes the clock");
        assert!(!ticks(&p, Duration::from_millis(300)), "after firing it beats, and the beat is an hour");
    }

    /// Right after firing, a plan later than the one that fired is kept
    /// to, not left to the beat.
    #[test]
    fn a_later_plan_after_a_tick_is_kept() {
        let mut p = Pump::start_with(EventProxy::new(), Duration::from_secs(1));
        p.plan(Plan::At(Instant::now() + Duration::from_millis(50)));
        assert!(ticks(&p, Duration::from_secs(2)), "no tick when it was due");
        let asked = Instant::now();
        p.plan(Plan::At(asked + Duration::from_millis(300)));
        assert!(ticks(&p, Duration::from_millis(700)), "the later plan was not kept");
        assert!(asked.elapsed() < Duration::from_millis(700));
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

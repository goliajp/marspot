//! How long the core's main loop may sleep, and when a pane's working
//! directory is read.
//!
//! The loop used to wake once a second whatever happened, and on that
//! wake asked the system for every pane's working directory.  With
//! twelve idle panes that is twelve system calls a second to learn that
//! nobody typed `cd`.
//!
//! A shell changes directory because it ran a command, and a command
//! being run shows as output: the echo of the line, then the prompt.  So
//! the directories are read after output -- shortly after it starts, so
//! the title follows a `cd` in a few hundredths of a second instead of
//! up to a full one, and once more a second after it stops, for a
//! command that changes directory and then goes quiet (`cd x; sleep 100`
//! echoes before it moves).  Under steady output that is the old once a
//! second.  They are also read when the set of panes changes, so a pane
//! reattached with a quiet shell has its title at once.  What this does
//! not see is a directory changed more than a second after the pane's
//! last output with none following: the title then waits for the next
//! output.
//!
//! With that gone the loop has nothing to do on a clock while idle, and
//! `idle_wait` says so: it names a deadline only for work that is
//! actually pending.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::{CWD_SAVE_MIN_GAP, CWD_SWEEP_INTERVAL, PROCESS_PANEL_REFRESH_INTERVAL};

/// How long after output starts the directories are read: long enough
/// for a shell to have acted on the line it just echoed.
const SETTLE: Duration = Duration::from_millis(50);

/// When the directories are next read, in milliseconds of uptime; zero
/// for not at all.  `SOON` follows the start of output, `LAST` its end.
static SOON_MS: AtomicU64 = AtomicU64::new(0);
static LAST_MS: AtomicU64 = AtomicU64::new(0);
/// The panes the directories were last read for, folded to one number.
static PANES: AtomicU64 = AtomicU64::new(0);

fn start() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

/// Never zero, which means "not scheduled".
fn ms(at: Instant) -> u64 {
    at.saturating_duration_since(start()).as_millis() as u64 + 1
}

fn at(ms: u64) -> Instant {
    start() + Duration::from_millis(ms)
}

/// The earlier of two optional waits.
fn sooner(a: Option<Duration>, b: Option<Duration>) -> Option<Duration> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

impl super::CoreApp {
    /// A pane produced output: its shell may have moved.
    pub(super) fn note_output(&self) {
        let now = Instant::now();
        LAST_MS.store(ms(now + CWD_SWEEP_INTERVAL), Ordering::Relaxed);
        if SOON_MS.load(Ordering::Relaxed) == 0 {
            // no more often than the sweep ever ran
            let soon = (now + SETTLE).max(self.last_cwd_sweep + CWD_SWEEP_INTERVAL);
            SOON_MS.store(ms(soon), Ordering::Relaxed);
        }
    }

    /// Read every pane's working directory if a read is due: after
    /// output (see the module), or to write down a move whose save was
    /// held back by `CWD_SAVE_MIN_GAP`.
    pub(super) fn sweep_pane_cwds_when_due(&mut self) {
        let now = ms(Instant::now());
        let due = |slot: &AtomicU64| {
            let t = slot.load(Ordering::Relaxed);
            let due = t != 0 && now >= t;
            if due {
                slot.store(0, Ordering::Relaxed);
            }
            due
        };
        let (soon, last) = (due(&SOON_MS), due(&LAST_MS));
        let save = self.cwd_save_pending && self.last_cwd_save.elapsed() >= CWD_SAVE_MIN_GAP;
        let panes = self
            .windows
            .iter()
            .flat_map(|w| w.panes.iter())
            .filter_map(|p| p.shelld_session_id())
            .fold(0u64, |acc, sid| acc.wrapping_mul(0x100_0000_01b3).wrapping_add(sid + 1));
        let moved = PANES.swap(panes, Ordering::Relaxed) != panes;
        if soon || last || save || moved {
            // the schedule above is the rate limit
            self.last_cwd_sweep = Instant::now() - CWD_SWEEP_INTERVAL;
            self.sweep_pane_cwds();
        }
    }

    /// The longest the loop may wait for an event when no frame is
    /// owed.  `None`: until one comes.
    pub(super) fn idle_wait(&self) -> Option<Duration> {
        let now = Instant::now();
        let until = |t: Instant| Some(t.saturating_duration_since(now));
        let scheduled = |slot: &AtomicU64| match slot.load(Ordering::Relaxed) {
            0 => None,
            t => until(at(t)),
        };
        let mut wait = sooner(scheduled(&SOON_MS), scheduled(&LAST_MS));
        if self.cwd_save_pending {
            wait = sooner(wait, until(self.last_cwd_save + CWD_SAVE_MIN_GAP));
        }
        for w in &self.windows {
            // an open process panel is refreshed, and a kill it sent is
            // followed up, on its own beat
            if let Some(p) = w.process_panel.as_ref() {
                wait = sooner(wait, until(p.last_refresh + PROCESS_PANEL_REFRESH_INTERVAL));
                if !p.pending_kills.is_empty() {
                    wait = sooner(wait, Some(Duration::from_millis(250)));
                }
            }
            for pane in &w.panes {
                if let Some(t) = pane.search.as_ref().and_then(|s| s.bar.debounce_until) {
                    wait = sooner(wait, until(t));
                }
            }
        }
        // the dev seams act on a clock of their own; none is set in the
        // installed app
        if self.dev_close_panes > 0 {
            wait = sooner(wait, self.dev_close_panes_at.map_or(Some(Duration::ZERO), until));
        }
        if let Some(t) = self.dev_open_settings_at {
            wait = sooner(wait, until(t));
        }
        if self.dev_drag.is_some() {
            wait = sooner(wait, Some(Duration::from_millis(250)));
        }
        // files that cannot be watched are looked at as before
        if !super::theme_follow::watching() {
            wait = sooner(wait, Some(Duration::from_secs(1)));
        }
        wait
    }
}

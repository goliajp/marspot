//! Where the supervisor's wake goes, when anyone asks.
//!
//! The supervisor wakes at least once a second forever
//! (`start_redraw_pump`; four times a second when the numbers below were
//! taken), and at idle that is the only thing waking it -- so whatever
//! it costs per wake is paid on every one of those wakes for as long as
//! the app is open.  On a machine with seventeen panes the process was measured at
//! 4.4% of a core while all seventeen panes together used 1.1%, and no
//! instrument said where it went.
//!
//! Turned on by `MARSPOT_SUPERVISOR_PROFILE=1`, or by creating the file
//! `<state>/supervisor-profile` -- the file, because the question is
//! about a running app with real panes in it, and an env var would mean
//! relaunching the thing being measured.  The file is looked at once
//! every ten seconds, never on the wake path.  Off, each phase costs
//! one relaxed atomic load.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

pub const PHASES: [&str; 8] = [
    "activity_drain",
    "settings_stat",
    "sweep_pane_status",
    "sweep_autorun",
    "dev_drivers",
    "wake_queue",
    "plugin_tick",
    "rest",
];
static ON: AtomicBool = AtomicBool::new(false);
static READY: AtomicBool = AtomicBool::new(false);
static NEXT_LOOK: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
static NANOS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
static WAKES: AtomicU64 = AtomicU64::new(0);
static SINCE: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

fn marker() -> std::path::PathBuf {
    marspot_term::paths::state_root().join("supervisor-profile")
}

pub fn on() -> bool {
    if !READY.load(Ordering::Relaxed) {
        if std::env::var("MARSPOT_SUPERVISOR_PROFILE").as_deref() == Ok("1") {
            ON.store(true, Ordering::Relaxed);
        }
        READY.store(true, Ordering::Relaxed);
    }
    // The marker file, at most once every ten seconds.  Off, this
    // is a clock read and a comparison; it is never a stat on the
    // wake path.
    let mut next = NEXT_LOOK.lock().unwrap_or_else(|p| p.into_inner());
    let due = next.map(|t| Instant::now() >= t).unwrap_or(true);
    if due {
        *next = Some(Instant::now() + std::time::Duration::from_secs(10));
        let present = marker().exists();
        if present != ON.load(Ordering::Relaxed) {
            ON.store(present, Ordering::Relaxed);
            crate::lx_info!(
                "supervisor.profile.toggled",
                "supervisor phase timing",
                on = present as u32,
                marker = marker().display()
            );
        }
    }
    ON.load(Ordering::Relaxed)
}

pub fn add(phase: usize, d: std::time::Duration) {
    NANOS[phase].fetch_add(d.as_nanos() as u64, Ordering::Relaxed);
}

/// Call once per wake, after the phases.  Prints every ten seconds.
pub fn wake_done() {
    WAKES.fetch_add(1, Ordering::Relaxed);
    let mut since = SINCE.lock().unwrap_or_else(|p| p.into_inner());
    let start = since.get_or_insert_with(Instant::now);
    let elapsed = start.elapsed();
    if elapsed < std::time::Duration::from_secs(10) {
        return;
    }
    let wakes = WAKES.swap(0, Ordering::Relaxed).max(1);
    let secs = elapsed.as_secs_f64();
    let mut total = 0u64;
    let mut parts = String::new();
    for (i, name) in PHASES.iter().enumerate() {
        let n = NANOS[i].swap(0, Ordering::Relaxed);
        total += n;
        parts.push_str(&format!(" {name}={:.0}us", n as f64 / wakes as f64 / 1000.0));
    }
    *since = Some(Instant::now());
    crate::lx_info!(
        "supervisor.profile",
        "where the wake went",
        wakes_per_s = format!("{:.1}", wakes as f64 / secs),
        per_wake_us = format!("{:.0}", total as f64 / wakes as f64 / 1000.0),
        core_pct = format!("{:.2}", total as f64 / 1e9 / secs * 100.0),
        parts = parts.trim().to_string()
    );
}

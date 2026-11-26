//! The watch on the supervisor's clock: one check at most every five
//! minutes while a claude pane exists, none otherwise.  No process-table
//! walk of its own: a check reads the observer's latest table, which the
//! claudecode plugin keeps fresh while there is a claude pane.
//!
//! The tick only wakes the watch's thread; the check itself reads
//! transcripts and stats files, and a stat on a network mount can take
//! seconds, so it never runs on the supervisor's thread.  The thread
//! starts with the first check and sleeps on its channel in between.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use marspot::{lx_info, lx_warn};

use super::checker::Checker;
use super::store;
use crate::plugins::{PLUGIN_API_VERSION, Plugin, PluginHost, PluginMetadata, TickDue};

/// The shortest time between two checks.
pub(crate) const EVERY: Duration = Duration::from_secs(300);

/// `zombie.scale_for_tests` as the last check read it; 1 outside a sandbox.
static SCALE: AtomicU64 = AtomicU64::new(1);

pub(super) fn set_scale(k: u64) {
    SCALE.store(k.max(1), Ordering::Relaxed);
}

fn every() -> Duration {
    EVERY / SCALE.load(Ordering::Relaxed).max(1) as u32
}

/// A window test shrinks the watch's times; the installed app never
/// reads this.
pub(super) fn sandboxed() -> bool {
    std::env::var_os("MARSPOT_DEV_SANDBOX").is_some()
}

pub struct ZombiePlugin {
    next: Instant,
    /// Wakes the thread with the check's unix time; dropping it ends the thread.
    wake: Option<SyncSender<u64>>,
}

impl ZombiePlugin {
    pub fn new() -> ZombiePlugin {
        if sandboxed() {
            set_scale(marspot::zombie::limits::scale(&std::fs::read_to_string(marspot::settings::path()).unwrap_or_default()));
        }
        ZombiePlugin { next: Instant::now() + every(), wake: None }
    }

    fn wake(&mut self, unix: u64) {
        if self.wake.is_none() {
            match spawn(store::dir(), unix) {
                Ok((tx, _)) => self.wake = Some(tx),
                Err(e) => {
                    lx_warn!("zombie.thread.unstarted", &e.to_string());
                    return;
                }
            }
        }
        let Some(tx) = &self.wake else { return };
        match tx.try_send(unix) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => lx_info!("zombie.check.skipped", "the last check is still running"),
            Err(TrySendError::Disconnected(_)) => self.wake = None,
        }
    }
}

/// The watch's thread, checking at each time it is sent until the
/// sender is dropped.  One check waits at most: a tick that finds the
/// last one still running skips its own.
pub(super) fn spawn(dir: std::path::PathBuf, now: u64) -> std::io::Result<(SyncSender<u64>, JoinHandle<()>)> {
    let (tx, rx) = mpsc::sync_channel(1);
    let handle = std::thread::Builder::new().name("l1-zombie".into()).spawn(move || run(dir, now, rx))?;
    Ok((tx, handle))
}

fn run(dir: std::path::PathBuf, now: u64, rx: Receiver<u64>) {
    let mut checker = Checker::new(dir, now);
    while let Ok(unix) = rx.recv() {
        checker.check(unix);
    }
}

impl Plugin for ZombiePlugin {
    fn metadata(&self) -> PluginMetadata {
        // the registry ticks no plugin more often than this, whatever its tick_due says
        PluginMetadata { name: "zombie", api_version: PLUGIN_API_VERSION, tick_interval_ms: every().as_millis() as u32 }
    }

    fn start(&mut self, _host: &dyn PluginHost) -> Result<(), crate::plugins::PluginError> {
        // after the peerbus plugin's, which starts first: its relays end before the kills drop
        super::stop::force::register();
        Ok(())
    }

    fn tick_due(&self) -> TickDue {
        if crate::plugins::claudecode::sessions().is_empty() { TickDue::Event } else { TickDue::At(self.next) }
    }

    fn tick(&mut self, _host: &dyn PluginHost) {
        let now = Instant::now();
        if now < self.next || crate::plugins::claudecode::sessions().is_empty() {
            return;
        }
        self.next = now + every();
        let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        self.wake(unix);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zombie_plugin_sleeps_without_a_claude_pane_and_checks_every_five_minutes_with_one() {
        let p = ZombiePlugin::new();
        assert_eq!(p.metadata().tick_interval_ms, 300_000);
        // no claude pane has been published in a test process
        assert_eq!(p.tick_due(), TickDue::Event);
        assert!(p.wake.is_none(), "no thread before the first check");
    }

    #[test]
    fn zombie_thread_checks_when_woken_and_ends_with_its_sender() {
        let dir = std::env::temp_dir().join(format!("zombie-thread-{}", std::process::id()));
        crate::observe::shared().fresh(crate::observe::NOW);
        let (tx, handle) = spawn(dir.clone(), 1_800_000_000).unwrap();
        tx.send(1_800_000_000).unwrap();
        tx.send(1_800_000_300).unwrap();
        drop(tx);
        let started = Instant::now();
        handle.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(30));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! What a session owes the files it shares with the rest of the app:
//! follow the settings, and leave when nobody can reach it any more.
//!
//! Both used to be looked at on a timer -- the main loop woke every five
//! seconds in every pane to `stat` four files that had not changed.  A
//! watcher reports the changes instead, so an idle session sleeps until
//! something happens.  The timer survives only as what is done when the
//! files cannot be watched.
//!
//! Reachability (2026-07-28 incident).  This process is designed to
//! outlive its L2 and L1 -- that is what makes silent updates and crash
//! reattach possible -- which also means nothing upstream will ever reap
//! it: after a shell crash its parent is launchd.  The registry entry is
//! its ownership record; every reaper (boot sweep, retire, sandbox wipe)
//! works by removing the entry or the directory.  176 sessions whose
//! entries were gone accumulated that day and helped push the machine
//! into a forced reboot.  The entry is not the only way to become
//! unreachable: L2's fresh-spawn path clears a slot before spawning into
//! it, and if that spawn never lands the incumbent holds a socket nobody
//! can dial; a replacement that does land rewrites the entry with its
//! own pid.  All three are one condition, checked together.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use marspot_term::fs_watch::Watcher;
use marspot_term::{lx_event, lx_warn, session_registry};

use crate::SessionEvent;

/// How often the settings are looked at when they cannot be watched.
const SETTINGS_EVERY: Duration = Duration::from_secs(1);

pub(crate) struct Upkeep {
    sid: u64,
    pid: i32,
    entry: PathBuf,
    socket: PathBuf,
    changed: Arc<AtomicBool>,
    watcher: Option<Watcher>,
    /// How often reachability is looked at when it cannot be watched.
    /// Five seconds; a test that deletes an entry to watch the session
    /// leave sets `MARSPOT_SESSION_ENTRY_CHECK_MS` instead of sitting
    /// through that.
    entry_every: Duration,
    settings_looked: Instant,
    entry_looked: Instant,
}

impl Upkeep {
    /// Starts watching.  A change sets a flag and wakes the main loop
    /// with `SessionEvent::Wake`; `look` does the rest there.
    pub(crate) fn new(sid: u64, wake: Sender<SessionEvent>) -> Self {
        let entry = session_registry::session_entry_path(sid);
        let socket = session_registry::session_socket_path(sid);
        let changed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&changed);
        let files = [
            marspot_term::settings::path(),
            marspot_term::system_theme::path(),
            entry.clone(),
            socket.clone(),
        ];
        let watcher = Watcher::spawn(&files, move || {
            flag.store(true, Ordering::Release);
            let _ = wake.send(SessionEvent::Wake);
        });
        if let Err(e) = &watcher {
            lx_warn!(
                "l3.upkeep.unwatched",
                &format!("{e}; looking at the settings and the registry on a timer instead"),
                session_id = sid
            );
        }
        let entry_every = std::env::var("MARSPOT_SESSION_ENTRY_CHECK_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(Duration::from_secs(5));
        // What changed between this session registering and the watch
        // being armed raised no event: the first pass looks once.
        changed.store(watcher.is_ok(), Ordering::Release);
        let now = Instant::now();
        Self {
            sid,
            pid: std::process::id() as i32,
            entry,
            socket,
            changed,
            watcher: watcher.ok(),
            entry_every,
            settings_looked: now,
            entry_looked: now,
        }
    }

    fn watching(&self) -> bool {
        self.watcher.as_ref().is_some_and(Watcher::alive)
    }

    /// The longest the main loop may sleep on this account: without
    /// limit while the files are watched.
    pub(crate) fn wait(&self) -> Option<Duration> {
        if self.changed.load(Ordering::Acquire) {
            return Some(Duration::ZERO);
        }
        (!self.watching()).then(|| self.entry_every.min(Duration::from_secs(5)))
    }

    /// Call on every pass of the main loop.  Does nothing unless a file
    /// changed (or, unwatched, its beat is due).  `Some(why)` when this
    /// session can no longer be reached and has to exit.
    pub(crate) fn look(&mut self) -> Option<&'static str> {
        let changed = self.changed.swap(false, Ordering::AcqRel);
        let timed = !self.watching();
        // A setting L3 owns -- character width -- bites as soon as the
        // file says so.  New content is parsed with the new rule; cells
        // already on screen keep the width they were laid out with, and
        // heal as the program redraws them.
        if changed || (timed && self.settings_looked.elapsed() >= SETTINGS_EVERY) {
            self.settings_looked = Instant::now();
            if marspot_term::settings::reload_if_changed() {
                lx_event!(
                    "SETTINGS_RELOADED",
                    "settings.toml changed on disk",
                    circled_wide = marspot_term::settings::get().appearance_circled_wide as u64
                );
            }
            marspot_term::system_theme::changed("colour queries are answered from the theme now in force");
        }
        if !(changed || (timed && self.entry_looked.elapsed() >= self.entry_every)) {
            return None;
        }
        self.entry_looked = Instant::now();
        if !self.entry.exists() {
            Some("entry.toml removed")
        } else if !self.socket.exists() {
            Some("socket unlinked")
        } else {
            match session_registry::read_session_entry(self.sid) {
                // Someone else owns this sid now.  Not on a read error:
                // a torn read of an entry being rewritten must not look
                // like eviction.
                Ok(e) if e.pid != self.pid => Some("entry reassigned to another pid"),
                _ => None,
            }
        }
    }
}

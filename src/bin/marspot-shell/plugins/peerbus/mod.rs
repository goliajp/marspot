//! peerbus — Codex sessions as peers of Claude Code sessions.
//!
//! Claude Code sessions find each other through a registry of records
//! and message each other over per-session inbox sockets.  This plugin
//! gives every Codex pane the same: a record naming it and an inbox
//! socket in the Claude protocol, with what arrives there delivered
//! into the Codex thread through the Codex app-server.  Codex sends the
//! other way with the `marspot-peer` MCP tools.  Every session in a
//! pane, Claude's included, is named after its pane (`names`).
//!
//! Gemini and Kimi panes get the same record, inbox and MCP tools; what
//! arrives for one waits in a file until its next turn (`gemini_bind`,
//! `kimi_bind`).
//!
//! The main thread only says which panes run Codex, Gemini or Kimi;
//! everything that touches a socket, a file or a daemon happens on the
//! worker thread.

mod bind;
mod claude_hooks;
mod codex_config;
mod daemon;
mod deliver;
mod gemini_bind;
mod gemini_config;
mod idle;
mod inbox;
mod kimi_bind;
mod kimi_config;
mod lifecycle;
mod names;
mod records;
#[cfg(test)]
mod live_tests;
mod worker;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::Mutex;
use std::thread::JoinHandle;

use super::{LogLevel, Plugin, PluginError, PluginHost, PluginMetadata, PLUGIN_API_VERSION};
use worker::{Event, Want, Worker, CHANNEL};

/// A pane's codex thread as the worker knows it, for other plugins:
/// the codex plugin restarts a pane's TUI onto the same thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ThreadView {
    pub codex: marspot::pidtree::ProcId,
    pub home: PathBuf,
    pub thread: String,
    /// The daemon says the thread is idle, with no turn running and no
    /// prompt waiting.  Only known for a thread this worker subscribed
    /// to; an unsubscribed one hears no status and reads as not idle.
    pub idle: bool,
}

static THREADS: Mutex<Option<HashMap<u64, ThreadView>>> = Mutex::new(None);

/// Every bound codex pane's thread, by session id.
pub(crate) fn threads() -> HashMap<u64, ThreadView> {
    THREADS.lock().unwrap().clone().unwrap_or_default()
}

pub(super) fn publish_threads(views: HashMap<u64, ThreadView>) {
    *THREADS.lock().unwrap() = Some(views);
}

pub(crate) use kimi_bind::mail_waiting as kimi_mail_waiting;

#[derive(Default)]
pub struct PeerbusPlugin {
    tx: Option<SyncSender<Event>>,
    worker: Option<JoinHandle<()>>,
    /// The last set the worker accepted; resent only when it changes.
    sent: Vec<Want>,
    /// The same for the gemini panes; None until the first is accepted.
    sent_gemini: Option<crate::plugins::gemini::Peers>,
    sent_kimi: Option<crate::plugins::kimi::Peers>,
    /// When the worker is next asked for a names pass; at once if None.
    names_at: Option<std::time::Instant>,
    /// The noted changes (`agents::changes`) the worker has been asked
    /// to look at the configuration for.
    config_seen: u64,
}

impl PeerbusPlugin {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Hand the worker an event if it has room.  `Some(false)` when it is
/// busy with a slow daemon, and the next tick offers again; None when
/// it is gone.
fn offer(tx: &SyncSender<Event>, ev: Event) -> Option<bool> {
    match tx.try_send(ev) {
        Ok(()) => Some(true),
        Err(TrySendError::Full(_)) => Some(false),
        Err(TrySendError::Disconnected(_)) => None,
    }
}

impl PeerbusPlugin {
    /// What the worker is asked for that no change of the panes asks
    /// for: a names pass when one is due, and a look at the agents'
    /// configuration when a change to it has been noted since the last
    /// one asked for.  A noted change knocks, so this runs for it even
    /// when the supervisor's clock was at rest.  False when the worker
    /// is gone.
    fn ask(&mut self, now: std::time::Instant, changes: u64) -> bool {
        let Some(tx) = &self.tx else { return false };
        if self.names_at.is_none_or(|t| t <= now) {
            match offer(tx, Event::Names) {
                Some(true) => self.names_at = Some(now + worker::NAMES_EVERY),
                Some(false) => {}
                None => return false,
            }
        }
        if changes != self.config_seen {
            match offer(tx, Event::Reconcile) {
                Some(true) => self.config_seen = changes,
                Some(false) => {}
                None => return false,
            }
        }
        true
    }
}

impl Plugin for PeerbusPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: "peerbus",
            api_version: PLUGIN_API_VERSION,
            // the codex plugin's cadence: a pane gains or loses codex
            // no faster than a person starts or quits one
            tick_interval_ms: 2000,
        }
    }

    fn start(&mut self, host: &dyn PluginHost) -> Result<(), PluginError> {
        // Which marspot wrote a record: the sandbox and the installed
        // app share the registry and must not clear each other's.
        let root = marspot_term::paths::state_root();
        let owner = root.canonicalize().unwrap_or(root).to_string_lossy().into_owned();
        let (tx, rx) = sync_channel(CHANNEL);
        let w = Worker::new(tx.clone(), rx, owner);
        let handle = std::thread::Builder::new()
            .name("peerbus".into())
            .spawn(move || w.run())
            .map_err(|e| PluginError::Other(format!("peerbus worker: {e}")))?;
        self.tx = Some(tx);
        self.worker = Some(handle);
        host.log(LogLevel::Info, "start", "worker running");
        // the other background worker of the agent plugins; it has no
        // pane state, so it needs no plugin of its own to tick it
        super::agents::start();
        Ok(())
    }

    fn tick(&mut self, host: &dyn PluginHost) {
        let Some(tx) = &self.tx else { return };
        let obs = crate::observe::shared().latest();
        // The title is read here, not on the worker, because a title
        // that changes has to reach the worker as a changed set.  It is
        // shared memory: no file system call, nothing that can block.
        let wants: Vec<Want> = obs
            .live()
            .filter_map(|s| {
                let p = s.find("codex")?;
                let title = marspot_term::grid_shm::title::read(s.entry.id).ok().flatten();
                Some(Want {
                    sid: s.entry.id,
                    codex: p.row.id(),
                    argv: p.argv.as_deref().map(<[String]>::to_vec).unwrap_or_default(),
                    title_thread: title.as_deref().and_then(bind::thread_in_title),
                })
            })
            .collect();
        let offer = |ev: Event| offer(tx, ev);
        let mut gone = false;
        if wants != self.sent {
            match offer(Event::Want(wants.clone())) {
                Some(true) => self.sent = wants,
                Some(false) => {}
                None => gone = true,
            }
        }
        // what the gemini plugin last published: memory, like the title
        if !gone
            && let Some(gemini) = crate::plugins::gemini::peers().filter(|g| self.sent_gemini.as_ref() != Some(g))
        {
            match offer(Event::Gemini(gemini.clone())) {
                Some(true) => self.sent_gemini = Some(gemini),
                Some(false) => {}
                None => gone = true,
            }
        }
        if !gone
            && let Some(kimi) = crate::plugins::kimi::peers().filter(|k| self.sent_kimi.as_ref() != Some(k))
        {
            match offer(Event::Kimi(kimi.clone())) {
                Some(true) => self.sent_kimi = Some(kimi),
                Some(false) => {}
                None => gone = true,
            }
        }
        gone = gone || !self.ask(std::time::Instant::now(), super::agents::changes::count());
        if gone {
            host.log(LogLevel::Warn, "worker_gone", "peerbus worker exited; inboxes are down");
            self.tx = None;
        }
    }

    /// Remove every inbox and record before marspot quits, so no
    /// session is left listing a peer that cannot answer.
    fn stop(&mut self, _host: &dyn PluginHost) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Event::Stop);
        }
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A CLI install that finishes while the supervisor's clock rests
    /// notes a change and knocks; the tick that knock runs is this.
    #[test]
    fn a_noted_change_has_the_worker_look_at_the_configuration_once() {
        let (tx, rx) = sync_channel(CHANNEL);
        let mut p = PeerbusPlugin { tx: Some(tx), ..Default::default() };
        let t0 = Instant::now();
        assert!(p.ask(t0, 0));
        assert!(matches!(rx.try_recv(), Ok(Event::Names)), "the first names pass");
        assert!(rx.try_recv().is_err(), "nothing was noted: the worker looked when it started");
        assert!(p.ask(t0 + Duration::from_secs(1), 1));
        assert!(matches!(rx.try_recv(), Ok(Event::Reconcile)), "asked on the tick after the change");
        assert!(p.ask(t0 + Duration::from_secs(3), 1));
        assert!(rx.try_recv().is_err(), "and asked once");
    }

    /// A worker with no room is asked again, not told once and forgotten.
    #[test]
    fn a_look_the_worker_had_no_room_for_is_asked_for_again() {
        let (tx, rx) = sync_channel(1);
        let mut p = PeerbusPlugin { tx: Some(tx), ..Default::default() };
        let t0 = Instant::now();
        assert!(p.ask(t0, 1));
        assert!(matches!(rx.try_recv(), Ok(Event::Names)));
        assert!(rx.try_recv().is_err(), "the names pass took the only place");
        assert!(p.ask(t0 + Duration::from_secs(1), 1));
        assert!(matches!(rx.try_recv(), Ok(Event::Reconcile)));
    }

    #[test]
    fn a_worker_that_is_gone_is_reported() {
        let (tx, rx) = sync_channel(CHANNEL);
        drop(rx);
        let mut p = PeerbusPlugin { tx: Some(tx), ..Default::default() };
        assert!(!p.ask(Instant::now(), 0));
    }
}

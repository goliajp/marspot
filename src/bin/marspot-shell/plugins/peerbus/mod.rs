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
//! Gemini panes get the same record, inbox and MCP tools; what arrives
//! for one waits in a file until its next turn (`gemini_bind`).
//!
//! The main thread only says which panes run Codex or Gemini;
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

#[derive(Default)]
pub struct PeerbusPlugin {
    tx: Option<SyncSender<Event>>,
    worker: Option<JoinHandle<()>>,
    /// The last set the worker accepted; resent only when it changes.
    sent: Vec<Want>,
    /// The same for the gemini panes; None until the first is accepted.
    sent_gemini: Option<crate::plugins::gemini::Peers>,
}

impl PeerbusPlugin {
    pub fn new() -> Self {
        Self::default()
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
        // busy with a slow daemon: the next tick offers it again
        let offer = |ev: Event| match tx.try_send(ev) {
            Ok(()) => Some(true),
            Err(TrySendError::Full(_)) => Some(false),
            Err(TrySendError::Disconnected(_)) => None,
        };
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

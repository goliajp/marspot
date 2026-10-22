//! peerbus — Codex sessions as peers of Claude Code sessions.
//!
//! Claude Code sessions find each other through a registry of records
//! and message each other over per-session inbox sockets.  This plugin
//! gives every Codex pane the same: a record naming it and an inbox
//! socket in the Claude protocol, with what arrives there delivered
//! into the Codex thread through the Codex app-server.  Codex sends the
//! other way with the `marspot-peer` CLI.
//!
//! The main thread only says which panes run Codex; everything that
//! touches a socket, a file or a daemon happens on the worker thread.

mod bind;
mod deliver;
mod inbox;
mod lifecycle;
#[cfg(test)]
mod live_tests;
mod worker;

use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::thread::JoinHandle;

use super::{LogLevel, Plugin, PluginError, PluginHost, PluginMetadata, PLUGIN_API_VERSION};
use worker::{Event, Want, Worker, CHANNEL};

#[derive(Default)]
pub struct PeerbusPlugin {
    tx: Option<SyncSender<Event>>,
    worker: Option<JoinHandle<()>>,
    /// The last set the worker accepted; resent only when it changes.
    sent: Vec<Want>,
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
        Ok(())
    }

    fn tick(&mut self, host: &dyn PluginHost) {
        let Some(tx) = &self.tx else { return };
        let obs = crate::observe::shared().latest();
        let wants: Vec<Want> = obs
            .live()
            .filter_map(|s| {
                let p = s.find("codex")?;
                Some(Want { sid: s.entry.id, codex: p.row.id(), argv: p.argv.as_deref().map(<[String]>::to_vec).unwrap_or_default() })
            })
            .collect();
        if wants == self.sent {
            return;
        }
        match tx.try_send(Event::Want(wants.clone())) {
            Ok(()) => self.sent = wants,
            // busy with a slow daemon: the next tick offers it again
            Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => {
                host.log(LogLevel::Warn, "worker_gone", "peerbus worker exited; inboxes are down");
                self.tx = None;
            }
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

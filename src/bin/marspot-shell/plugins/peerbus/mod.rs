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
pub(crate) mod claude_mcp;
mod claude_hooks;
mod clock;
pub(crate) mod codex_approval;
mod codex_config;
mod codex_entry;
mod codex_inline;
mod codex_scan;
mod codex_toml;
mod codex_locks;
mod daemon;
mod deliver;
#[cfg(test)]
mod fake_daemon;
mod gemini_bind;
mod gemini_config;
mod idle;
mod inbox;
mod kimi_bind;
mod kimi_config;
mod leftovers;
mod lifecycle;
mod mcp_servers;
pub(crate) mod moving;
mod moving_checks;
mod moving_release;
#[cfg(test)]
mod moving_release_tests;
#[cfg(test)]
mod moving_tests;
#[cfg(test)]
mod mcp_table_tests;
mod names;
mod others;
mod pair;
mod records;
mod refusal;
mod reply;
#[cfg(test)]
mod live_tests;
mod title;
mod views;
mod worker;

use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::thread::JoinHandle;

use super::{LogLevel, Plugin, PluginError, PluginHost, PluginMetadata, PLUGIN_API_VERSION};
use worker::{Event, Want, Worker, CHANNEL};

pub(crate) use refusal::TurnEnds;
pub(crate) use views::{ThreadView, publish_threads, threads};
pub(crate) use codex_locks::cli_main as codex_locks_main;
pub(crate) use kimi_bind::mail_waiting as kimi_mail_waiting;
pub(in crate::plugins) use pair::{Conversation, conversation_of, conversation_of_agent, record_files};

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
    /// Keeps the CLI installer's thread running; dropped at stop.
    agents: Option<std::sync::mpsc::Sender<()>>,
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
        moving::connect(Some(tx.clone()));
        self.tx = Some(tx);
        self.worker = Some(handle);
        host.log(LogLevel::Info, "start", "worker running");
        // the other background worker of the agent plugins; it has no
        // pane state, so it needs no plugin of its own to tick it
        self.agents = super::agents::start();
        Ok(())
    }

    /// With no codex in a pane, nothing new from the gemini and kimi
    /// plugins and no noted change to look at, a tick only asks for the
    /// names pass when it is due; a codex starting in a pane changes what
    /// the observer sees, and its knock brings the tick.
    fn tick_due(&self) -> super::TickDue {
        if self.tx.is_none() {
            return super::TickDue::Event;
        }
        let settled = self.sent.is_empty()
            && self.config_seen == super::agents::changes::count()
            && crate::plugins::gemini::peers() == self.sent_gemini
            && crate::plugins::kimi::peers() == self.sent_kimi
            && !crate::observe::shared().latest().live().any(|s| crate::plugins::codex::pane_codex(s).is_some());
        match self.names_at {
            Some(t) if settled => super::TickDue::At(t),
            _ => super::TickDue::Cadence,
        }
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
                let p = crate::plugins::codex::pane_codex(s)?;
                let title = marspot_term::grid_shm::SessionRegions::of_entry(&s.entry)
                    .and_then(|r| marspot_term::grid_shm::title::read(&r).ok().flatten());
                Some(Want {
                    sid: s.entry.id,
                    codex: p.row.id(),
                    argv: p.argv.as_deref().map(<[String]>::to_vec).unwrap_or_default(),
                    title_thread: title.as_deref().and_then(title::thread_in_title),
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
        moving::connect(None);
        self.agents = None;
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

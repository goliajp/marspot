//! The worker's connections to Codex app-servers: one per `CODEX_HOME`,
//! opened on first use, with a reader thread that turns everything the
//! daemon sends into worker events.

use std::path::Path;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Instant;

use marspot_peer::json::Value;
use marspot_peer::rpc::{self, Rpc, RpcError};
use marspot_term::{lx_info, lx_warn};

use super::worker::{Conn, Event, Worker, CALL_TIMEOUT, MAX_STASH, OPT_OUT, RETRY_MAX};

impl Worker {
    pub(super) fn conn(&mut self, home: &Path) -> Result<&Conn, RpcError> {
        if !self.conns.contains_key(home) {
            let sock = home.join("app-server-control/app-server-control.sock");
            let rpc = Rpc::connect(&sock, "marspot-peerbus", env!("CARGO_PKG_VERSION"), OPT_OUT, CALL_TIMEOUT)?;
            // "codex-tui/0.163.0 (…)" → "0.163.0"
            let version = rpc.user_agent.split([' ', '/']).nth(1).unwrap_or_default().to_string();
            let (tx, mut events) = rpc.into_split();
            events.set_read_timeout(None)?;
            let (wtx, h) = (self.tx.clone(), home.to_path_buf());
            // Detached: `teardown` shuts the socket down, which ends the
            // read, and the send after it fails once the worker is gone.
            std::thread::Builder::new()
                .name("peerbus-daemon".into())
                .spawn(move || loop {
                    match events.recv() {
                        Ok(msg) => {
                            if wtx.send(Event::Rpc { home: h.clone(), msg }).is_err() {
                                return;
                            }
                        }
                        Err(_) => {
                            let _ = wtx.send(Event::RpcClosed { home: h });
                            return;
                        }
                    }
                })
                .map_err(RpcError::Io)?;
            lx_info!("peerbus.daemon.connected", "app-server connected", home = home.display().to_string(), version = version.clone());
            self.conns.insert(home.to_path_buf(), Conn { tx, version });
        }
        Ok(&self.conns[home])
    }

    pub(super) fn call(&mut self, home: &Path, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.conn(home)?.tx.request(method, params)?;
        let deadline = Instant::now() + CALL_TIMEOUT;
        loop {
            match self.rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Event::Rpc { home: h, msg }) if h == home => match rpc::answer(&msg) {
                    Some((got, result)) if got == id => return result,
                    Some(_) => {} // the answer to a call that already timed out
                    None => self.set_aside(Event::Rpc { home: h, msg }),
                },
                Ok(Event::RpcClosed { home: h }) if h == home => {
                    self.set_aside(Event::RpcClosed { home: h });
                    return Err(RpcError::Closed);
                }
                Ok(ev) => self.set_aside(ev),
                Err(RecvTimeoutError::Timeout) => {
                    return Err(RpcError::Protocol(format!("{method}: no answer in {}s", CALL_TIMEOUT.as_secs())));
                }
                Err(RecvTimeoutError::Disconnected) => return Err(RpcError::Closed),
            }
        }
    }

    pub(super) fn set_aside(&mut self, ev: Event) {
        if self.stash.len() == MAX_STASH {
            self.stash.pop_front();
            lx_warn!("peerbus.stash.full", "dropped an event set aside during a call");
        }
        self.stash.push_back(ev);
    }

    pub(super) fn schedule_retry(&mut self) {
        self.retry_at = Some(Instant::now() + self.backoff);
        self.backoff = (self.backoff * 2).min(RETRY_MAX);
    }
}

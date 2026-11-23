//! The pane control socket: `<state dir>/control.sock`, one JSON
//! request per line (`marspot_peer::control`), served on a thread per
//! connection.
//!
//! What is read only (`pane.list`, a `pane.read` without a wait,
//! `pane.audit`) is answered on the connection's thread, from what the
//! observer and the main loop have published.  What changes a pane goes
//! to the main loop as a frame to send ([`MainCmd`]), and the
//! connection sleeps on its answer: the core's reader thread hands the
//! answers here by request id (`links`), never through the main loop.
//! Waits never poll: the session judges the condition and reports it;
//! the connection thread sleeps until that report or its deadline.
//!
//! Who asks decides nothing about what may be done; it sets the limits,
//! the parent of a new pane and the audit line (`caller`).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::time::Instant;

use marspot::shell_proto::MsgType;
use marspot_peer::control::Reject;

mod audit;
mod audit_log;
pub(crate) mod caller;
mod config;
mod conn;
mod dispatch;
mod hangup;
pub(crate) mod helpers_port;
mod links;
mod matched;
mod limits;
mod mainloop;
pub(crate) mod names;
mod ops_input;
mod ops_pane;
mod ops_read;
mod panes;
mod recent;
pub(crate) mod run_trailer;
mod server;
mod shadow_gate;
mod shadow_tree;
mod wait;

pub(crate) use mainloop::MainSide;
pub(crate) use panes::PaneView;

/// A frame for the main loop to send to the core, with how to answer
/// when it cannot.
pub(crate) enum MainCmd {
    Send {
        sid: u64,
        msg: MsgType,
        payload: Vec<u8>,
        /// Refuse while a multi-step script runs on the pane.
        guard_ops: bool,
        reply: Option<(u32, Sender<Reply>)>,
    },
    /// A core announced what it does: clear every session's watches,
    /// then register the table's again.
    Resync,
    /// A connection came while the per-pane state was not being kept.
    PublishViews,
}

/// What came back for one request.
#[derive(Debug)]
pub(crate) enum Reply {
    Typed(marspot::shell_proto::PaneTyped),
    Done(marspot::shell_proto::PaneDone),
    Spawned(marspot::shell_proto::PaneSpawned),
    Fired(marspot::shell_proto::PaneWatchFired),
    /// The main loop did not send request `.0`.
    Refused(u32, Reject),
    /// The caller's end of the connection closed during a wait.
    HungUp,
}

/// Everything the connection threads, the core's reader and the main
/// loop share.
pub(crate) struct Hub {
    epoch: Instant,
    caps: AtomicU32,
    /// When the current core said hello, ms after `epoch`; 0 before.
    hello_ms: AtomicU64,
    focus: AtomicU64,
    last_key_ms: AtomicU64,
    conns: AtomicUsize,
    links: links::Links,
    limits: Mutex<limits::Limits>,
    /// One new pane or move into the shadows at a time.
    shadow_gate: shadow_gate::Gate,
    /// Panes opened a moment ago, for lists from older observations.
    recent: Mutex<recent::Recent>,
    config: RwLock<Arc<config::Config>>,
    deny: Mutex<config::DenyList>,
    panes: RwLock<Arc<panes::PaneViews>>,
    /// Nobody was connected, so the main loop stopped publishing `panes`.
    views_stale: AtomicBool,
    /// A connection asked for `panes` again and the main loop has not
    /// published yet.
    views_owed: (Mutex<bool>, Condvar),
    /// Which entries of the saved layout are windows open now.
    open_windows: RwLock<Arc<Vec<usize>>>,
    audit: audit_log::Audit,
    main_tx: Sender<MainCmd>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

static HUB: OnceLock<Hub> = OnceLock::new();

/// A key went to the core: the user is typing in the focused pane.  A
/// relaxed store of a clock reading, the one thing the key path pays.
pub(crate) fn key_pressed() {
    if let Some(h) = HUB.get() {
        h.key_pressed();
    }
}

/// Rotate a log as the audit log is: `log` becomes `log.1`, and so on.
pub(crate) fn rotate_log(path: &std::path::Path) {
    audit_log::rotate(path);
}

/// The hub, once [`start`] ran.
pub(crate) fn hub() -> Option<&'static Hub> {
    HUB.get()
}

/// Bind the socket and start serving; the main loop's half.
pub(crate) fn start(wake: Arc<dyn Fn() + Send + Sync>) -> Receiver<MainCmd> {
    let (main_tx, main_rx) = std::sync::mpsc::channel();
    let built = Hub {
        epoch: Instant::now(),
        caps: AtomicU32::new(0),
        hello_ms: AtomicU64::new(0),
        focus: AtomicU64::new(0),
        last_key_ms: AtomicU64::new(0),
        conns: AtomicUsize::new(0),
        links: links::Links::new(),
        limits: Mutex::new(limits::Limits::default()),
        shadow_gate: shadow_gate::Gate::default(),
        recent: Mutex::new(recent::Recent::default()),
        config: RwLock::new(Arc::new(config::Config::load())),
        deny: Mutex::new(config::DenyList::at(config::deny_path())),
        panes: RwLock::new(Arc::new(panes::PaneViews::default())),
        views_stale: AtomicBool::new(true),
        views_owed: (Mutex::new(false), Condvar::new()),
        open_windows: RwLock::new(Arc::new(Vec::new())),
        audit: audit_log::Audit::start(),
        main_tx,
        wake,
    };
    if HUB.set(built).is_ok()
        && let Some(h) = hub()
        && let Err(e) = server::serve(h)
    {
        marspot::lx_warn!("shell.control.bind_failed", &format!("{e}"));
    }
    main_rx
}

impl Hub {
    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Queue `cmd` for the main loop and knock.
    fn queue(&self, cmd: MainCmd) -> bool {
        let ok = self.main_tx.send(cmd).is_ok();
        if ok {
            (self.wake)();
        }
        ok
    }

    /// Knock on the main loop.
    pub(crate) fn knock(&self) {
        (self.wake)();
    }

    pub(crate) fn caps(&self) -> u32 {
        self.caps.load(Ordering::Acquire)
    }

    fn config(&self) -> Arc<config::Config> {
        Arc::clone(&self.config.read().unwrap_or_else(|p| p.into_inner()))
    }

    fn pane_views(&self) -> Arc<panes::PaneViews> {
        Arc::clone(&self.panes.read().unwrap_or_else(|p| p.into_inner()))
    }

    /// A key reached the core: the user is typing in the focused pane.
    pub(crate) fn key_pressed(&self) {
        self.last_key_ms.store(self.now_ms().max(1), Ordering::Relaxed);
    }

    pub(crate) fn focused(&self, sid: u64) {
        self.focus.store(sid, Ordering::Relaxed);
    }

    /// A new core said hello: until it says what it does, it does nothing.
    fn core_hello(&self) {
        self.caps.store(0, Ordering::Release);
        self.hello_ms.store(self.now_ms().max(1), Ordering::Release);
    }

    pub(crate) fn reload_config(&self) {
        *self.config.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(config::Config::load());
    }

    fn open_windows(&self) -> Arc<Vec<usize>> {
        Arc::clone(&self.open_windows.read().unwrap_or_else(|p| p.into_inner()))
    }

    pub(crate) fn publish_open_windows(&self, mut open: Vec<usize>) {
        open.sort_unstable();
        let mut cur = self.open_windows.write().unwrap_or_else(|p| p.into_inner());
        if **cur != open {
            *cur = Arc::new(open);
        }
    }

    pub(crate) fn publish_panes(&self, views: panes::PaneViews) {
        let mut cur = self.panes.write().unwrap_or_else(|p| p.into_inner());
        if **cur != views {
            *cur = Arc::new(views);
        }
        drop(cur);
        let (owed, cv) = &self.views_owed;
        *owed.lock().unwrap_or_else(|p| p.into_inner()) = false;
        cv.notify_all();
    }

    /// Whether the main loop should publish per-pane state: only while
    /// someone is connected to read it.
    pub(crate) fn wants_views(&self) -> bool {
        if self.conns.load(Ordering::Acquire) > 0 {
            return true;
        }
        self.views_stale.store(true, Ordering::Release);
        false
    }

    /// A connection was let in: per-pane state not kept while nobody was
    /// connected is asked for again.
    fn connected(&self) {
        if self.views_stale.swap(false, Ordering::AcqRel) {
            *self.views_owed.0.lock().unwrap_or_else(|p| p.into_inner()) = true;
            self.queue(MainCmd::PublishViews);
        }
    }

    /// Per-pane state as published, once the main loop caught up with
    /// a request for it (or `within` passed).
    fn fresh_pane_views(&self, within: std::time::Duration) -> Arc<panes::PaneViews> {
        let (owed, cv) = &self.views_owed;
        let g = owed.lock().unwrap_or_else(|p| p.into_inner());
        drop(cv.wait_timeout_while(g, within, |o| *o).unwrap_or_else(|p| p.into_inner()));
        self.pane_views()
    }

    /// Panes that closed leave every per-pane table.
    pub(crate) fn forget(&self, open: &dyn Fn(u64) -> bool) {
        self.limits.lock().unwrap_or_else(|p| p.into_inner()).retain(open);
        self.links.retain_panes(open);
    }

    /// A pane-control frame from the core; false when it is not one.
    ///
    /// Read on the core's reader thread, in the order the core sent: a
    /// core's `HelloAck` comes before its `PaneControlCaps`, so the reset
    /// cannot land after the caps it precedes.
    pub(crate) fn take_core_frame(&self, msg: MsgType, payload: &[u8]) -> bool {
        if msg == MsgType::HelloAck {
            self.core_hello();
            return false;
        }
        if msg == MsgType::PaneControlCaps {
            if let Ok(caps) = marspot::shell_proto::decode_pane_control_caps(payload) {
                self.caps.store(caps, Ordering::Release);
                marspot::lx_event!("PANE_CONTROL_CAPS_SEEN", "the core said what it does with pane control", caps = caps);
                self.queue(MainCmd::Resync);
            }
            return true;
        }
        self.links.route(msg, payload)
    }
}

#[cfg(test)]
mod tests;

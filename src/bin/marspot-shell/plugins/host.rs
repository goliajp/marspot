//! `PluginHost` impl backed by shell-side state.
//!
//! `ShellPluginHost` is the single object plugins see.  It owns `Arc`s
//! into shell-internal mirrors so plugin methods are `&self` (suits
//! plugin code holding the host across threads), without forcing the
//! shell main loop to take locks on every state access.
//!
//! In MVP, most data is sampled at tick boundaries by the host's own
//! tick-prologue pass (TODO when wired into the supervisor poll) —
//! plugins reading them get a snapshot, not live state.  This avoids
//! cross-process query latency on every plugin call.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use marspot::pane_state::{Activity, PaneStatus};
use marspot::{lx_debug, lx_error, lx_info, lx_warn};

use super::{LogLevel, PaneSession, PermissionSet, PluginError, PluginHost, PtyChild};

/// Channel message the shell main loop drains and forwards to L2 core
/// as a `PaneBadge` frame.  Empty `text` = clear.
pub struct PaneBadgeUpdate {
    pub shelld_session_id: u64,
    pub text: String,
}

/// Channel message for `PaneWheelKeys`.  A plugin declares how the
/// wheel reaches the program it understands; empty `up`/`down` clears
/// the declaration (the pane goes back to the terminal's own routing).
#[derive(Clone)]
pub struct PaneWheelKeysUpdate {
    pub shelld_session_id: u64,
    pub enter: Vec<u8>,
    pub up: Vec<u8>,
    pub down: Vec<u8>,
    /// On-screen text that means the scrollable view is open.
    pub marker: Vec<u8>,
}

/// Channel message for `PaneRenderMarkup` — see the trait method.
#[derive(Clone)]
pub struct PaneRenderMarkupUpdate {
    pub shelld_session_id: u64,
    pub on: bool,
}

/// Same channel shape but for the pane's main title (resolution chain
/// slot ABOVE cwd basename, BELOW user-set custom title).  Empty
/// `text` clears the plugin-set title for that session.
pub struct PaneTitleUpdate {
    pub shelld_session_id: u64,
    pub text: String,
}

/// Channel message: a plugin wants to take over a pane.  Main loop
/// stashes the session in `active_pane_sessions`, emits PaneSessionBegin
/// to L2, and starts routing key/escape events back here.
pub struct PaneSessionBeginRequest {
    pub shelld_session_id: u64,
    pub plugin_name: &'static str,
    pub session: Box<dyn PaneSession>,
}

/// Snapshot taken once per tick by the host so plugins read O(1)
/// instead of walking `/dev` + `/proc` (or its macOS equivalent)
/// per call.  All fields default-empty until the host populates them.
#[derive(Default, Clone)]
pub struct PaneSnapshot {
    pub pty_device: Option<PathBuf>,
    pub pid_tree: Vec<PtyChild>,
}

pub struct ShellPluginHost {
    /// Live during a session.  Resized when L2 core reports a new
    /// pane count.  Index is the L2 pane index (1-based to plugins?
    /// — no, we keep 0-based to mirror Rust convention; plugins that
    /// want 1-based label them).
    panes: Arc<Mutex<Vec<PaneSnapshot>>>,
    focused: Arc<Mutex<Option<usize>>>,
    /// `shelld_session_id → foreground status`, refreshed once per
    /// `pane_status::SWEEP_INTERVAL` by the shell's supervisor tick.
    /// Keyed by session id rather than pane index because that is what
    /// plugins already hold (badges, PaneSessions, inject-input all
    /// address sessions), and because pane indices shift when panes
    /// are added or moved between windows.
    pane_status: Arc<Mutex<HashMap<u64, (PaneStatus, Duration, bool)>>>,
    /// Current plugin being invoked — used for the log namespace and
    /// permission lookup.  Set/cleared by the registry around each
    /// hook.  See `with_active_plugin`.
    active_plugin: Arc<Mutex<Option<ActivePlugin>>>,
    /// One-way pipe to the shell main loop for L2-bound side-effects.
    /// Plugin → host → channel → main loop → CoreConn::send.  None
    /// in tests / standalone hosts where no L2 is around.
    pane_badge_tx: Mutex<Option<Sender<PaneBadgeUpdate>>>,
    pane_wheel_keys_tx: Mutex<Option<Sender<PaneWheelKeysUpdate>>>,
    pane_render_markup_tx: Mutex<Option<Sender<PaneRenderMarkupUpdate>>>,
    pane_title_tx: Mutex<Option<Sender<PaneTitleUpdate>>>,
    /// PaneSession take-over requests bound for the main loop.
    pane_session_begin_tx: Mutex<Option<Sender<PaneSessionBeginRequest>>>,
    /// L1→L2 InjectInput sender — the shell main loop's CoreConn
    /// transport.  Cc plugin calls in here from its profile-cycle
    /// state machine to push raw bytes (e.g. `claude5 --resume <uuid>\r`)
    /// at the PTY backing a given session id.  None in tests.
    inject_input_tx: Mutex<Option<Sender<InjectInputRequest>>>,
    /// Where a submitted PTY operation goes.  The queue itself lives in
    /// the main loop — see `PtyOpRequest`.
    pty_op_tx: Mutex<Option<Sender<PtyOpRequest>>>,
    /// Plugin → shell activity reports, feeding the pane state
    /// machines.  Plugins push what they know about the program they
    /// understand; composition with the kernel's view is the shell's
    /// job, not theirs.
    pane_activity_tx: Mutex<Option<Sender<(u64, Activity)>>>,
}

/// What the main loop receives on its inject_input channel.  The
/// loop walks active core's control socket and writes an InjectInput
/// frame carrying these bytes for the named session_id.
/// Reaches a pane's PTY through the main loop's inject channel.
///
/// The queue that runs operations lives in the main loop and has no
/// plugin behind it, so it needs its own way in — the same one every
/// plugin uses, rather than a private path.
pub struct HostPtyIo {
    tx: Sender<InjectInputRequest>,
}

impl HostPtyIo {
    pub fn new(tx: Sender<InjectInputRequest>) -> Self {
        Self { tx }
    }
    fn send_what(&self, session_id: u64, what: InjectWhat) -> std::io::Result<()> {
        self.tx
            .send(InjectInputRequest { session_id, what })
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "main loop dropped")
            })
    }
}

impl crate::plugins::pty_op::PtyIo for HostPtyIo {
    fn send(&self, sid: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.send_what(sid, InjectWhat::Input(bytes.to_vec()))
    }
    fn hold(&self, sid: u64, on: bool) -> std::io::Result<()> {
        self.send_what(sid, InjectWhat::HoldGrid(on))
    }
    fn paste(&self, sid: u64, text: &str) -> std::io::Result<()> {
        self.send_what(sid, InjectWhat::Paste(text.to_string()))
    }
    fn reset_mouse_reporting(&self, sid: u64) -> std::io::Result<()> {
        self.send_what(sid, InjectWhat::ResetMouseReporting)
    }
}

/// A PTY operation on its way to the one queue that serialises them.
///
/// The queue is in the main loop rather than in whoever submitted:
/// plugins are not the only submitter any more (the `--send` CLI is
/// another), and "one operation at a time per pane" is only a rule if
/// everyone goes through the same door.
pub struct PtyOpRequest {
    pub shelld_session_id: u64,
    pub op: crate::plugins::pty_op::PtyOp,
    /// Where to start — non-zero when re-arming a parked script.
    pub start_at: usize,
}

#[derive(Debug, Clone)]
pub struct InjectInputRequest {
    pub session_id: u64,
    /// `Input` carries bytes for the PTY; `HoldGrid` carries a hold
    /// flag for the pane's L3.  One channel because they are the same
    /// journey — plugin → shell main loop → live core — and ordering
    /// between them matters: releasing a hold before the input that
    /// justifies it would show the very frames the hold is hiding.
    pub what: InjectWhat,
}

#[derive(Debug, Clone)]
pub enum InjectWhat {
    Input(Vec<u8>),
    HoldGrid(bool),
    /// Text delivered the way a paste is — see
    /// `MsgType::PaneInjectPaste`.  What anything handing a *message*
    /// to a running program wants, because only L3 knows whether that
    /// program has bracketed paste on.
    Paste(String),
    /// L1 took this pane's foreground program down; L3 should stop
    /// reporting mouse tracking as on.  See
    /// `MsgType::PaneResetMouseReporting`.
    ResetMouseReporting,
}

#[derive(Clone)]
struct ActivePlugin {
    name: &'static str,
    permissions: PermissionSet,
}

impl ShellPluginHost {
    pub fn new() -> Self {
        Self {
            panes: Arc::new(Mutex::new(Vec::new())),
            focused: Arc::new(Mutex::new(None)),
            pane_status: Arc::new(Mutex::new(HashMap::new())),
            active_plugin: Arc::new(Mutex::new(None)),
            pane_badge_tx: Mutex::new(None),
            pane_wheel_keys_tx: Mutex::new(None),
            pane_render_markup_tx: Mutex::new(None),
            pane_title_tx: Mutex::new(None),
            pane_session_begin_tx: Mutex::new(None),
            inject_input_tx: Mutex::new(None),
            pty_op_tx: Mutex::new(None),
            pane_activity_tx: Mutex::new(None),
        }
    }

    /// Wire the channel the shell main loop drains for plugin-set
    /// pane title updates.
    pub fn attach_pane_title_tx(&self, tx: Sender<PaneTitleUpdate>) {
        *self.pane_title_tx.lock().unwrap() = Some(tx);
    }

    /// Wire the channel the shell main loop will drain for badge
    /// updates.  Called once during shell startup, after the main
    /// loop has created its receiver half.  Subsequent
    /// `set_pane_badge` calls through the trait push updates into
    /// this channel.
    pub fn attach_pane_badge_tx(&self, tx: Sender<PaneBadgeUpdate>) {
        *self.pane_badge_tx.lock().unwrap() = Some(tx);
    }

    /// Same, for wheel-key declarations.
    pub fn attach_pane_render_markup_tx(&self, tx: Sender<PaneRenderMarkupUpdate>) {
        *self.pane_render_markup_tx.lock().unwrap() = Some(tx);
    }

    pub fn attach_pane_wheel_keys_tx(&self, tx: Sender<PaneWheelKeysUpdate>) {
        *self.pane_wheel_keys_tx.lock().unwrap() = Some(tx);
    }

    /// Same shape for PaneSession take-overs.
    pub fn attach_pane_session_begin_tx(&self, tx: Sender<PaneSessionBeginRequest>) {
        *self.pane_session_begin_tx.lock().unwrap() = Some(tx);
    }

    /// Wire the channel the shell main loop drains for cc-driven
    /// PTY inject requests.  Called once during shell startup.
    pub fn attach_pty_op_tx(&self, tx: Sender<PtyOpRequest>) {
        *self.pty_op_tx.lock().unwrap() = Some(tx);
    }

    pub fn attach_inject_input_tx(&self, tx: Sender<InjectInputRequest>) {
        *self.inject_input_tx.lock().unwrap() = Some(tx);
    }

    /// Wire the channel the shell main loop drains for plugin
    /// activity reports.  Called once during shell startup.
    pub fn attach_pane_activity_tx(&self, tx: Sender<(u64, Activity)>) {
        *self.pane_activity_tx.lock().unwrap() = Some(tx);
    }

    /// Publish the sweep's result so plugin calls to `pane_status`
    /// read a snapshot instead of probing the kernel per call (a
    /// plugin walking N panes would otherwise multiply the syscall
    /// cost by however many plugins are loaded).
    pub fn publish_pane_status(&self, map: HashMap<u64, (PaneStatus, Duration, bool)>) {
        *self.pane_status.lock().unwrap() = map;
    }

    /// Refresh per-pane snapshots.  Called from the shell's tick
    /// prologue so plugins reading via `pane_pty_device` etc. see
    /// fresh data.  MVP: takes a closure that walks the actual
    /// system tree; in v1.1 wire to a system-level subscription.
    pub fn refresh_panes<F>(&self, pane_count: usize, focused: Option<usize>, mut probe: F)
    where
        F: FnMut(usize) -> PaneSnapshot,
    {
        let mut g = self.panes.lock().unwrap();
        g.clear();
        for i in 0..pane_count {
            g.push(probe(i));
        }
        *self.focused.lock().unwrap() = focused;
    }

    fn require(&self, want: PermissionSet) -> Result<&'static str, PluginError> {
        let active = self.active_plugin.lock().unwrap();
        match active.as_ref() {
            Some(ap) if ap.permissions.contains(want) => Ok(ap.name),
            Some(_) => Err(PluginError::NotPermitted(want)),
            None => {
                // Called from outside a plugin hook — internal bug.
                lx_warn!(
                    "plugin.host.no_active",
                    "host permission check fired with no active plugin"
                );
                Err(PluginError::NotPermitted(want))
            }
        }
    }

    fn plugin_name(&self) -> &'static str {
        self.active_plugin
            .lock()
            .unwrap()
            .as_ref()
            .map(|ap| ap.name)
            .unwrap_or("<unknown>")
    }
}

impl Default for ShellPluginHost {
    fn default() -> Self {
        Self::new()
    }
}

/// Concrete `InjectInputProxy` that just forwards onto the cc
/// inject-input channel.  The shell main loop drains the receiver
/// half and routes through the live core's control socket.
struct InjectInputForwarder {
    tx: Sender<InjectInputRequest>,
}

impl InjectInputForwarder {
    fn send(&self, session_id: u64, what: InjectWhat) -> std::io::Result<()> {
        self.tx
            .send(InjectInputRequest { session_id, what })
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "inject_input channel closed",
                )
            })
    }
}

impl crate::plugins::claudecode::InjectInputProxy for InjectInputForwarder {
    fn inject_input(&self, session_id: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.send(session_id, InjectWhat::Input(bytes.to_vec()))
    }

    fn hold_grid(&self, session_id: u64, on: bool) -> std::io::Result<()> {
        self.send(session_id, InjectWhat::HoldGrid(on))
    }

    fn paste(&self, session_id: u64, text: &str) -> std::io::Result<()> {
        self.send(session_id, InjectWhat::Paste(text.to_string()))
    }

    fn reset_mouse_reporting(&self, session_id: u64) -> std::io::Result<()> {
        self.send(session_id, InjectWhat::ResetMouseReporting)
    }
}

impl PluginHost for ShellPluginHost {
    fn pane_count(&self) -> usize {
        // `READ_PANE_INFO` gates the per-pane queries, but the bare
        // count is harmless and lets unauth'd plugins say "I'd need
        // permission to do my job".
        self.panes.lock().unwrap().len()
    }

    fn pane_pty_device(&self, pane: usize) -> Result<Option<PathBuf>, PluginError> {
        self.require(PermissionSet::READ_PANE_INFO)?;
        let g = self.panes.lock().unwrap();
        Ok(g.get(pane).and_then(|s| s.pty_device.clone()))
    }

    fn pane_pty_pid_tree(&self, pane: usize) -> Result<Vec<PtyChild>, PluginError> {
        self.require(PermissionSet::READ_PTY_TREE)?;
        let g = self.panes.lock().unwrap();
        Ok(g.get(pane).map(|s| s.pid_tree.clone()).unwrap_or_default())
    }

    fn pane_status(
        &self,
        shelld_session_id: u64,
    ) -> Result<Option<super::PaneStatusView>, PluginError> {
        self.require(PermissionSet::READ_PANE_INFO)?;
        Ok(self.pane_status.lock().unwrap().get(&shelld_session_id).map(
            |(status, held, quiescent)| super::PaneStatusView {
                status: status.clone(),
                held: *held,
                quiescent: *quiescent,
            },
        ))
    }

    fn report_pane_activity(
        &self,
        shelld_session_id: u64,
        activity: Activity,
    ) -> Result<(), PluginError> {
        self.require(PermissionSet::SET_STATUS_LINE)?;
        if let Some(tx) = self.pane_activity_tx.lock().unwrap().as_ref() {
            let _ = tx.send((shelld_session_id, activity));
        }
        Ok(())
    }

    fn pane_focused(&self) -> Option<usize> {
        *self.focused.lock().unwrap()
    }

    fn state_dir(&self) -> Result<PathBuf, PluginError> {
        self.require(PermissionSet::PERSIST_STATE)?;
        let plugin = self.plugin_name();
        let base = marspot::paths::state_root().join("plugins").join(plugin);
        std::fs::create_dir_all(&base)?;
        Ok(base)
    }

    fn log(&self, level: LogLevel, tag: &str, msg: &str) {
        let plugin = self.plugin_name();
        // Pre-compose `plugin.<name>.<tag>` here so consumers can just
        // `grep $'\tplugin\.'`.
        let composed = format!("plugin.{}.{}", plugin, tag);
        match level {
            LogLevel::Debug => lx_debug!(&*composed, msg),
            LogLevel::Info => lx_info!(&*composed, msg),
            LogLevel::Warn => lx_warn!(&*composed, msg),
            LogLevel::Error => lx_error!(&*composed, msg),
        }
    }

    /// Override the trait's default no-op.  Critical — the registry
    /// calls this through `&dyn PluginHost` around every hook so that
    /// `require()` permission checks see the right `ActivePlugin`.
    /// When this lived as an inherent method instead of a trait
    /// override, claudecode's init failed permission every install
    /// because the trait default fired no-op and `active_plugin`
    /// stayed None.
    fn set_active_plugin(&self, name: &'static str, permissions: PermissionSet) {
        *self.active_plugin.lock().unwrap() = Some(ActivePlugin { name, permissions });
    }

    fn clear_active_plugin(&self) {
        *self.active_plugin.lock().unwrap() = None;
    }

    fn set_pane_render_markup(
        &self,
        shelld_session_id: u64,
        on: bool,
    ) -> Result<(), PluginError> {
        self.require(PermissionSet::SET_STATUS_LINE)?;
        let Some(tx) = self.pane_render_markup_tx.lock().unwrap().clone() else {
            return Ok(());
        };
        let _ = tx.send(PaneRenderMarkupUpdate { shelld_session_id, on });
        Ok(())
    }

    fn set_pane_wheel_keys(
        &self,
        shelld_session_id: u64,
        enter: &[u8],
        up: &[u8],
        down: &[u8],
        marker: &[u8],
    ) -> Result<(), PluginError> {
        self.require(PermissionSet::SET_STATUS_LINE)?;
        // No L2 wired (standalone host, e.g. tests); drop silently.
        // A real shell always has this attached before plugins tick,
        // and remembers what it forwarded — see `pane_wheel_keys` in
        // the shell app, which re-sends on every core handshake.
        let Some(tx) = self.pane_wheel_keys_tx.lock().unwrap().clone() else {
            return Ok(());
        };
        let _ = tx.send(PaneWheelKeysUpdate {
            shelld_session_id,
            enter: enter.to_vec(),
            up: up.to_vec(),
            down: down.to_vec(),
            marker: marker.to_vec(),
        });
        Ok(())
    }

    fn set_pane_badge(
        &self,
        shelld_session_id: u64,
        text: &str,
    ) -> Result<(), PluginError> {
        self.require(PermissionSet::SET_STATUS_LINE)?;
        let Some(tx) = self.pane_badge_tx.lock().unwrap().clone() else {
            // No L2 wired (standalone host); drop silently.
            return Ok(());
        };
        let _ = tx.send(PaneBadgeUpdate {
            shelld_session_id,
            text: text.to_string(),
        });
        Ok(())
    }

    fn set_pane_title(
        &self,
        shelld_session_id: u64,
        text: &str,
    ) -> Result<(), PluginError> {
        self.require(PermissionSet::SET_STATUS_LINE)?;
        let Some(tx) = self.pane_title_tx.lock().unwrap().clone() else {
            return Ok(());
        };
        let _ = tx.send(PaneTitleUpdate {
            shelld_session_id,
            text: text.to_string(),
        });
        Ok(())
    }

    fn cc_inject_proxy(
        &self,
    ) -> Option<Arc<dyn crate::plugins::claudecode::InjectInputProxy>> {
        // Clone the tx out once, reuse it through Arc so the cc plugin
        // doesn't have to lock-and-clone on every keystroke.
        let tx = self.inject_input_tx.lock().unwrap().clone()?;
        Some(Arc::new(InjectInputForwarder { tx }))
    }

    fn submit_pty_op_at(
        &self,
        shelld_session_id: u64,
        op: crate::plugins::pty_op::PtyOp,
        start_at: usize,
    ) -> Result<(), PluginError> {
        // Same permission as `begin_pane_session`: an op *is* a pane
        // session, declared rather than hand-written.
        self.require(PermissionSet::SET_STATUS_LINE)?;
        let Some(tx) = self.pty_op_tx.lock().unwrap().clone() else {
            return Err(PluginError::Other("no L2 wired".into()));
        };
        tx.send(PtyOpRequest { shelld_session_id, op, start_at })
            .map_err(|_| PluginError::Other("main loop dropped".into()))
    }

    fn begin_pane_session(
        &self,
        shelld_session_id: u64,
        session: Box<dyn PaneSession>,
    ) -> Result<(), PluginError> {
        // No dedicated capability — the same SET_STATUS_LINE that
        // gates set_pane_badge covers PaneSession too for now; the
        // session is the meta-action plugin uses to manage that
        // badge's pane.  Plugins without permission see a clear
        // refusal instead of a silent drop.
        self.require(PermissionSet::SET_STATUS_LINE)?;
        let plugin_name = self.plugin_name();
        let Some(tx) = self.pane_session_begin_tx.lock().unwrap().clone() else {
            return Err(PluginError::Other("no L2 wired".into()));
        };
        tx.send(PaneSessionBeginRequest {
            shelld_session_id,
            plugin_name,
            session,
        })
        .map_err(|_| PluginError::Other("main loop dropped".into()))?;
        Ok(())
    }
}

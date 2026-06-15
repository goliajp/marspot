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

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::sync::Mutex;

use marspot::{lx_debug, lx_error, lx_info, lx_warn};

use super::{LogLevel, PermissionSet, PluginError, PluginHost, PtyChild};

/// Channel message the shell main loop drains and forwards to L2 core
/// as a `PaneBadge` frame.  Empty `text` = clear.
pub struct PaneBadgeUpdate {
    pub shelld_session_id: u64,
    pub text: String,
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
    /// Current plugin being invoked — used for the log namespace and
    /// permission lookup.  Set/cleared by the registry around each
    /// hook.  See `with_active_plugin`.
    active_plugin: Arc<Mutex<Option<ActivePlugin>>>,
    /// One-way pipe to the shell main loop for L2-bound side-effects.
    /// Plugin → host → channel → main loop → CoreConn::send.  None
    /// in tests / standalone hosts where no L2 is around.
    pane_badge_tx: Mutex<Option<Sender<PaneBadgeUpdate>>>,
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
            active_plugin: Arc::new(Mutex::new(None)),
            pane_badge_tx: Mutex::new(None),
        }
    }

    /// Wire the channel the shell main loop will drain for badge
    /// updates.  Called once during shell startup, after the main
    /// loop has created its receiver half.  Subsequent
    /// `set_pane_badge` calls through the trait push updates into
    /// this channel.
    pub fn attach_pane_badge_tx(&self, tx: Sender<PaneBadgeUpdate>) {
        *self.pane_badge_tx.lock().unwrap() = Some(tx);
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
}

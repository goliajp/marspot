//! The plugin's route into a pane: the L1 -> L2 -> L3 input proxy.

use super::*;

// RFC-003 Amendment 16 cc rewrite — read sessions from L3 entry.toml
// registry (where each L3 records its shell child pid), forward
// keystrokes via L1 → L2 → L3 InjectInput frame proxy.  Monitor
// (attach_raw_only) still stubbed until the L3 PTY-broadcast wire
// frame lands.

/// Replacement for the legacy ShelldClient surface — same shape so
/// the badge / profile-cycle code below didn't have to change.  Reads
/// from the L3 entry.toml registry directly; send_input proxies
/// through the PluginHost's inject_input path.
pub(super) struct ShelldClient {
    /// PluginHost handle so send_input_to can forward through the
    /// L1 → L2 → L3 wire-frame proxy.  Set by `init_with_host`
    /// when the plugin is registered with a real host (tests use
    /// `None` to skip the proxy entirely).
    host_inject: Option<Arc<dyn InjectInputProxy>>,
}

/// Indirection trait so the plugin doesn't carry a `&dyn PluginHost`
/// (the trait isn't `'static`).  Implemented by `ShellPluginHost`.
pub trait InjectInputProxy: Send + Sync {
    fn inject_input(&self, session_id: u64, bytes: &[u8]) -> std::io::Result<()>;
    /// Ask the pane's L3 to hold its picture where it is (or release
    /// it).  Separate from `PANE_SESSION_CAP_FREEZE_GRID`, which only
    /// stops L2 drawing: that one dies with the core, and every silent
    /// update restarts the core.
    fn hold_grid(&self, session_id: u64, on: bool) -> std::io::Result<()>;
    /// Deliver text the way a paste would arrive.
    fn paste(&self, session_id: u64, text: &str) -> std::io::Result<()>;
    /// Tell the pane's L3 that its foreground program was taken down
    /// by us, so mouse reporting should stop.
    fn reset_mouse_reporting(&self, session_id: u64) -> std::io::Result<()>;
}

/// The plugin's route to a pane is also the op runner's, so a script
/// can be handed the same client the rest of this plugin already uses.
impl pty_op::PtyIo for ShelldClient {
    fn send(&self, sid: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.send_input_to(sid, bytes)
    }
    fn hold(&self, sid: u64, on: bool) -> std::io::Result<()> {
        self.hold_grid_of(sid, on)
    }
    fn paste(&self, sid: u64, text: &str) -> std::io::Result<()> {
        match self.host_inject.as_ref() {
            Some(p) => p.paste(sid, text),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "no InjectInputProxy on this host (test build?)",
            )),
        }
    }
    fn program_input(&self, _: u64, _: pty_op::Typed<'_>) -> std::io::Result<()> {
        // this plugin's scripts are marspot's own; programs come through the op queue
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "no program input through the claudecode client"))
    }
    fn reset_mouse_reporting(&self, sid: u64) -> std::io::Result<()> {
        match self.host_inject.as_ref() {
            Some(p) => p.reset_mouse_reporting(sid),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "no InjectInputProxy on this host (test build?)",
            )),
        }
    }
}

impl ShelldClient {
    pub(super) fn new(host_inject: Option<Arc<dyn InjectInputProxy>>) -> Self {
        Self { host_inject }
    }

    /// Forward raw bytes into the PTY backing `sid`.  Routes through
    /// L1 → L2 control socket → L3 via the InjectInput wire frame
    /// the active core knows how to dispatch.  No bracketed-paste
    /// wrapping — the caller's bytes hit the PTY verbatim, so
    /// scripts like `claude5 --resume <uuid>\r` work even inside
    /// apps that have DECSET 2004 on.
    pub(super) fn hold_grid_of(&self, sid: u64, on: bool) -> std::io::Result<()> {
        match self.host_inject.as_ref() {
            Some(p) => p.hold_grid(sid, on),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "no InjectInputProxy on this host (test build?)",
            )),
        }
    }

    fn send_input_to(&self, sid: u64, bytes: &[u8]) -> std::io::Result<()> {
        match self.host_inject.as_ref() {
            Some(p) => p.inject_input(sid, bytes),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "no InjectInputProxy on this host (test build?)",
            )),
        }
    }

    /// PTY-broadcast subscribe — pending the L3-side PTY fan-out
    /// frame.  Returning Err disables the C7 API-error-retry monitor
    /// until that wire lands; the badge / profile-cycle paths above
    /// still work.
    pub(super) fn attach_raw_only(&self, _sid: u64) -> std::io::Result<std::sync::mpsc::Receiver<Vec<u8>>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "PTY raw broadcast TODO (no L3 fan-out frame yet)",
        ))
    }

    pub(super) fn detach_raw(&self, _sid: u64) -> std::io::Result<()> {
        Ok(())
    }
}


//! Update state machine for the shell supervisor.
//!
//! The shell is the *supervisor* in the silent-update architecture: it owns
//! the window and never restarts, the core is the worker that gets swapped
//! on every upgrade. The filesystem surface (the three-slot `binaries/`
//! tree) lives in `marspot_term::binary_tree::BinaryTree` so the daemon
//! (`marspot-shelld`) can share it without pulling in any GUI deps; this
//! module just keeps the shell-side state machine.
//!
//! ## Lifecycle (post dual-core retirement, RFC-003 single-core swap)
//!
//! ```text
//!  Idle ──→ apply_pending_update():
//!            promote pending/→current/
//!            shutdown_active (old core)
//!            spawn_core (new active from current/)
//!         ──→ Idle (immediately; no probation gate)
//! ```
//!
//! The old dual-core probation was retired with RFC-003: L3 is single-
//! client, so a parallel pending core would kick the active core off
//! every L3's UDS the moment it shook hands, dropping all input for the
//! probation window.  L3 self-execv + state.bin reattach already lets
//! each L3 survive an L2 swap on its own, so the warm-up net was both
//! wrong-shape and unnecessary.  Now the swap is a brief blackout (~200
//! ms) while the new core boots and reattaches via the registry.

// Re-export so existing `supervisor::BinaryTree::for_shell()` call sites in
// `marspot-shell/main.rs` keep resolving without a churn rename.
pub use marspot::binary_tree::BinaryTree;

/// What one probe round concluded, per layer.  `None` means that layer
/// had nothing staged and was not probed.
///
/// Both layers are probed in the same round when both have a candidate,
/// which is the common case: `install-local.sh` stages shell and core
/// together.  Knowing about the core *before* the shell execs is what
/// lets the successor spawn the new core directly — otherwise it boots
/// the old one, and a second probe + swap retires it moments later.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProbeVerdict {
    pub shell: Option<bool>,
    pub core: Option<bool>,
}

impl ProbeVerdict {
    /// Compact rendering for the log line: `shell=ok core=failed`.
    pub fn summary(&self) -> String {
        fn one(v: Option<bool>) -> &'static str {
            match v {
                Some(true) => "ok",
                Some(false) => "failed",
                None => "-",
            }
        }
        format!("shell={} core={}", one(self.shell), one(self.core))
    }
}

/// Where in the swap lifecycle the shell currently is. Lives on `ShellApp`;
/// transitions driven by either explicit user action (SIGUSR1 trigger) or
/// focus-loss.
///
/// `Probing` exists because a staged binary's very first `exec` is where
/// macOS charges the Gatekeeper assessment, and that charge has no upper
/// bound: on 2026-07-29 a concurrent cargo build flooded `syspolicyd` and
/// `marspot-core` sat in the kernel's exec path for 204 s.  The swap had
/// already killed the outgoing core by then, so the window was frozen for
/// the whole stall with no core to draw it.
///
/// So the probe runs *first*, on a background thread, against the still
/// un-promoted `pending/` file — a throwaway `--version` exec whose only
/// job is to make the kernel pay that bill while the outgoing process is
/// still on screen.  `promote_pending` uses `rename`, which preserves the
/// inode, so the verdict the probe warmed is the one the real spawn hits.
/// A slow Gatekeeper now means "the update lands later", not "the window
/// is frozen" — which is why there is deliberately no timeout here.
#[derive(Debug)]
pub enum SupervisorState {
    /// No update in flight; supervisor accepts new SIGUSR1 / focus-loss
    /// triggers.  Every completed or abandoned swap returns here.
    Idle,
    /// A background thread is exec'ing the staged binaries to warm
    /// their Gatekeeper verdicts.  Further triggers are ignored until
    /// it reports — the outgoing processes keep serving throughout.
    Probing,
}

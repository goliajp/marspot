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

/// Where in the swap lifecycle the shell currently is. Lives on `ShellApp`;
/// transitions driven by either explicit user action (SIGUSR1 trigger) or
/// focus-loss.  Post-dual-core, the only meaningful state is `Idle` — the
/// in-place swap is fully synchronous, so we re-enter `Idle` immediately
/// after `spawn_core` returns.  The enum is kept (vs collapsing to a bool)
/// so a future watchdog state (e.g. crash-loop quarantine) has a place to
/// live without re-introducing the whole machine.
#[derive(Debug)]
pub enum SupervisorState {
    /// No update in flight; supervisor accepts new SIGUSR1 / focus-loss
    /// triggers.  After `apply_pending_update` finishes the swap, the
    /// state machine returns here.
    Idle,
}

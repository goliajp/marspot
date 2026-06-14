//! Update state machine for the shell supervisor.
//!
//! The shell is the *supervisor* in the silent-update architecture: it owns
//! the window and never restarts, the core is the worker that gets swapped
//! on every upgrade. The filesystem surface (the three-slot `binaries/`
//! tree) lives in `marspot_term::binary_tree::BinaryTree` so the daemon
//! (`marspot-shelld`) can share it without pulling in any GUI deps; this
//! module just keeps the shell-side state machine.
//!
//! ## Lifecycle
//!
//! ```text
//!         ┌─── promote_pending ───┐
//!  Idle ──┤                       ├──→ Probation(30s)
//!         └─── (no pending)       │           ├──→ Stable   (deletes prev)
//!                                 │           └──→ Failed   (mv prev → current)
//!                                 │
//!                                 └──→ Failed (filesystem swap errored)
//! ```

use std::time::{Duration, Instant};

// Re-export so existing `supervisor::BinaryTree::for_shell()` call sites in
// `marspot-shell/main.rs` keep resolving without a churn rename.
pub use marspot::binary_tree::BinaryTree;

/// How long the supervisor watches a freshly-promoted binary before
/// declaring it stable. 30 s = enough for a flat-out broken binary to
/// abort during startup, short enough that an upgrade feels committed.
/// Overridable via `MARSPOT_PROBATION_S` so update-cycle soaks can iterate
/// fast; unset → 30 s, the production default.
pub fn probation() -> Duration {
    match std::env::var("MARSPOT_PROBATION_S").ok().and_then(|s| s.parse::<u64>().ok()) {
        Some(s) => Duration::from_secs(s),
        None => Duration::from_secs(30),
    }
}

/// Where in the swap lifecycle the shell currently is. Lives on `ShellApp`;
/// transitions driven by either explicit user action (refresh button
/// click), focus-loss timer, or core-child exit detection.
#[derive(Debug)]
pub enum SupervisorState {
    /// No update in flight. If `BinaryTree::has_pending()` becomes true
    /// the shell can move to `PreSwap`.
    Idle,
    /// New core has been exec'd; we're watching it for the first
    /// `probation()` seconds. `started_at` is wall-clock at the swap.
    Probation { started_at: Instant },
    /// Probation passed without the core dying. We finalize and fall
    /// back to `Idle`.
    #[allow(dead_code)]
    Stable,
    /// Swap failed — either filesystem error, or the new core died inside
    /// probation. We've already rolled back the binaries; the running
    /// core is whatever was the *previous* one.
    #[allow(dead_code)]
    Failed { reason: String },
}

impl SupervisorState {
    pub fn probation_elapsed(&self) -> bool {
        match self {
            SupervisorState::Probation { started_at } => {
                started_at.elapsed() >= probation()
            }
            _ => false,
        }
    }
}

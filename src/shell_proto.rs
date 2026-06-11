//! Shell ↔ core protocol shared types.
//!
//! For the initial cross-process IOSurface bring-up (Step 1) the
//! shell hands the core everything it needs to attach via environment
//! variables — no socket yet.  Once we add input forwarding and resize
//! negotiation (Steps 3 + 4) the same module grows a framed message
//! codec.  Keeping the contract in one place means both binaries
//! agree on the wire by construction.

/// Environment variable carrying the global IOSurface ID the core
/// should look up.  `u32` decimal.
pub const ENV_SURFACE_ID: &str = "MARSPOT_SHELL_SURFACE_ID";

/// Surface width in **physical pixels** at attach time.  Decimal `usize`.
pub const ENV_SURFACE_WIDTH: &str = "MARSPOT_SHELL_SURFACE_W";

/// Surface height in **physical pixels** at attach time.  Decimal `usize`.
pub const ENV_SURFACE_HEIGHT: &str = "MARSPOT_SHELL_SURFACE_H";

/// Backing scale factor of the shell window's screen (1.0, 2.0, …).
/// The core needs this to map between logical layout coordinates and
/// the physical pixels it writes into the surface.
pub const ENV_SURFACE_SCALE: &str = "MARSPOT_SHELL_SURFACE_SCALE";

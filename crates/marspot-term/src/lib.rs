//! marspot-term — the zero-GUI terminal engine.
//!
//! Everything marspot needs to drive a terminal session that does NOT
//! touch the window system: the VT/xterm parser, the cell grid + bounded
//! scrollback, the terminal emulator, the PTY, the shelld client +
//! protocol, the shell↔core wire protocol, layout geometry, the update
//! poller, render-input data types, and the pure key→bytes mapping.
//!
//! Deliberately depends on nothing that links AppKit/Metal/CoreText, so
//! a light per-session process (target #4's L3) links it at a ~3–5 MB
//! resident floor.  The GUI crate (`marspot`) re-exports this whole
//! surface (`pub use marspot_term::*`) so existing `marspot::grid::...`
//! paths keep resolving.  See `docs/per-session-l3.md`.

pub mod binary_tree;
pub mod bytelog;
pub mod emoji_presentation;
pub mod fast_hash;
pub mod unicode_data;
pub mod grapheme;
pub mod grid;
pub mod grid_links;
pub mod logx;
pub mod scrollback_search;

/// F1+14 — ask the system allocator to release any unused dirty
/// pages back to the kernel.  Marspot's startup paths (Terminal
/// snapshot replay, bytelog ingest, scrollback RAM ring init) all
/// peak the working set well above steady state; without an
/// explicit nudge, libmalloc keeps those pages mapped as
/// `MALLOC_LARGE (empty)` / `MALLOC_SMALL (empty)` for future
/// allocations.  `vmmap -summary` on a 9-claudecode marspot showed
/// ~50 MB per L3 sitting in that empty bucket — a free 450 MB
/// across the fleet just by asking nicely.
///
/// `malloc_zone_pressure_relief(NULL, 0)` is macOS-only; the second
/// arg is "bytes you'd LIKE freed" (0 = release everything you can).
/// Returns immediately; the kernel reclaims pages lazily.  Idempotent
/// — safe to call any time, costs roughly nothing when there's
/// nothing to release.
pub fn release_unused_memory() {
    // SAFETY: `malloc_zone_pressure_relief` is part of the public
    // libmalloc surface (macOS 10.7+).  Passing a NULL zone means
    // "all zones".
    #[cfg(target_os = "macos")]
    unsafe {
        extern "C" {
            fn malloc_zone_pressure_relief(
                zone: *mut std::ffi::c_void,
                goal: libc::size_t,
            ) -> libc::size_t;
        }
        let _ = malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
}

/// Fingerprint embedded into the binary's rodata so `install-local.sh`
/// can extract the git sha + build timestamp via `strings BIN | grep
/// MARSPOT_FP=` without running the binary. `#[used]` keeps the linker
/// from stripping it. Mirrors the equivalent constant in the root
/// `marspot` crate (`src/lib.rs::MARSPOT_FP`) so binaries that link
/// only marspot-term (notably `marspot-session`) also carry it.
#[used]
#[unsafe(no_mangle)]
pub static MARSPOT_FP_TERM: &str = concat!(
    "MARSPOT_FP=",
    env!("MARSPOT_GIT_SHA"),
    "|",
    env!("MARSPOT_BUILD_TS"),
    "|END"
);

/// Per-layer version vector — mirrors `MARSPOT_LAYER_VERS` in the
/// root `marspot` crate so binaries that link only marspot-term
/// (notably `marspot-session`) carry the same marker.  See the
/// root crate for the rationale: `install-local.sh` reads this
/// from each binary, compares against the running binary's
/// per-layer version, and skips stage + SIGUSR1 when the layer's
/// version is unchanged — so a pure-L2 change doesn't trigger the
/// L1 self-execv flash.
#[used]
#[unsafe(no_mangle)]
pub static MARSPOT_LAYER_VERS_TERM: &str = concat!(
    "MARSPOT_LAYER_VERS=shell:",
    env!("MARSPOT_VERSION_SHELL"),
    "|core:",
    env!("MARSPOT_VERSION_CORE"),
    "|session:",
    env!("MARSPOT_VERSION_SESSION"),
    "|END"
);
pub mod grid_shm;
pub mod input_core;
pub mod layout;
pub mod parser;
pub mod paths;
pub mod pty;
pub mod render;
pub mod scrollback;
pub mod session;
pub mod session_registry;
pub mod session_state;
pub mod shell_proto;
pub mod terminal;
pub mod tmux;
pub mod uds_session_client;
pub mod updater;

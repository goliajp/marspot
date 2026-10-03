//! marspot-the-library: the GUI half (AppKit window, Metal renderer,
//! CoreText glyph atlas, IOSurface bridge) plus the multi-session UI
//! facade, layered on top of the zero-GUI `marspot-term` engine.
//!
//! The terminal engine — parser, grid, terminal, PTY, shelld client +
//! protocols, layout, updater, render data types, and the pure
//! key→bytes mapping — lives in the `marspot-term` crate so a light,
//! Metal-free per-session process can link it (target #4 L3).  It is
//! re-exported below so existing
//! `marspot::grid::Grid` / `marspot::terminal::Terminal` paths keep
//! resolving and the bins need no churn.

pub use marspot_term::*;

// Macros are a separate namespace; `pub use marspot_term::*` doesn't
// carry them across crates. Re-export each one explicitly so call sites
// in this crate's binaries (marspot-shelld, marspot-core, marspot-shell)
// can `use marspot::lx_info` without reaching past the facade.
pub use marspot_term::{lx_debug, lx_error, lx_event, lx_info, lx_trace, lx_warn};

/// Fingerprint string embedded into the binary's rodata so external
/// tools can extract the git sha + build timestamp without running the
/// binary. The unique `MARSPOT_FP=` prefix makes `strings BIN | grep`
/// reliable regardless of how the linker arranges other strings in
/// rodata. `#[used]` keeps the compiler / linker from stripping this
/// even though no code reads it at runtime — `install-local.sh`
/// extracts the value via `strings BIN | grep -oE 'MARSPOT_FP=[^|]*'`.
#[used]
#[unsafe(no_mangle)]
pub static MARSPOT_FP: &str = concat!(
    "MARSPOT_FP=",
    env!("MARSPOT_GIT_SHA"),
    "|",
    env!("MARSPOT_BUILD_TS"),
    "|END"
);

/// The version of the product: what a tag is, what a release is named,
/// what the updater compares against, and what a person means when
/// they say which marspot they are on.
///
/// One definition, from `Cargo.toml`.  There used to be four numbers
/// all called "the version" and none of them agreed, which is how the
/// update channel came to be quietly dead: the shell handed the
/// updater the *core layer's* build number, and the updater compared
/// it to a release tag, so a tag could never be newer.
pub const PRODUCT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the three layers were built from.
///
/// Not versions of anything anyone buys — build numbers, and they earn
/// their keep twice: `install-local` decides from them which layers
/// changed, and they are how you tell at a glance whether the code you
/// just wrote is the code that is running.
pub mod build_ids {
    pub const SHELL: &str = env!("MARSPOT_VERSION_SHELL");
    pub const CORE: &str = env!("MARSPOT_VERSION_CORE");
    pub const SESSION: &str = env!("MARSPOT_VERSION_SESSION");
}

/// `0.4.0 (core 0.12.219)` — identity, then the build that is running.
/// Both are wanted and they are not the same question.
pub fn version_line() -> String {
    format!("{PRODUCT_VERSION} (core {})", build_ids::CORE)
}


/// Per-layer version vector embedded in the binary rodata so
/// `install-local.sh` can decide "did THIS layer's contract change?"
/// without relying on byte-compare (which sees every rebuild as
/// different even when source is unchanged) or git sha (same
/// problem — sha bumps on every commit even if the commit only
/// touches one layer).  Source of truth is `version-vector.toml`:
/// if the developer didn't bump the layer's version, install-local
/// skips that layer's stage + SIGUSR1, so a pure-L2 change doesn't
/// trigger the L1 self-execv flash.
#[used]
#[unsafe(no_mangle)]
pub static MARSPOT_LAYER_VERS: &str = concat!(
    "MARSPOT_LAYER_VERS=shell:",
    env!("MARSPOT_VERSION_SHELL"),
    "|core:",
    env!("MARSPOT_VERSION_CORE"),
    "|session:",
    env!("MARSPOT_VERSION_SESSION"),
    "|END"
);

// GUI-coupled modules (AppKit / Metal / CoreText) — these stay here.
pub mod app;
pub mod cc;
pub mod cc_usage;
pub mod dev_window;
pub mod chrome_measure;
pub mod font_cache;
pub mod font_shape;
pub mod font_trait;
pub mod frame_scene;
pub mod glyph_atlas;
pub mod input;
pub mod keyboard_layout;
pub mod link_probe;
pub mod iosurface;
pub mod pane;
pub mod pane_name;
pub mod pane_read;
pub mod pane_state;
pub mod selection_tape;
pub mod wheel_marker;
pub mod pidtree;
/// PNG writer for `--snapshot`; see the `snapshot` feature in
/// `Cargo.toml`.
#[cfg(feature = "snapshot")]
pub mod png;
pub mod qos;
pub mod shortcuts;
pub mod render_metal;
/// Shared with L3, so it lives in the stone crate — every layer
/// reads the same file, and only one of them parses it.
pub use marspot_term::settings;
pub mod state;
pub mod tools;
pub mod ui;

/// Window-chrome height in **logical points** — ONE band above the
/// grid carrying three groups on a single row: the AppKit-drawn
/// traffic lights (window left), the L2-owned toolbar icon buttons
/// (right of the lights), and the version label (flush right).
///
/// 32 = 2 × `layout::TRAFFIC_LIGHT_CENTER_Y_LOGICAL`.  AppKit places
/// its window buttons at a fixed offset from the window's top edge
/// (measured: 14pt discs spanning y 9..23, center 16) and that offset
/// does not follow our chrome height.  32pt is therefore the one band
/// height that centers the lights inside itself; everything else in
/// the header centers on the same row, so the header reads as a single
/// line rather than two stacked strips.
///
/// Shared between binaries so a single-session consumer (mcli) gets
/// the same window chrome as the multi-session container (marspot)
/// automatically.  `Layout::build` takes it as `top_inset`.
pub const HEADER_PT: f64 = 32.0;

//! marspot-the-library: the GUI half (AppKit window, Metal renderer,
//! CoreText glyph atlas, IOSurface bridge) plus the multi-session UI
//! facade, layered on top of the zero-GUI `marspot-term` engine.
//!
//! The terminal engine — parser, grid, terminal, PTY, shelld client +
//! protocols, layout, updater, render data types, and the pure
//! key→bytes mapping — lives in the `marspot-term` crate so a light,
//! Metal-free per-session process can link it (target #4 L3; see
//! `docs/per-session-l3.md`).  It is re-exported below so existing
//! `marspot::grid::Grid` / `marspot::terminal::Terminal` paths keep
//! resolving and the bins need no churn.

pub use marspot_term::*;

// GUI-coupled modules (AppKit / Metal / CoreText) — these stay here.
pub mod app;
pub mod font_cache;
pub mod glyph_atlas;
pub mod input;
pub mod iosurface;
pub mod pane;
pub mod render_metal;
pub mod ui;

/// Top chrome strip in **logical points** that every Marspot-style
/// window must reserve so the macOS traffic-light buttons don't paint
/// over the terminal grid. Shared between binaries so a single-session
/// consumer (mcli) gets the same window chrome as the multi-session
/// container (marspot) automatically.
pub const HEADER_PT: f64 = 32.0;

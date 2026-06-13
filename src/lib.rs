//! marspot-the-library: shared building blocks used by both binaries
//! (`marspot` — the multi-session app — and `mcli` — the standalone
//! single-session terminal).
//!
//! The split is deliberate: keeping `Session`, `Renderer`, `Terminal`,
//! and friends in a library that no binary owns lets us hold the line
//! on "structure and individual cleanly separated" — `Session` is
//! independent of marspot's window or layout code, and any new binary
//! (mcli today, perhaps a headless ssh-host server tomorrow) consumes
//! the same API.

pub mod app;
pub mod grid;
pub mod input;
pub mod layout;
pub mod pane;
pub mod parser;
pub mod pty;
pub mod font_cache;
pub mod glyph_atlas;
pub mod iosurface;
pub mod render;
pub mod render_metal;
pub mod scrollback;
pub mod session;
pub mod shell_proto;
pub mod shelld_client;
pub mod shelld_proto;
pub mod terminal;
pub mod tmux;
pub mod ui;
pub mod updater;

/// Top chrome strip in **logical points** that every Marspot-style
/// window must reserve so the macOS traffic-light buttons don't paint
/// over the terminal grid. Shared between binaries so a single-session
/// consumer (mcli) gets the same window chrome as the multi-session
/// container (marspot) automatically.
pub const HEADER_PT: f64 = 32.0;

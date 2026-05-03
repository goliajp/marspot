//! mars-the-library: shared building blocks used by both binaries
//! (`mars` — the multi-session app — and `mcli` — the standalone
//! single-session terminal).
//!
//! The split is deliberate: keeping `Session`, `Renderer`, `Terminal`,
//! and friends in a library that no binary owns lets us hold the line
//! on "structure and individual cleanly separated" — `Session` is
//! independent of mars's window or layout code, and any new binary
//! (mcli today, perhaps a headless ssh-host server tomorrow) consumes
//! the same API.

pub mod grid;
pub mod input;
pub mod layout;
pub mod parser;
pub mod pty;
pub mod font_cache;
pub mod glyph_atlas;
pub mod render;
pub mod render_metal;
pub mod scrollback;
pub mod session;
pub mod terminal;
pub mod tmux;

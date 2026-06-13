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

pub mod grid;
pub mod input_core;
pub mod layout;
pub mod parser;
pub mod paths;
pub mod pty;
pub mod render;
pub mod scrollback;
pub mod session;
pub mod shell_proto;
pub mod shelld_client;
pub mod shelld_proto;
pub mod terminal;
pub mod tmux;
pub mod updater;

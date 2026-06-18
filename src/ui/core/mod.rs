//! marspot UI core — foundation primitives every overlay-bearing
//! feature reaches for first.
//!
//! `core` is the "what every UI in marspot needs": a `View` that
//! handles pixel-correct positioning + always-on-top + opaque BG +
//! optional backdrop dim.  Everything else (platform-specific
//! chrome, composite widgets) is built on top of it.

pub mod view;

pub use view::{View, ViewStyle, ViewPainter, Backdrop};

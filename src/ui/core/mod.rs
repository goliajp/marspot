//! marspot UI core — foundation primitives every overlay-bearing
//! feature reaches for first.
//!
//! `core` is the "what every UI in marspot needs": a `View` that
//! handles pixel-correct positioning + always-on-top + opaque BG +
//! optional backdrop dim.  Everything else (platform-specific
//! chrome, composite widgets) is built on top of it.

pub mod view;
pub(crate) mod ink;
pub mod color;
pub mod text_fit;

pub use view::{View, ViewStyle, ViewPainter, Backdrop};
pub use color::Color;
pub use text_fit::fit_mono;

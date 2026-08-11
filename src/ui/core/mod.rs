//! marspot UI core — foundation primitives every overlay-bearing
//! feature reaches for first.
//!
//! `core` is the "what every UI in marspot needs": a `View` that
//! handles pixel-correct positioning + always-on-top + opaque BG +
//! optional backdrop dim.  Everything else (platform-specific
//! chrome, composite widgets) is built on top of it.

pub mod view;
pub mod icon;
pub mod units;
pub mod color;
pub mod canvas;
pub mod text_fit;

pub use view::{View, ViewStyle, ViewPainter, Backdrop};
pub use icon::IconComponent;
pub use units::{Pt, Pct, Length};
pub use color::Color;
pub use canvas::{Canvas, ParentRect, Primitive, RectPrim, LinePrim, TextPrim};
pub use text_fit::fit_mono;

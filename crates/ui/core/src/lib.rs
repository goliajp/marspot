//! The platform-agnostic core of a Rust GUI with no dependencies.
//!
//! Three things live here and nothing else does: how long a thing is,
//! what colour it is, and the scene a backend draws.  Widgets are a
//! layer above; windows, input and GPUs are a layer below, one crate
//! per platform.  Neither reaches across.
//!
//! The rule that keeps that true is mechanical rather than cultural:
//! this crate depends on nothing, so there is nowhere for a platform
//! detail to come in from. A test asserts that no graphics API's
//! vocabulary appears in the source.
#![forbid(unsafe_code)]

pub mod color;
pub mod scene;
pub mod units;

pub use color::Rgba8;
pub use scene::{Kind, Layer, Run, Scene};
pub use units::{Direction, Px, RectPx};

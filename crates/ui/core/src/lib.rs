//! The platform-agnostic core of a Rust GUI with no dependencies.
//!
//! What lives here: how long a thing is, what colour it is, the scene
//! a backend draws, and the tree an interface is described as — its
//! nodes, their layout, what is under the pointer, what the keyboard
//! does, and where the picture is sent.  Preset widgets are a layer
//! above; windows, fonts, input and GPUs are a layer below, one crate
//! per platform.  Neither reaches across.
//!
//! The rule that keeps that true is mechanical rather than cultural:
//! this crate depends on nothing, so there is nowhere for a platform
//! detail to come in from. A test asserts that no graphics API's
//! vocabulary appears in the source.
#![forbid(unsafe_code)]

pub mod action;
pub mod color;
pub mod geom;
pub mod hit;
pub mod interact;
pub mod layout;
pub mod metrics;
pub mod node;
pub mod paint;
pub mod scene;
mod stack;
pub mod surface;
pub mod text;
pub mod tree;
pub mod units;

pub use action::{Action, NO_ITEM, Target, WidgetId};
pub use color::Rgba8;
pub use geom::{Align, Rect, Size};
pub use hit::{Hit, hit_test};
pub use interact::{Interaction, Key, KeyResult, SCROLL_SLOTS};
pub use layout::{Constraints, Layout, ScrollExtent, TextBox};
pub use metrics::{Edges, Length, Metrics};
pub use node::{AlignCross, Distribute, GroupKind, Ink, Kind as NodeKind, Node, NodeId, Style, TextRef};
pub use paint::{Op, PaintOut, Recording, WidgetState, paint};
pub use scene::{Kind, Layer, Run, Scene};
pub use surface::{Counters, Inputs, Surface};
pub use text::{Extent, FakeMeasure, Fit, FontMetrics, FontSpec, TextFont, TextMeasure};
pub use tree::{At, Tree, Ui};
pub use units::{Px, RectPx};

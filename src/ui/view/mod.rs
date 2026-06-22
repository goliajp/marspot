//! `View` — declarative UI tree.
//!
//! See `docs/ui-system-model.md` §5+ for the full design.
//!
//! Three top-level concepts:
//! - `View` (`view.rs`): the tree node enum
//! - `Modifier` (`view.rs`): chain entries (`.padding()`, `.border()`, ...)
//! - Layout types (`types.rs`): `Edges`, `FrameSpec`, `Anchor`,
//!   `AlignCross`, `Distribute`, `Shadow`, `ActionId`, ...
//!
//! Layout algorithm + hit-test land in subsequent commits.

pub mod types;
pub mod view;
pub mod layout;
pub mod paint;
pub mod hit_test;

pub use types::{
    AlignCross, Anchor, Distribute, Edges, FrameSpec, Shadow,
    ActionId, HoverId, ViewId,
};
pub use view::{
    View, Modifier, Text, TextSize, TextWeight, TextAlign,
    TextLines, Truncate,
    vstack, hstack, zstack, spacer, filled, hairline_horiz, hairline_vert,
};
pub use layout::{
    layout as layout_view, Constraints, Size, Rect, LaidOut,
    Decoration, DecoShadow, EdgesPhys, LayoutCtx,
};
pub use paint::{paint, paint_into};
pub use hit_test::{hit_test_click, hit_test_hover, HitTarget};

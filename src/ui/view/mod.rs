//! `View` — declarative UI tree.
//!
//!
//! Three top-level concepts:
//! - `View` (`view.rs`): the tree node enum
//! - `Modifier` (`view.rs`): chain entries (`.padding()`, `.border()`, ...)
//! - Layout types (`types.rs`): `Edges`, `FrameSpec`, `Anchor`,
//!   `AlignCross`, `Distribute`, `Shadow`, `ActionId`, ...
//!
//! Layout algorithm + hit-test land in subsequent commits.

// `view::view` holds the `View` enum itself; the sibling modules are the
// algorithms over it.  Renaming either half would read worse than the
// repetition.
#[allow(clippy::module_inception)]
pub mod view;
pub mod types;
pub mod type_scale;
pub mod scroll;
pub mod state;
pub mod layout;
pub mod paint;
pub mod hit_test;

pub use types::{
    AlignCross, Anchor, Distribute, Edges, FrameSpec, Shadow,
    ActionId, HoverId, ViewId,
};
pub use view::{
    TextFontSpec, pack_shape_opts, unpack_shape_opts,
};
pub use type_scale::UiSize;
pub use view::{
    View, Modifier, Text, TextSize, TextWeight, TextStyle,
    TextLines, Truncate, ClipShape, AspectMode,
    ShapeSpec,
    LinearGradient, GradientDir, MaterialStyle,
    ToggleState, PickerState,
    vstack, hstack, zstack, spacer, filled, hairline_horiz,
    toggle, picker, grid, variable_grid, GridTrack,
    shape_circle, shape_capsule, shape_rounded_rect,
    card, panel, badge, tooltip, tab_strip, context_menu, breadcrumb, list_row,
};
pub use types::{
    ScrollWheelId, DragId, Point, Modifiers, DragInProgress,
    AnimCurve, Anim, Lerp,
    Transform,
};
pub use layout::{
    layout as layout_view, Constraints, Size, Rect, LaidOut,
    Decoration, DecoShadow, EdgesPhys, LayoutCtx,
    FontMetricsProvider, MockFontMetrics,
};
pub use paint::{paint, paint_into};
pub use hit_test::hit_test_click;
pub use scroll::{
    ScrollState,
    scroll_state, set_scroll_state, with_scroll_state, apply_scroll_delta,
};
pub use view::scroll_view;
pub use state::{
    HostState, HOST_STATE, with_host_state, with_host_state_mut,
    AnimRegistry, anim_tick, anim_any_active, anim_gc,
};
pub use types::{Key, KeyEquivalent};

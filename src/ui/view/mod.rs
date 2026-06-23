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
pub mod scroll;
pub mod state;
pub mod lazy;
pub mod layout;
pub mod paint;
pub mod hit_test;

pub use types::{
    AlignCross, Anchor, Distribute, Edges, FrameSpec, Shadow,
    ActionId, HoverId, ViewId,
};
pub use view::{
    View, Modifier, Text, TextSize, TextWeight, TextStyle, TextAlign,
    TextLines, Truncate, ClipShape, AspectMode,
    Image, ImageSource, ContentMode, ShapeSpec,
    LinearGradient, GradientDir, MaterialStyle, AxRole,
    ToggleState, PickerState,
    vstack, hstack, zstack, spacer, filled, hairline_horiz, hairline_vert,
    toggle, picker, grid, variable_grid, GridTrack,
    image_named, shape_circle, shape_capsule, shape_rounded_rect,
    card, panel, badge, tooltip, tab_strip, context_menu, breadcrumb, list_row,
};
pub use types::{
    ScrollWheelId, DragId, Point, Modifiers, InputEvent, DragInProgress,
    AnimCurve, Anim, Lerp, Transition, TransitionKind, SlideDirection,
    Transform, BlendMode,
};
pub use lazy::{lazy_vstack, lazy_hstack};
pub use layout::{
    layout as layout_view, Constraints, Size, Rect, LaidOut,
    Decoration, DecoShadow, EdgesPhys, LayoutCtx,
};
pub use paint::{paint, paint_into};
pub use hit_test::{
    hit_test_click, hit_test_double_click, hit_test_right_click,
    hit_test_scroll, hit_test_drag_begin, hit_test_hover, HitTarget,
};
pub use scroll::{
    ScrollState,
    scroll_state, set_scroll_state, with_scroll_state, apply_scroll_delta,
    forget as forget_scroll_state,
};
pub use view::scroll_view;
pub use state::{
    HostState, HOST_STATE, with_host_state, with_host_state_mut,
    reconcile, LifecycleEvent,
    AnimRegistry, anim_start, anim_tick, anim_progress, anim_any_active, anim_gc,
};
pub use types::{FocusId, Key, KeyEquivalent};

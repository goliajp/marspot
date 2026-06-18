//! marspot UI component kit (F3+1.5).
//!
//! Each submodule is a small reusable piece that produces a deterministic
//! layout (parent rect + state → child rects) plus a paint routine that
//! pushes the appropriate `UiRectInstance` / `CellInstance` / `GlyphInstance`
//! into caller-owned scratch buffers.  Components do NOT own those buffers,
//! atlases, or fonts — that ownership stays with the renderer.
//!
//! Naming convention:
//!   - `<Name>` is the layout + state holder
//!   - `<Name>::layout(parent_rect, …) -> Self` computes child rects
//!   - `<Name>::paint(…, scratches…)` pushes instances
//!   - `<Name>::hit_test(pt)` maps point → semantic action
//!
//! Components compose by holding each other's rects as parent inputs;
//! no global registry, no event bus.  Each consumer (e.g. the Process
//! Monitor modal) wires its own `mouse_down` / `key` routing.

pub mod traffic_lights;
pub mod tab_strip;
pub mod scroll_view;
pub mod modal_frame;

pub use traffic_lights::{TrafficLights, TrafficLightHit};
pub use tab_strip::TabStrip;
pub use scroll_view::ScrollView;
pub use modal_frame::ModalFrame;

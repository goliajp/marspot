//! macOS-style chrome icons (Lucide / SF-Symbols flavoured shapes
//! drawn from thin stroked rects).  Every icon implements
//! `core::IconComponent` so it can plug into any widget's icon slot
//! (`Button` today; `MenuItem` / `Toolbar` later).
//!
//! No raw paint code in scenes — pick from this list, or add a new
//! icon file here with its own unit test.

pub mod grid;
pub mod sidebar;
pub mod list_tree;

pub use grid::GridIcon;
pub use sidebar::SidebarIcon;
pub use list_tree::ListTreeIcon;

use marspot_term::layout::Rect;

/// Standard stroke width for chrome icons — tuned to read at toolbar
/// button sizes (~28pt).  All icons in this module share it so they
/// land as one visual family.
pub(crate) fn icon_stroke(inner: Rect) -> f64 {
    (inner.w.min(inner.h) * 0.07).round().max(1.0)
}

/// Inner padding inside the icon's bounding rect — gives the strokes
/// breathing room from the button BG edge.
pub(crate) fn icon_inner_rect(container: Rect) -> Rect {
    let pad = (container.w.min(container.h) * 0.22).max(2.0);
    Rect {
        x: container.x + pad,
        y_top: container.y_top + pad,
        w: (container.w - 2.0 * pad).max(1.0),
        h: (container.h - 2.0 * pad).max(1.0),
    }
}

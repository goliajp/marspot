//! `IconComponent` — the contract for a paintable icon.  Any shape
//! that wants to live inside a `Button` (or any other icon slot)
//! implements this trait.  No raw paint closures in scene code:
//! scenes only pick from concrete icons in `system/macos/icons/`
//! (or `components/icons/`, future cross-platform).
//!
//! Rationale:
//!
//! - Closures were the leak point — scenes could paint anything
//!   without leaving an artifact for reuse / inspection / testing.
//! - With a trait, every visual shape lives in a named type that
//!   carries its own paint logic + unit tests.

use marspot_term::layout::Rect;
use crate::ui::core::ViewPainter;

pub trait IconComponent {
    /// Paint the icon into `rect` using the painter's scratches.
    /// The rect is the icon's bounding box (square, set by the
    /// host widget's `style.icon_size`).
    ///
    /// `fg` is the icon's primary stroke / fill colour.  Concrete
    /// icons MUST honour it for hover state to read; ignore at
    /// your own peril.
    fn paint(&self, p: &mut ViewPainter, rect: Rect, fg: [f32; 4]);
}

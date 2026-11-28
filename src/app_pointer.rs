//! The pointer over a window: hearing that it left, and its shape.
//!
//! `mouseMoved:` is delivered while the pointer is over the key
//! window and stops at its edge, so the last move the app heard is
//! still inside it.  A tracking area over the whole view reports the
//! leaving, in an inactive app as well, and the view passes it on as a
//! move to [`OUTSIDE`].

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSCursor, NSTrackingArea, NSTrackingAreaOptions, NSView};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use crate::shell_proto::PointerShape;

/// A point in no window: where the pointer is said to be once it has
/// left one.  Physical pixels, like every other pointer position.
pub const OUTSIDE: (f64, f64) = (-1.0, -1.0);

/// Have `view` sent `mouseExited:` when the pointer leaves it.
pub(crate) fn track(view: &NSView, mtm: MainThreadMarker) {
    let options = NSTrackingAreaOptions::MouseEnteredAndExited
        | NSTrackingAreaOptions::ActiveAlways
        | NSTrackingAreaOptions::InVisibleRect;
    let owner: &AnyObject = view;
    // SAFETY: the view owns the area and outlives it; the area holds
    // its owner weakly.  The rect is ignored under `InVisibleRect`.
    let area: Retained<NSTrackingArea> = unsafe {
        NSTrackingArea::initWithRect_options_owner_userInfo(
            mtm.alloc(),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)),
            options,
            Some(owner),
            None,
        )
    };
    view.addTrackingArea(&area);
}

/// Shows `shape`, as the core asks, until something else sets the
/// pointer.  On the main thread, where AppKit's cursor lives.
pub fn set_shape(shape: PointerShape) {
    let cursor = match shape {
        PointerShape::Arrow => NSCursor::arrowCursor(),
        PointerShape::Hand => NSCursor::pointingHandCursor(),
        PointerShape::IBeam => NSCursor::IBeamCursor(),
    };
    cursor.set();
}

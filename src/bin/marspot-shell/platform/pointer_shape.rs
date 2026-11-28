//! The pointer's shape over a window's content, as the core asks.

use marspot::shell_proto::PointerShape;
use objc2_app_kit::NSCursor;

/// Shows `shape` until something else sets the pointer.  On the main
/// thread, where AppKit's cursor lives.
pub(crate) fn set(shape: PointerShape) {
    let cursor = match shape {
        PointerShape::Arrow => NSCursor::arrowCursor(),
        PointerShape::Hand => NSCursor::pointingHandCursor(),
        PointerShape::IBeam => NSCursor::IBeamCursor(),
    };
    cursor.set();
}

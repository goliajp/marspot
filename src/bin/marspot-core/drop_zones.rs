//! Where a dragged pane would land in the pane under it.

use super::DropZone;

/// RFC-006 §1 — zone geometry.  Edge bands are 25 % of the rect's
/// span on their axis, clamped to at least 48 physical px (slim panes
/// keep usable bands) and at most a third of the span (so the center
/// never vanishes).  Corners resolve to the axis with the deeper
/// penetration.
pub(super) fn drop_zone_at(rect: &marspot::layout::CellRect, px: f64, py: f64) -> DropZone {
    let band_w = (rect.w * 0.25).max(48.0 * marspot::ui::chrome_scale()).min(rect.w / 3.0);
    let band_h = (rect.h * 0.25).max(48.0 * marspot::ui::chrome_scale()).min(rect.h / 3.0);
    let from_l = px - rect.x;
    let from_r = rect.x + rect.w - px;
    let from_t = py - rect.y_top;
    let from_b = rect.y_top + rect.h - py;
    // Penetration depth into each band; ≤ 0 = not in that band.
    let pen_l = band_w - from_l;
    let pen_r = band_w - from_r;
    let pen_t = band_h - from_t;
    let pen_b = band_h - from_b;
    let best_h = pen_l.max(pen_r);
    let best_v = pen_t.max(pen_b);
    if best_h <= 0.0 && best_v <= 0.0 {
        return DropZone::Center;
    }
    if best_h >= best_v {
        if pen_l >= pen_r { DropZone::Left } else { DropZone::Right }
    } else if pen_t >= pen_b {
        DropZone::Top
    } else {
        DropZone::Bottom
    }
}

/// The preview rect a zone paints: the half of the hovered pane a
/// split would occupy, or the whole pane for a swap.
pub(super) fn drop_preview_rect(rect: &marspot::layout::CellRect, zone: DropZone) -> (f64, f64, f64, f64) {
    match zone {
        DropZone::Left => (rect.x, rect.y_top, rect.w / 2.0, rect.h),
        DropZone::Right => (rect.x + rect.w / 2.0, rect.y_top, rect.w / 2.0, rect.h),
        DropZone::Top => (rect.x, rect.y_top, rect.w, rect.h / 2.0),
        DropZone::Bottom => (rect.x, rect.y_top + rect.h / 2.0, rect.w, rect.h / 2.0),
        DropZone::Center => (rect.x, rect.y_top, rect.w, rect.h),
    }
}

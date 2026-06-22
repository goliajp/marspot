//! `LazyVStack` — virtualised vertical list.
//!
//! Combines ScrollView + uniform-height VStack in one node.  Only
//! layouts the items currently inside the viewport;  rest is skipped
//! (zero layout cost) — essential for sidebar / process panel /
//! search overlay where the list can be ≥1k rows.
//!
//! ## v1 constraints
//!
//! - Items are assumed UNIFORM height (`item_height` is the
//!   declared size; all items lay out at exactly that).
//! - Caller's responsibility to keep items uniform shape.  Variable
//!   heights = `LazyVStack` + per-item height cache via HostState —
//!   v2+ follow-up.
//! - Horizontal scroll: not supported.  `LazyHStack` will be a
//!   mirror image when needed.
//!
//! ## State
//!
//! Stored in `HostState` keyed by `id`(`ScrollState` flavour —
//! offset_y / content_h / viewport_h).Same shape as `ScrollView`
//! so the scroll-wheel pipeline routes events identically.

use crate::ui::core::Length;
use super::scroll::ScrollState;
use super::types::ViewId;
use super::view::View;

/// Wrap `items` in a LazyVStack keyed by `id`.  All items lay out
/// at exactly `item_height`;  uniform-height invariant is the
/// caller's responsibility.
pub fn lazy_vstack(id: ViewId, items: Vec<View>, item_height: Length, gap: Length) -> View {
    View::LazyVStack { id, items, item_height, gap }
}

/// Compute the visible-item index range for a given scroll offset
/// + viewport.  Returns `(first, last)` where `first <= last <=
/// items_len`.  Both indices are inclusive on first, exclusive on
/// last — like `&items[first..last]`.
pub fn visible_range(
    items_len: usize,
    item_h: f64,
    gap: f64,
    offset_y: f64,
    viewport_h: f64,
) -> (usize, usize) {
    if items_len == 0 || item_h <= 0.0 {
        return (0, 0);
    }
    let step = item_h + gap;
    let first = (offset_y / step).floor().max(0.0) as usize;
    let last = ((offset_y + viewport_h) / step).ceil() as usize + 1;
    let first = first.min(items_len);
    let last  = last .min(items_len);
    (first, last)
}

/// Compute total content height for `items_len` uniform items.
pub fn total_height(items_len: usize, item_h: f64, gap: f64) -> f64 {
    if items_len == 0 { return 0.0; }
    items_len as f64 * item_h + (items_len.saturating_sub(1)) as f64 * gap
}

/// Used by paint pass — returns the y-coordinate of the i-th item
/// within the LazyVStack's content frame(pre-scroll-shift).
pub fn item_y(i: usize, item_h: f64, gap: f64) -> f64 {
    i as f64 * (item_h + gap)
}

/// Read the current `ScrollState` for a LazyVStack's id (same
/// state shape as `ScrollView`).  Wraps `super::scroll::scroll_state`
/// so callers don't have to thread through two modules.
pub fn scroll_state(id: ViewId) -> ScrollState {
    super::scroll::scroll_state(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_range_at_top_includes_first_items() {
        // 100 items × 32 phys each, gap 0, viewport 100 phys, offset 0.
        let (first, last) = visible_range(100, 32.0, 0.0, 0.0, 100.0);
        assert_eq!(first, 0);
        // viewport / step = 100/32 ≈ 3.125 ceil = 4, +1 = 5
        assert_eq!(last, 5);
    }

    #[test]
    fn visible_range_mid_scroll() {
        // offset 320 = 10 items down; viewport 100 = 3 more.
        let (first, last) = visible_range(100, 32.0, 0.0, 320.0, 100.0);
        assert_eq!(first, 10);
        // (320+100)/32 = 13.125 ceil = 14, +1 = 15
        assert_eq!(last, 15);
    }

    #[test]
    fn visible_range_clamps_to_items_len() {
        let (first, last) = visible_range(5, 32.0, 0.0, 1000.0, 100.0);
        assert_eq!(first, 5);
        assert_eq!(last, 5);
    }

    #[test]
    fn visible_range_handles_gap() {
        // 10 items × 32 phys, gap 8 phys, step=40.  viewport 80, offset 0.
        let (first, last) = visible_range(10, 32.0, 8.0, 0.0, 80.0);
        assert_eq!(first, 0);
        // (0+80)/40 = 2 ceil = 2, +1 = 3
        assert_eq!(last, 3);
    }

    #[test]
    fn total_height_includes_gaps_between() {
        // 5 items × 32 + 4 gaps × 8 = 160 + 32 = 192
        assert_eq!(total_height(5, 32.0, 8.0), 192.0);
        assert_eq!(total_height(0, 32.0, 8.0), 0.0);
        assert_eq!(total_height(1, 32.0, 8.0), 32.0);
    }

    #[test]
    fn item_y_steps_by_item_h_plus_gap() {
        assert_eq!(item_y(0, 32.0, 8.0), 0.0);
        assert_eq!(item_y(1, 32.0, 8.0), 40.0);
        assert_eq!(item_y(5, 32.0, 8.0), 200.0);
    }
}

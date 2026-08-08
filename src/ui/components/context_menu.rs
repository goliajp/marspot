//! `ContextMenu` — right-click menu component.  Geometry-only here
//! (rect layout + hit-test); the caller in `main.rs` owns the items
//! list, the action dispatch, and the open/close state.  Render goes
//! through `View` + overlay scratches in `render_metal::push_context_menu_via_view`.
//!
//! Why a separate component (and not reuse `LayoutModal`-style modal
//! frame): a context menu has different shape rules than a modal —
//! sized to its longest item, anchored at the cursor (not centred),
//! flips to the other side of the cursor when it would overflow the
//! window, no title bar / close [×] chrome.  Sharing the
//! `ModalFrame` would make every rule below conditional.
//!
//! Wire shape:
//!   1. Caller computes the click region (Pane / Sidebar / etc.) and
//!      builds a `Vec<MenuItem>` for it.
//!   2. Caller calls `ContextMenu::layout(window_w, window_h, scale,
//!      anchor_x, anchor_y, &items)` to get a `ContextMenu` struct
//!      with `frame`, `items[i].rect`, etc.  Layout's only side-effect
//!      is anchor flipping (e.g. cursor too close to right edge
//!      flips the menu to the left of the cursor).
//!   3. Caller publishes the menu state into `Marspot::context_menu`.
//!   4. `render_metal` reads that state, walks `ContextMenu::layout`
//!      again to get rects, then paints chrome + items via
//!      `ViewPainter` (1 px **inside** border per F3+3.8 SDF shader
//!      change, opaque BG per `feedback_overlays_must_be_opaque`).
//!   5. mouse_down / mouse_moved / key_event each consult `hit_test`
//!      and emit Close / Item dispatches.

use marspot_term::layout::{Rect, Alignment};

/// One row in the menu.  `Divider` rows render as a single thin line
/// separator and ignore clicks; `enabled = false` rows render greyed
/// out and ignore clicks too.
#[derive(Debug, Clone)]
pub struct MenuItem {
    pub label: String,
    /// Right-aligned hint (typically a keyboard shortcut like
    /// `⌘C`).  Empty string = no hint.
    pub shortcut_hint: String,
    pub enabled: bool,
    pub divider: bool,
    /// Opaque integer the caller assigns to identify this row.  The
    /// component itself doesn't interpret it — `hit_test` returns the
    /// row index, and the caller maps `items[idx].action_tag` to an
    /// `Action` enum it owns.
    pub action_tag: u32,
}

impl MenuItem {
    pub fn entry(label: &str, action_tag: u32) -> Self {
        Self {
            label: label.to_string(),
            shortcut_hint: String::new(),
            enabled: true,
            divider: false,
            action_tag,
        }
    }
    pub fn with_shortcut(mut self, hint: &str) -> Self {
        self.shortcut_hint = hint.to_string();
        self
    }
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }
    pub fn divider() -> Self {
        Self {
            label: String::new(),
            shortcut_hint: String::new(),
            enabled: false,
            divider: true,
            action_tag: 0,
        }
    }
}

/// Hit-test result.  Caller uses `Item(idx)` to fire the row's
/// action (skipping disabled / divider rows — `hit_test` already
/// returns `Frame` for those).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextMenuHit {
    Item(usize),
    Frame,
    Outside,
}

pub struct ContextMenu {
    pub frame: Rect,
    /// Per-row rect (length == items.len()).  Disabled / divider
    /// rows still have a rect so the caller can paint them; only
    /// `hit_test` skips them.
    pub item_rects: Vec<Rect>,
}

/// All physical-pixel constants are LOGICAL — multiplied by `scale`
/// in `layout` so the menu stays the same physical size at every
/// display density.
const MENU_MIN_W_LOGICAL: f64 = 180.0;
const MENU_MAX_W_LOGICAL: f64 = 360.0;
const ROW_H_LOGICAL: f64 = 26.0;
const DIVIDER_H_LOGICAL: f64 = 8.0;
const SIDE_PAD_LOGICAL: f64 = 12.0;
const TOP_PAD_LOGICAL: f64 = 6.0;
const BOT_PAD_LOGICAL: f64 = 6.0;
/// Margin from anchor point to menu edge — keeps the menu from
/// covering the exact pixel under the cursor.
const ANCHOR_OFFSET_LOGICAL: f64 = 2.0;
/// Average advance per character **in physical pixels**, for sizing
/// the menu before the painter knows the real metrics.
///
/// Physical pixels, and deliberately **not** multiplied by the
/// window's scale like every other constant in this file — because
/// the thing it is estimating is not.  Panel text is drawn at
/// `pt × ViewPainter::PX_PER_PT` physical pixels whatever the display
/// reports; the boxes around it are laid out in `logical × scale`.
/// On a display where those two agree (any retina Mac) the mistake is
/// invisible.  On one where the scale is 1 the estimate came out half
/// the real advance, so the menu was built at half the width its own
/// labels needed and they ran out of it — reported 2026-08-09,
/// reproduced offscreen with `MARSPOT_SHOT_SCALE=1`.
///
/// Derived from the label's own role rather than written down, so it
/// tracks the type: the old 7.0 was sized for the mono chrome cell and
/// left menus ~75 % wider than their text once labels became SF Pro.
/// SF Pro's mean advance is close to half its point size for
/// mixed-case text.
///
/// It stays an estimate on purpose: width is clamped between
/// `MENU_MIN_W_LOGICAL` and `MENU_MAX_W_LOGICAL`, and row hit-testing
/// is by row height, never by text width — so being a few percent out
/// costs a few percent of padding and nothing else.
fn label_ch_w_phys() -> f64 {
    crate::ui::theme::PanelText::Item.pt()
        * 0.52
        * crate::ui::core::ViewPainter::PX_PER_PT
}

impl ContextMenu {
    /// Layout a menu anchored at `(anchor_x, anchor_y)` in physical
    /// pixels.  The menu prefers to extend down-right; if it would
    /// overflow the window edge it flips up / left for that axis.
    /// Always stays fully inside the window content area.
    ///
    /// `items` is the caller-built list.  `top_obstruction` carries
    /// the title-strip-y-bottom so the menu never covers the
    /// always-on-top title strip (mirrors `LayoutModal`'s
    /// `top_obstruction` arg).
    pub fn layout(
        window_w: f64,
        window_h: f64,
        scale: f64,
        anchor_x: f64,
        anchor_y: f64,
        top_obstruction: f64,
        items: &[MenuItem],
    ) -> Self {
        let row_h = ROW_H_LOGICAL * scale;
        let divider_h = DIVIDER_H_LOGICAL * scale;
        let side_pad = SIDE_PAD_LOGICAL * scale;
        let top_pad = TOP_PAD_LOGICAL * scale;
        let bot_pad = BOT_PAD_LOGICAL * scale;
        let anchor_offset = ANCHOR_OFFSET_LOGICAL * scale;
        let min_w = MENU_MIN_W_LOGICAL * scale;
        let max_w = MENU_MAX_W_LOGICAL * scale;
        let label_ch = label_ch_w_phys();

        // Width: widest item label + shortcut hint, clamped.  Shortcut
        // hint sits at the right edge with `side_pad` between it and
        // the label.
        let mut natural_w: f64 = min_w;
        for it in items {
            if it.divider {
                continue;
            }
            let label_w = it.label.chars().count() as f64 * label_ch;
            let hint_w = it.shortcut_hint.chars().count() as f64 * label_ch;
            let gap = if it.shortcut_hint.is_empty() {
                0.0
            } else {
                side_pad
            };
            let row_w = label_w + gap + hint_w + 2.0 * side_pad;
            if row_w > natural_w {
                natural_w = row_w;
            }
        }
        let menu_w = natural_w.min(max_w);

        // Height: sum of row heights.
        let body_h: f64 = items
            .iter()
            .map(|it| if it.divider { divider_h } else { row_h })
            .sum();
        let menu_h = top_pad + body_h + bot_pad;

        // Anchor + axis flip + window clamp.
        let mut x = anchor_x + anchor_offset;
        if x + menu_w > window_w {
            // Try flipping to the left of the cursor.
            let flipped = anchor_x - anchor_offset - menu_w;
            if flipped >= 0.0 {
                x = flipped;
            } else {
                // Neither side fits cleanly: clamp to right edge.
                x = (window_w - menu_w).max(0.0);
            }
        }
        let mut y = anchor_y + anchor_offset;
        if y + menu_h > window_h {
            let flipped = anchor_y - anchor_offset - menu_h;
            if flipped >= top_obstruction {
                y = flipped;
            } else {
                y = (window_h - menu_h).max(top_obstruction);
            }
        }
        if y < top_obstruction {
            y = top_obstruction;
        }
        let frame = Rect { x, y_top: y, w: menu_w, h: menu_h };

        // Per-item rects.
        let mut item_rects: Vec<Rect> = Vec::with_capacity(items.len());
        let mut cur_y = y + top_pad;
        for it in items {
            let h = if it.divider { divider_h } else { row_h };
            item_rects.push(Rect {
                x: x + side_pad * 0.0, // padded inside via paint; rect spans full width for hover bg
                y_top: cur_y,
                w: menu_w,
                h,
            });
            cur_y += h;
        }
        let _ = bot_pad;
        let _ = Alignment::Center; // kept for future text alignment

        Self { frame, item_rects }
    }

    /// Hit-test (`px, py` are physical-pixel coords).  Returns
    /// `Item(i)` only for **enabled, non-divider** rows; clicks on
    /// divider / disabled rows resolve to `Frame` (swallowed but
    /// don't dispatch).  Clicks outside the menu frame return
    /// `Outside` so the caller can dismiss + fall through to its
    /// regular click handling.
    pub fn hit_test(&self, items: &[MenuItem], px: f64, py: f64) -> ContextMenuHit {
        if !self.frame.contains(px, py) {
            return ContextMenuHit::Outside;
        }
        for (i, rect) in self.item_rects.iter().enumerate() {
            if rect.contains(px, py) {
                let it = match items.get(i) {
                    Some(it) => it,
                    None => return ContextMenuHit::Frame,
                };
                if it.divider || !it.enabled {
                    return ContextMenuHit::Frame;
                }
                return ContextMenuHit::Item(i);
            }
        }
        ContextMenuHit::Frame
    }

    /// Find the hover index for `(px, py)`.  Returns None when the
    /// cursor isn't over any enabled item (covers divider, disabled,
    /// frame padding, outside).  Used by mouse_moved to drive the
    /// hovered-row visual highlight without triggering an action.
    pub fn hover_index(&self, items: &[MenuItem], px: f64, py: f64) -> Option<usize> {
        if !self.frame.contains(px, py) {
            return None;
        }
        for (i, rect) in self.item_rects.iter().enumerate() {
            if rect.contains(px, py) {
                let it = items.get(i)?;
                if it.divider || !it.enabled {
                    return None;
                }
                return Some(i);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_items() -> Vec<MenuItem> {
        vec![
            MenuItem::entry("Copy", 1).with_shortcut("⌘C"),
            MenuItem::entry("Paste", 2).with_shortcut("⌘V"),
            MenuItem::divider(),
            MenuItem::entry("Clear scrollback", 3),
            MenuItem::entry("Close pane", 4),
        ]
    }

    #[test]
    fn menu_fits_inside_window_when_anchored_near_corner() {
        let items = sample_items();
        let m = ContextMenu::layout(1920.0, 1080.0, 1.0, 1900.0, 1070.0, 0.0, &items);
        assert!(m.frame.x >= 0.0);
        assert!(m.frame.y_top >= 0.0);
        assert!(m.frame.x + m.frame.w <= 1920.0 + 0.5);
        assert!(m.frame.y_top + m.frame.h <= 1080.0 + 0.5);
    }

    #[test]
    fn menu_respects_top_obstruction() {
        let items = sample_items();
        // Anchor near the top of the window with a 60 pt strip
        // reservation — menu must not cover the strip.
        let m = ContextMenu::layout(1920.0, 1080.0, 1.0, 100.0, 0.0, 60.0, &items);
        assert!(m.frame.y_top >= 60.0);
    }

    #[test]
    fn hit_test_returns_item_for_enabled_row() {
        let items = sample_items();
        let m = ContextMenu::layout(1920.0, 1080.0, 1.0, 200.0, 200.0, 0.0, &items);
        let r = &m.item_rects[0]; // Copy
        let hit = m.hit_test(&items, r.x + r.w * 0.5, r.y_top + r.h * 0.5);
        assert_eq!(hit, ContextMenuHit::Item(0));
    }

    #[test]
    fn hit_test_skips_divider() {
        let items = sample_items();
        let m = ContextMenu::layout(1920.0, 1080.0, 1.0, 200.0, 200.0, 0.0, &items);
        let r = &m.item_rects[2]; // divider
        let hit = m.hit_test(&items, r.x + r.w * 0.5, r.y_top + r.h * 0.5);
        assert_eq!(hit, ContextMenuHit::Frame);
    }

    #[test]
    fn hit_test_outside_frame() {
        let items = sample_items();
        let m = ContextMenu::layout(1920.0, 1080.0, 1.0, 200.0, 200.0, 0.0, &items);
        let hit = m.hit_test(&items, 1.0, 1.0);
        assert_eq!(hit, ContextMenuHit::Outside);
    }

    #[test]
    fn width_grows_with_longest_label() {
        let mut items = sample_items();
        items[0].label = "x".repeat(200);
        let m = ContextMenu::layout(1920.0, 1080.0, 1.0, 200.0, 200.0, 0.0, &items);
        // Wide label hits the MENU_MAX_W cap.
        assert!((m.frame.w - MENU_MAX_W_LOGICAL).abs() < 0.5);
    }
}

//! The context menu's rows and where it stands.
//!
//! A menu is as wide as its longest row asks, opens down and to the
//! right of the point it was asked for, flips to the other side of
//! that point on an axis where it would leave the window, and never
//! covers the title strip.  What is in it and how it is drawn are
//! `context_menu_view`'s.

use marspot_term::layout::Rect;

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

/// What a press lands on: a row that acts, the menu where nothing acts
/// (a divider, a disabled row, its padding), or the window outside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextMenuHit {
    Item(usize),
    Frame,
    Outside,
}

/// The menu's frame and each row's rectangle, worked out by plain
/// arithmetic: what the tree's layout is held to.
#[cfg(test)]
pub struct ContextMenu {
    pub frame: Rect,
    pub item_rects: Vec<Rect>,
}

// The menu's sizes are the spec's, in window units: multiplied by
// `scale` in `layout` so the menu stays the same physical size at
// every display density.
use crate::ui::theme::spec::chrome::{
    MENU_DIVIDER_H as DIVIDER_H_LOGICAL, MENU_MAX_W as MENU_MAX_W_LOGICAL,
    MENU_MIN_W as MENU_MIN_W_LOGICAL, MENU_PAD_X as SIDE_PAD_LOGICAL, MENU_PAD_Y as TOP_PAD_LOGICAL,
    MENU_PAD_Y as BOT_PAD_LOGICAL, MENU_ROW_H as ROW_H_LOGICAL,
};
/// Margin from anchor point to menu edge — keeps the menu from
/// covering the exact pixel under the cursor.
const ANCHOR_OFFSET_LOGICAL: f64 = 2.0;
/// Average advance per character **in physical pixels**, for sizing
/// the menu before the painter knows the real metrics.
///
/// Physical pixels, and deliberately **not** multiplied by the
/// window's scale like every other constant in this file — because
/// the thing it is estimating is not.  Panel text is drawn at
/// `pt × ViewPainter::px_per_pt()` physical pixels whatever the display
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
        * crate::ui::core::ViewPainter::px_per_pt()
}

#[cfg(test)]
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
        let frame = frame_of(window_w, window_h, scale, anchor_x, anchor_y, top_obstruction, items);
        let top_pad = TOP_PAD_LOGICAL * scale;
        let mut rows_h = 0.0;
        // Full width: the row's own horizontal padding is inside it,
        // and the rect is what a hover background is drawn to.
        let item_rects = items
            .iter()
            .map(|it| {
                let h = if it.divider { DIVIDER_H_LOGICAL * scale } else { ROW_H_LOGICAL * scale };
                let top = top_pad + rows_h;
                rows_h += h;
                Rect { x: frame.x, y_top: frame.y_top + top, w: frame.w, h }
            })
            .collect();
        Self { frame, item_rects }
    }
}

/// Where a menu of `items` anchored at `(anchor_x, anchor_y)` stands
/// and how large it is, device pixels.  `top_obstruction` is the y it
/// must stay under.
pub fn frame_of(
    window_w: f64,
    window_h: f64,
    scale: f64,
    anchor_x: f64,
    anchor_y: f64,
    top_obstruction: f64,
    items: &[MenuItem],
) -> Rect {
    let row_h = ROW_H_LOGICAL * scale;
    let divider_h = DIVIDER_H_LOGICAL * scale;
    let side_pad = SIDE_PAD_LOGICAL * scale;
    let top_pad = TOP_PAD_LOGICAL * scale;
    let bot_pad = BOT_PAD_LOGICAL * scale;
    let anchor_offset = ANCHOR_OFFSET_LOGICAL * scale;
    let min_w = MENU_MIN_W_LOGICAL * scale;
    let max_w = window_w.max(0.0).min(MENU_MAX_W_LOGICAL * scale);

    // a row is as wide as its label, its hint, and the padding
    // between and around them; the menu, as its widest row
    let ch_w = label_ch_w_phys();
    let text_w = |s: &str| crate::ui::core::text_fit::cells(s) as f64 * ch_w;
    let mut widest = 0.0f64;
    let mut rows_h = 0.0;
    for it in items {
        let h = if it.divider {
            divider_h
        } else {
            let mut text = text_w(&it.label);
            if !it.shortcut_hint.is_empty() {
                text = text + text_w(&it.shortcut_hint) + side_pad;
            }
            let text = text.min((max_w - (side_pad + side_pad)).max(0.0));
            widest = widest.max((text + side_pad + side_pad).min(max_w));
            row_h
        };
        rows_h += h;
    }
    let menu_w = widest.max(min_w).min(max_w);
    let menu_h = (rows_h.min((window_h - (top_pad + bot_pad)).max(0.0)) + top_pad + bot_pad)
        .min(window_h)
        .max(0.0);

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
    Rect { x, y_top: y, w: menu_w, h: menu_h }
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

    /// The menu holds its own rows, and its labels fit, at **both**
    /// densities.
    ///
    /// This one has a foot in each unit by construction: the box is
    /// `logical × scale`, the labels are drawn at `pt × px_per_pt` —
    /// which is what put the labels outside the menu on a display
    /// whose scale is 1 (2026-08-11).  Both are exercised here.
    #[test]
    fn the_menu_holds_its_rows_and_labels_at_every_density() {
        for scale in [1.0f64, 2.0] {
            crate::ui::set_chrome_scale(scale);
            let items = sample_items();
            for (w, h) in [(2160.0 * scale, 1300.0 * scale), (700.0, 500.0)] {
                for (ax, ay) in [(10.0, 10.0), (w - 20.0, h - 20.0), (w * 0.5, h * 0.5)] {
                    let m = ContextMenu::layout(w, h, scale, ax, ay, 0.0, &items);
                    let f = m.frame;
                    assert!(
                        f.x >= -1e-6 && f.y_top >= -1e-6
                            && f.x + f.w <= w + 1e-6 && f.y_top + f.h <= h + 1e-6,
                        "scale {scale} @{ax},{ay}: menu escapes the window",
                    );
                    for (i, r) in m.item_rects.iter().enumerate() {
                        assert!(
                            r.y_top >= f.y_top - 1e-6
                                && r.y_top + r.h <= f.y_top + f.h + 1e-6,
                            "scale {scale}: row {i} outside the menu",
                        );
                    }
                    // The widest label plus its padding has to fit the
                    // frame the estimate produced.
                    let widest = items
                        .iter()
                        .filter(|i| !i.divider)
                        .map(|i| i.label.chars().count())
                        .max()
                        .unwrap_or(0) as f64
                        * label_ch_w_phys();
                    assert!(
                        widest <= f.w + 1e-6,
                        "scale {scale}: {widest:.0}px of label in a {:.0}px menu",
                        f.w,
                    );
                }
            }
        }
        crate::ui::set_chrome_scale(1.0);
    }

    #[test]
    fn width_grows_with_longest_label() {
        let mut items = sample_items();
        items[0].label = "x".repeat(200);
        let m = ContextMenu::layout(1920.0, 1080.0, 1.0, 200.0, 200.0, 0.0, &items);
        // Wide label hits the MENU_MAX_W cap.
        assert!((m.frame.w - MENU_MAX_W_LOGICAL).abs() < 0.5);
    }

    /// The menu has one unit of length, the window unit — half a panel
    /// point — and its labels are in points: a row holds the same label, with the same
    /// room around it, at every scale.
    #[test]
    fn a_menu_unit_is_a_window_unit_at_every_scale() {
        use crate::ui::theme::spec::{self, chrome};
        for scale in [1.0, 2.0] {
            crate::ui::components::density_kit::at_scale(scale, || {
                assert_eq!(chrome::px(1.0), crate::ui::chrome_scale());
                assert_eq!(2.0 * chrome::px(1.0), spec::px(1.0), "two window units to the panel point");
                let m = ContextMenu::layout(4000.0, 4000.0, crate::ui::chrome_scale(), 100.0, 100.0, 0.0, &sample_items());
                assert_eq!(m.item_rects[0].h, chrome::px(chrome::MENU_ROW_H));
                // a label's capitals, in points, against the row, in window units
                let cap = spec::px(crate::ui::theme::PanelText::Item.cap());
                assert_eq!(m.item_rects[0].h / cap, chrome::MENU_ROW_H / (2.0 * crate::ui::theme::PanelText::Item.cap()));
            });
        }
    }
}

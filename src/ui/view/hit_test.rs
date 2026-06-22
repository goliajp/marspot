//! Hit testing — `LaidOut` tree + pointer (phys) → optional
//! `ActionId` / `HoverId`.
//!
//! Walks the tree post-order (children before parents) so deeper /
//! later-submitted views win when overlapping.  Matches submission
//! order = z order at paint time.
//!
//! See `docs/ui-system-model.md` §8.

use super::layout::LaidOut;
use super::types::{ActionId, HoverId, ScrollWheelId, DragId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HitTarget {
    Click(ActionId),
    Hover(HoverId),
}

/// Find the deepest / topmost click target containing the point.
/// `None` when no view at that position carries an `OnClick` modifier.
pub fn hit_test_click(laid: &LaidOut, p: (f64, f64)) -> Option<ActionId> {
    hit_test_click_clipped(laid, p, None)
}

fn hit_test_click_clipped(laid: &LaidOut, p: (f64, f64), clip: Option<&super::layout::Rect>) -> Option<ActionId> {
    hit_test_field(laid, p, clip, |d| d.on_click)
}

/// Same as `hit_test_click` but for hover regions.
pub fn hit_test_hover(laid: &LaidOut, p: (f64, f64)) -> Option<HoverId> {
    hit_test_hover_clipped(laid, p, None)
}

fn hit_test_hover_clipped(laid: &LaidOut, p: (f64, f64), clip: Option<&super::layout::Rect>) -> Option<HoverId> {
    if laid.deco.hidden { return None; }
    if let Some(c) = clip { if !c.contains(p) { return None; } }
    if !laid.rect.contains(p) { return None; }
    let child_clip = if establishes_clip(&laid.view, &laid.deco) {
        Some(&laid.rect)
    } else { clip };
    for ch in laid.children.iter().rev() {
        if let Some(h) = hit_test_hover_clipped(ch, p, child_clip) {
            return Some(h);
        }
    }
    laid.deco.on_hover
}

/// `Modifier::OnDoubleClick(...)` target lookup — same walk shape
/// as `hit_test_click` but reads a different deco field.
pub fn hit_test_double_click(laid: &LaidOut, p: (f64, f64)) -> Option<ActionId> {
    hit_test_field(laid, p, None, |d| d.on_double_click)
}

/// `Modifier::OnRightClick(...)` target lookup.
pub fn hit_test_right_click(laid: &LaidOut, p: (f64, f64)) -> Option<ActionId> {
    hit_test_field(laid, p, None, |d| d.on_right_click)
}

/// `Modifier::OnScroll(...)` target lookup — used to route wheel
/// events to the right `ScrollWheelId`.
pub fn hit_test_scroll(laid: &LaidOut, p: (f64, f64)) -> Option<ScrollWheelId> {
    hit_test_field(laid, p, None, |d| d.on_scroll)
}

/// `Modifier::OnDragBegin(...)` target lookup — mouse-down hit
/// during gesture begin.
pub fn hit_test_drag_begin(laid: &LaidOut, p: (f64, f64)) -> Option<DragId> {
    hit_test_field(laid, p, None, |d| d.on_drag_begin)
}

fn hit_test_field<T: Copy>(
    laid: &LaidOut,
    p: (f64, f64),
    clip: Option<&super::layout::Rect>,
    field: impl Fn(&super::layout::Decoration) -> Option<T> + Copy,
) -> Option<T> {
    if laid.deco.hidden { return None; }
    if let Some(c) = clip { if !c.contains(p) { return None; } }
    if !laid.rect.contains(p) { return None; }
    let child_clip = if establishes_clip(&laid.view, &laid.deco) {
        Some(&laid.rect)
    } else { clip };
    for ch in laid.children.iter().rev() {
        if let Some(h) = hit_test_field(ch, p, child_clip, field) {
            return Some(h);
        }
    }
    field(&laid.deco)
}

fn establishes_clip(view: &super::view::View, deco: &super::layout::Decoration) -> bool {
    matches!(view, super::view::View::ScrollView { .. }) || deco.clip.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::view::{
        Text, vstack,
        Constraints, LayoutCtx, layout_view,
        Edges,
    };
    use crate::ui::core::Length;

    fn ctx() -> LayoutCtx {
        LayoutCtx { scale: 2.0, cell_w_phys: 16.0, cell_h_phys: 32.0, ascent_phys: 24.0 }
    }

    #[test]
    fn click_lands_on_inner_view_not_outer() {
        // VStack contains two rows; only second has on_click.
        let v = vstack(vec![
            Text::new("plain").build(),
            Text::new("clicky").build()
                .padding(Edges::all(Length::Pt(4.0)))
                .on_click(ActionId(42)),
        ]);
        let l = layout_view(&v, ctx(), (0.0, 0.0), Constraints::loose(1000.0, 1000.0));
        // First row at y∈[0, 32).  Second row at y > 32.
        assert_eq!(hit_test_click(&l, (10.0, 10.0)), None);
        assert_eq!(hit_test_click(&l, (10.0, 50.0)), Some(ActionId(42)));
    }

    #[test]
    fn click_outside_subtree_is_none() {
        let v = Text::new("hi").build()
            .frame(crate::ui::view::FrameSpec {
                width: Some(Length::Pt(50.0)),
                height: Some(Length::Pt(20.0)),
                ..Default::default()
            })
            .on_click(ActionId(7));
        let l = layout_view(&v, ctx(), (0.0, 0.0), Constraints::loose(1000.0, 1000.0));
        // Frame is 50pt × 20pt @ scale 2 = 100 × 40 phys.
        assert_eq!(hit_test_click(&l, (50.0, 20.0)), Some(ActionId(7)));
        assert_eq!(hit_test_click(&l, (200.0, 100.0)), None);
    }
}

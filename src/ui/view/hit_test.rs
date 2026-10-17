//! Hit testing — `LaidOut` tree + pointer (phys) → optional
//! `ActionId` / `HoverId`.
//!
//! Walks the tree post-order (children before parents) so deeper /
//! later-submitted views win when overlapping.  Matches submission
//! order = z order at paint time.
//!

use super::layout::LaidOut;
use super::types::ActionId;

/// Find the deepest / topmost click target containing the point.
/// `None` when no view at that position carries an `OnClick` modifier.
pub fn hit_test_click(laid: &LaidOut, p: (f64, f64)) -> Option<ActionId> {
    hit_test_click_clipped(laid, p, None)
}

fn hit_test_click_clipped(laid: &LaidOut, p: (f64, f64), clip: Option<&super::layout::Rect>) -> Option<ActionId> {
    hit_test_field(laid, p, clip, |d| d.on_click)
}

fn hit_test_field<T: Copy>(
    laid: &LaidOut,
    p: (f64, f64),
    clip: Option<&super::layout::Rect>,
    field: impl Fn(&super::layout::Decoration) -> Option<T> + Copy,
) -> Option<T> {
    if laid.deco.hidden { return None; }
    if let Some(c) = clip && !c.contains(p) { return None; }
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
        Constraints, LayoutCtx, layout_view, MockFontMetrics,
        Edges,
    };
    use crate::ui::core::Length;

    static TEST_FONTS: MockFontMetrics = MockFontMetrics {
        cell_w_phys: 16.0,
        cell_h_phys: 32.0,
    };

    fn ctx() -> LayoutCtx<'static> {
        LayoutCtx {
            scale: 2.0,
            cell_w_phys: 16.0,
            cell_h_phys: 32.0,
            ascent_phys: 24.0,
            fonts: &TEST_FONTS,
        }
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

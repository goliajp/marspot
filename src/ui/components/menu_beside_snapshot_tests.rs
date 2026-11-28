//! A context menu with a menu open beside one of its rows, the
//! pointer on a row of that menu, and one with the reason a disabled
//! row is off said under it, at both display scales.  Baselines
//! `bench/ui/snapshots/chrome_menu_{sub,tip}{,.2x}.png`.

use super::super::context_menu_beside_tests::items;
use super::super::context_menu_view::MenuRender;
use super::super::density_kit;
use super::{four_panes, snapshot};
use crate::render_metal::MetalRenderer;

/// The menu of `items()` over the window, the pointer on row `on` of
/// the menu, then on row `then` of the menu beside when one is open.
fn open(renderer: &mut MetalRenderer, top: f64, on: usize, then: Option<usize>) {
    let scale = crate::ui::chrome_scale();
    let window = (1200.0 * scale, 760.0 * scale);
    let mut m = MenuRender::new((420.0 * scale, 150.0 * scale), top, items());
    let at = |m: &MenuRender| m.surface.borrow().row_centre(on).expect("the row");
    m.surface.borrow_mut().lay_out(renderer.font_mut(), &m, window);
    let p = at(&m);
    m.surface.borrow_mut().pointer_move(p.0, p.1);
    m.surface.borrow_mut().lay_out(renderer.font_mut(), &m, window);
    m.pointer = Some(p);
    if let Some(j) = then {
        let q = m.surface.borrow().sub_row_centre(j).expect("a row of the menu beside");
        m.surface.borrow_mut().pointer_move(q.0, q.1);
        m.pointer = Some(q);
    }
    renderer.set_context_menu(Some(m));
}

#[test]
pub(super) fn chrome_snapshot_menu_sub() {
    snapshot("chrome_menu_sub", (1200, 760), (2, 2), &four_panes(), 0, |renderer, _, shape| {
        open(renderer, shape.layout.top_inset, 2, Some(1));
    });
}

#[test]
pub(super) fn chrome_snapshot_menu_tip() {
    snapshot("chrome_menu_tip", (1200, 760), (2, 2), &four_panes(), 0, |renderer, _, shape| {
        open(renderer, shape.layout.top_inset, 3, None);
    });
}

#[test]
fn chrome_snapshot_menu_sub_2x() {
    density_kit::at_scale(2.0, chrome_snapshot_menu_sub);
}

#[test]
fn chrome_snapshot_menu_tip_2x() {
    density_kit::at_scale(2.0, chrome_snapshot_menu_tip);
}

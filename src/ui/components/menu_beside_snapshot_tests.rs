//! A context menu with a menu open beside one of its rows, the
//! pointer on a row of that menu, and one with the reason a disabled
//! row is off said under it, and a pane's menu with "Replace with…"
//! open, at both display scales.  Baselines
//! `bench/ui/snapshots/chrome_menu_{sub,tip,replace}{,.2x}.png`.

use super::super::context_menu_beside_tests::items;
use super::super::context_menu::{grouped, MenuItem};
use super::super::context_menu_view::MenuRender;
use super::super::density_kit;
use super::{four_panes, snapshot};
use crate::render_metal::MetalRenderer;
use crate::ui::strings::{t, text, Arg, Msg};

/// The menu of `items()` over the window, the pointer on row `on` of
/// the menu, then on row `then` of the menu beside when one is open.
fn open(renderer: &mut MetalRenderer, top: f64, items: Vec<MenuItem>, on: usize, then: Option<usize>) {
    let scale = crate::ui::chrome_scale();
    let window = (1200.0 * scale, 760.0 * scale);
    let mut m = MenuRender::new((420.0 * scale, 150.0 * scale), top, items);
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
        open(renderer, shape.layout.top_inset, items(), 2, Some(1));
    });
}

#[test]
pub(super) fn chrome_snapshot_menu_tip() {
    snapshot("chrome_menu_tip", (1200, 760), (2, 2), &four_panes(), 0, |renderer, _, shape| {
        open(renderer, shape.layout.top_inset, items(), 3, None);
    });
}

/// A pane's menu: "Replace with…" in the group of moves, open beside,
/// a new shell first, then the hidden panes, a child with its parent's
/// code; the pointer on the child.
#[test]
pub(super) fn chrome_snapshot_menu_replace() {
    let hidden = vec![
        MenuItem::entry(t(Msg::MenuNewShell), 1),
        MenuItem::divider(),
        MenuItem::entry(&format!("#8590 madpdf{}", text(Arg::ChildOf(5528))), 2),
        MenuItem::entry("#2210 lab", 3),
    ];
    let items = grouped([
        vec![MenuItem::entry(t(Msg::MenuCopy), 0).disabled(), MenuItem::entry(t(Msg::MenuPaste), 0)],
        vec![MenuItem::entry(t(Msg::MenuClearScrollback), 0)],
        vec![MenuItem::entry(t(Msg::MenuNewPane), 0), MenuItem::entry(t(Msg::MenuClosePane), 0)],
        vec![MenuItem::entry(t(Msg::MenuMoveToNewWindow), 0), MenuItem::entry(t(Msg::MenuReplaceWith), 0).with_submenu(hidden)],
    ]);
    let on = items.len() - 1;
    snapshot("chrome_menu_replace", (1200, 760), (2, 2), &four_panes(), 0, |renderer, _, shape| {
        open(renderer, shape.layout.top_inset, items.clone(), on, Some(2));
    });
}

#[test]
fn chrome_snapshot_menu_replace_2x() {
    density_kit::at_scale(2.0, chrome_snapshot_menu_replace);
}

#[test]
fn chrome_snapshot_menu_sub_2x() {
    density_kit::at_scale(2.0, chrome_snapshot_menu_sub);
}

#[test]
fn chrome_snapshot_menu_tip_2x() {
    density_kit::at_scale(2.0, chrome_snapshot_menu_tip);
}

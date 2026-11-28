//! A menu opened beside a row, by the pointer and the keyboard, and the
//! reason a disabled row is off, at both display scales.

use super::context_menu::{ContextMenuHit, MenuItem};
use super::context_menu_view::{MenuKey, MenuRender};
use crate::font_cache::FontCache;
use crate::ui::components::density_kit::at_scale;
use crate::ui::tree::preset;
use golia_ui_core::{Key, NodeId};

pub(super) fn items() -> Vec<MenuItem> {
    let parents = vec![MenuItem::entry("#4145 smix", 10), MenuItem::entry("#6004 alt", 11)];
    vec![
        MenuItem::entry("Close pane", 1),
        MenuItem::divider(),
        MenuItem::entry("Belongs to…", 2).with_submenu(parents),
        MenuItem::entry("Move to New Window", 3).disabled_because("A pane with child panes can't belong to another pane"),
    ]
}

fn open_at(font: &mut FontCache, window: (f64, f64), anchor: (f64, f64)) -> MenuRender {
    let m = MenuRender::new(anchor, 0.0, items());
    m.surface.borrow_mut().lay_out(font, &m, window);
    m
}

/// The frames the tree holds, in the order described.
fn frames(m: &MenuRender) -> Vec<golia_ui_core::Rect> {
    let s = m.surface.borrow();
    let (tree, layout) = (s.kept.surface.tree(), s.kept.surface.layout());
    let tagged = tree.nodes().iter().enumerate().filter(|(_, n)| n.leaf_tag == preset::MENU_FRAME);
    tagged.map(|(i, _)| layout.rect(NodeId(i as u32))).collect()
}

fn tip_label(m: &MenuRender) -> Option<String> {
    let s = m.surface.borrow();
    let tree = s.kept.surface.tree();
    let n = tree.nodes().iter().find(|n| n.leaf_tag == preset::MENU_TIP)?;
    Some(tree.text_of(n).to_string())
}

#[test]
fn a_row_that_opens_a_menu_opens_it_beside_and_a_row_of_it_acts() {
    for scale in [1.0, 2.0] {
        at_scale(scale, || {
            let mut font = FontCache::build().expect("font");
            let window = (1600.0 * scale, 1000.0 * scale);
            let m = open_at(&mut font, window, (200.0 * scale, 200.0 * scale));
            assert_eq!(frames(&m).len(), 1, "nothing beside until asked");
            let opens = m.surface.borrow().row_centre(2).expect("the row");
            assert!(m.surface.borrow_mut().pointer_move(opens.0, opens.1), "the menu beside opening is a new picture");
            m.surface.borrow_mut().lay_out(&mut font, &m, window);
            let f = frames(&m);
            assert_eq!(f.len(), 2, "scale {scale}: the menu beside is described");
            assert!((f[1].x - (f[0].x + f[0].w)).abs() < 0.01, "scale {scale}: against the menu's right edge: {f:?}");
            let sub = m.surface.borrow().sub_row_centre(1).expect("its second row");
            assert!((sub.1 - opens.1).abs() < f[0].h, "level with its row");
            // on the way there the pointer crosses nothing that closes it
            assert!(!m.surface.borrow_mut().pointer_move(sub.0, sub.1) || frames(&m).len() == 2);
            assert_eq!(m.surface.borrow_mut().press(sub.0, sub.1), ContextMenuHit::SubItem(2, 1));
            // onto another row of the menu: closed
            let close = m.surface.borrow().row_centre(0).expect("the first row");
            assert!(m.surface.borrow_mut().pointer_move(close.0, close.1));
            m.surface.borrow_mut().lay_out(&mut font, &m, window);
            assert_eq!(frames(&m).len(), 1, "scale {scale}: closed on another row");
            // a press on the row opens it too, and acts on nothing
            assert_eq!(m.surface.borrow_mut().press(opens.0, opens.1), ContextMenuHit::Frame);
            m.surface.borrow_mut().lay_out(&mut font, &m, window);
            assert_eq!(frames(&m).len(), 2);
        });
    }
}

#[test]
fn a_menu_beside_where_the_window_ends_stands_on_the_left() {
    for scale in [1.0, 2.0] {
        at_scale(scale, || {
            let mut font = FontCache::build().expect("font");
            let window = (900.0 * scale, 600.0 * scale);
            let m = open_at(&mut font, window, (860.0 * scale, 500.0 * scale));
            let opens = m.surface.borrow().row_centre(2).expect("the row");
            m.surface.borrow_mut().pointer_move(opens.0, opens.1);
            m.surface.borrow_mut().lay_out(&mut font, &m, window);
            let f = frames(&m);
            assert!((f[1].x + f[1].w - f[0].x).abs() < 0.01, "scale {scale}: against the left edge: {f:?}");
            assert!(f[1].y + f[1].h <= window.1 + 0.01, "inside the window");
        });
    }
}

#[test]
fn the_keyboard_goes_down_the_menu_into_the_menu_beside_and_back() {
    let mut font = FontCache::build().expect("font");
    let window = (1600.0, 1000.0);
    let m = open_at(&mut font, window, (200.0, 200.0));
    let key = |k: Key| m.surface.borrow_mut().key(k);
    assert_eq!(key(Key::Down), MenuKey::Moved, "onto the first row");
    assert_eq!(key(Key::Down), MenuKey::Moved, "past the divider, onto the row that opens");
    assert_eq!(key(Key::Down), MenuKey::Moved, "the disabled row is passed over: the keyboard stays");
    assert_eq!(key(Key::Right), MenuKey::Opened);
    m.surface.borrow_mut().lay_out(&mut font, &m, window);
    m.surface.borrow_mut().key_into_sub();
    assert_eq!(key(Key::Down), MenuKey::Moved);
    assert_eq!(key(Key::Enter), MenuKey::SubItem(2, 1), "the second row of the menu beside");
    assert_eq!(key(Key::Left), MenuKey::Moved, "back to the row that opened it");
    m.surface.borrow_mut().lay_out(&mut font, &m, window);
    assert_eq!(frames(&m).len(), 1, "and it closed");
    assert_eq!(key(Key::Up), MenuKey::Moved);
    assert_eq!(key(Key::Enter), MenuKey::Item(0), "Close pane");
}

#[test]
fn a_disabled_row_says_why_while_the_pointer_is_on_it() {
    for scale in [1.0, 2.0] {
        at_scale(scale, || {
            let mut font = FontCache::build().expect("font");
            let window = (1600.0 * scale, 1000.0 * scale);
            let m = open_at(&mut font, window, (200.0 * scale, 200.0 * scale));
            let off = m.surface.borrow().row_centre(3).expect("the disabled row");
            assert!(m.surface.borrow_mut().pointer_move(off.0, off.1), "the reason coming is a new picture");
            m.surface.borrow_mut().lay_out(&mut font, &m, window);
            assert_eq!(tip_label(&m).as_deref(), Some("A pane with child panes can't belong to another pane"));
            assert_eq!(m.surface.borrow_mut().press(off.0, off.1), ContextMenuHit::Frame, "the row still acts on nothing");
            let on = m.surface.borrow().row_centre(0).expect("the first row");
            assert!(m.surface.borrow_mut().pointer_move(on.0, on.1));
            m.surface.borrow_mut().lay_out(&mut font, &m, window);
            assert_eq!(tip_label(&m), None, "gone with the pointer");
        });
    }
}

//! The context menu's surface, without a frame drawn: its rows stand
//! where the menu's geometry puts them, a press finds the row that
//! acts and nothing else, and a frame of an open menu costs nothing.

use super::context_menu::{ContextMenu, ContextMenuHit, MenuItem};
use super::context_menu_view::MenuRender;
use crate::font_cache::FontCache;
use crate::frame_alloc_tests::counted;
use crate::render_metal::{new_atlas, system_default_device};
use crate::ui::components::density_kit::at_scale;
use crate::ui::core::ViewPainter;
use crate::ui::tree::preset;
use golia_ui_core::NodeId;

fn items() -> Vec<MenuItem> {
    vec![
        MenuItem::entry("Copy", 1).with_shortcut("⌘C").disabled(),
        MenuItem::entry("Paste", 2).with_shortcut("⌘V"),
        MenuItem::divider(),
        MenuItem::entry("Clear scrollback", 3),
        MenuItem::entry("Close pane", 4),
    ]
}

fn laid_out(font: &mut FontCache, window: (f64, f64), anchor: (f64, f64), top: f64) -> MenuRender {
    let m = MenuRender::new(anchor, top, items());
    m.surface.borrow_mut().lay_out(font, &m, window);
    m
}

#[test]
fn the_frame_and_every_row_stand_where_the_menus_geometry_puts_them() {
    for scale in [1.0, 2.0] {
        at_scale(scale, || {
            let mut font = FontCache::build().expect("font");
            // roomy, against each edge, at half a pixel, and in a
            // window too short to hold the rows
            for (window, anchor, top) in [
                ((1600.0, 1000.0), (300.0, 200.0), 0.0),
                ((1600.0, 1000.0), (1590.0, 990.0), 0.0),
                ((1600.0, 1000.0), (40.0, 3.0), 60.0),
                ((1600.0, 1000.0), (211.5, 97.25), 44.0),
                ((500.0, 90.0), (20.0, 20.0), 0.0),
                ((120.0, 400.0), (60.0, 100.0), 0.0),
            ] {
                let (window, anchor) = ((window.0 * scale, window.1 * scale), (anchor.0 * scale, anchor.1 * scale));
                let m = laid_out(&mut font, window, anchor, top * scale);
                let want = ContextMenu::layout(window.0, window.1, scale, anchor.0, anchor.1, top * scale, &m.items);
                let surface = m.surface.borrow();
                let (tree, layout) = (surface.kept.surface.tree(), surface.kept.surface.layout());
                let tagged = |tag: u32| {
                    tree.nodes()
                        .iter()
                        .enumerate()
                        .filter(move |(_, n)| n.leaf_tag == tag)
                        .map(|(i, _)| layout.rect(NodeId(i as u32)))
                };
                let frame = tagged(preset::MENU_FRAME).next().expect("a frame");
                assert_eq!(
                    (frame.x, frame.y, frame.w, frame.h),
                    (want.frame.x, want.frame.y_top, want.frame.w, want.frame.h),
                    "scale {scale}, {window:?} @{anchor:?}: the frame"
                );
                let mut rows = tagged(preset::MENU_ROW);
                let mut dividers = tagged(preset::MENU_DIVIDER);
                for (i, (it, r)) in m.items.iter().zip(&want.item_rects).enumerate() {
                    let got = if it.divider { dividers.next() } else { rows.next() }.expect("a node for the row");
                    assert_eq!(
                        (got.x, got.y, got.w, got.h),
                        (r.x, r.y_top, r.w, r.h),
                        "scale {scale}, {window:?} @{anchor:?}: row {i}"
                    );
                }
                assert!(rows.next().is_none() && dividers.next().is_none(), "more nodes than rows");
            }
        });
    }
}

#[test]
fn a_press_finds_the_row_that_acts_and_nothing_else() {
    for scale in [1.0, 2.0] {
        at_scale(scale, || {
            let mut font = FontCache::build().expect("font");
            let window = (1600.0 * scale, 1000.0 * scale);
            let anchor = (200.0 * scale, 200.0 * scale);
            let m = laid_out(&mut font, window, anchor, 0.0);
            let want = ContextMenu::layout(window.0, window.1, scale, anchor.0, anchor.1, 0.0, &m.items);
            let mut s = m.surface.borrow_mut();
            let mid = |i: usize| {
                let r = want.item_rects[i];
                (r.x + r.w * 0.5, r.y_top + r.h * 0.5)
            };
            for (i, hit) in [
                (1, ContextMenuHit::Item(1)),
                (3, ContextMenuHit::Item(3)),
                (4, ContextMenuHit::Item(4)),
                // a disabled row and a divider swallow the press
                (0, ContextMenuHit::Frame),
                (2, ContextMenuHit::Frame),
            ] {
                let (x, y) = mid(i);
                assert_eq!(s.press(x, y), hit, "scale {scale}: row {i}");
                // a row is hit across its whole width, edge to edge
                let r = want.item_rects[i];
                assert_eq!(s.press(r.x + 0.5, r.y_top + 0.5), hit, "scale {scale}: row {i}'s corner");
                assert_eq!(s.press(r.x + r.w - 0.5, r.y_top + r.h - 0.5), hit, "scale {scale}: row {i}'s far corner");
            }
            // the padding above the first row is the menu's and acts on nothing
            let f = want.frame;
            assert_eq!(s.press(f.x + f.w * 0.5, f.y_top + 1.0), ContextMenuHit::Frame);
            assert_eq!(s.press(f.x + f.w * 0.5, f.y_top + f.h - 1.0), ContextMenuHit::Frame);
            assert_eq!(s.press(f.x - 1.0, f.y_top + 10.0), ContextMenuHit::Outside);
            assert_eq!(s.press(1.0, 1.0), ContextMenuHit::Outside);

            // the pointer lights a row that acts and no other
            assert!(s.pointer_move(mid(1).0, mid(1).1), "the pointer came onto a row and nothing changed");
            assert!(!s.pointer_move(mid(1).0 + 1.0, mid(1).1), "the pointer stayed on the row and something changed");
            assert!(s.pointer_move(mid(0).0, mid(0).1), "the pointer left the row for a disabled one");
            assert!(!s.pointer_move(mid(2).0, mid(2).1), "from a disabled row to a divider, nothing is lit either way");
        });
    }
}

/// A frame of an open menu: nothing while nothing changes, a repaint
/// when the pointer comes onto a row, all three stages for a menu
/// opened somewhere else, and no allocation in any of them.
#[test]
fn menu_surface_frame() {
    let Ok(device) = system_default_device() else {
        eprintln!("SKIPPED: no Metal device");
        return;
    };
    let mut font = FontCache::build().expect("font");
    let mut atlas = new_atlas(&device, 2048, 2048, false, font.font_table()).map(|(a, _)| a).expect("atlas");
    let (mut cells, mut glyphs, mut ui_rects) = (Vec::new(), Vec::new(), Vec::new());
    let window = (1600.0, 1000.0);
    let mut m = MenuRender::new((300.0, 200.0), 0.0, items());
    let mut frame = |m: &MenuRender| {
        let (atlas_w, atlas_h) = atlas.dims();
        let mut p = ViewPainter {
            cell_w: font.cell_w as f32,
            cell_h: font.cell_h as f32,
            ascent: font.ascent as f32,
            atlas_w: atlas_w as f32,
            atlas_h: atlas_h as f32,
            window_w: window.0,
            window_h: window.1,
            font: &mut font,
            atlas: &mut atlas,
            cells: &mut cells,
            glyphs: &mut glyphs,
            ui_rects: &mut ui_rects,
        };
        m.surface.borrow_mut().refresh(&mut p, m);
        let s = m.surface.borrow();
        let (c, g, u) = s.kept.kept();
        (c.len(), g.len(), u.len(), s.painted_rev())
    };
    let counters = |m: &MenuRender| m.surface.borrow().kept.surface.counters();
    let row = |m: &MenuRender, i: usize| {
        let r = ContextMenu::layout(window.0, window.1, 1.0, m.anchor_phys.0, m.anchor_phys.1, 0.0, &m.items).item_rects[i];
        (r.x + r.w * 0.5, r.y_top + r.h * 0.5)
    };
    at_scale(1.0, || {
        // both places twice, and the pointer on a row once: every
        // buffer has grown to what any of these frames needs
        for x in [300.0, 500.0, 300.0, 500.0, 300.0] {
            m.anchor_phys.0 = x;
            frame(&m);
        }
        let on = row(&m, 1);
        m.surface.borrow_mut().pointer_move(on.0, on.1);
        frame(&m);
        m.surface.borrow_mut().kept.surface.pointer_leave();
        frame(&m);
        let warm = counters(&m);

        let (n, drawn) = counted(|| frame(&m));
        assert_eq!(counters(&m), warm, "a frame with nothing new ran a stage");
        assert_eq!(n, 0, "a frame with nothing new allocated {n} times");
        assert!(drawn.1 > 10 && drawn.2 == 2, "the menu has to have been painted: {drawn:?}");

        m.surface.borrow_mut().pointer_move(on.0, on.1);
        let (n, lit) = counted(|| frame(&m));
        let c = counters(&m);
        assert_eq!((c.build, c.layout, c.paint), (warm.build, warm.layout, warm.paint + 1), "the pointer on a row is a repaint");
        assert_eq!(n, 0, "the pointer coming onto a row allocated {n} times");
        assert_eq!((lit.2, lit.3), (drawn.2 + 1, drawn.3 + 1), "the row's band is one more rectangle in a new picture");

        m.anchor_phys.0 = 500.0;
        let (n, moved) = counted(|| frame(&m));
        let d = counters(&m);
        assert_eq!((d.build, d.layout, d.paint), (c.build + 1, c.layout + 1, c.paint + 1), "a menu opened elsewhere is all three stages");
        assert_eq!(n, 0, "a menu opened elsewhere allocated {n} times");
        assert_eq!(moved.3, lit.3 + 1);

        // the counter counts: one allocation on purpose is seen
        let (n, _) = counted(|| std::hint::black_box(vec![0u8; 64]));
        assert!(n >= 1, "an allocation made on purpose was not counted");
    });
}

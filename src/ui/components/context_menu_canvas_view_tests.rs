//! Every context menu the app opens, drawn from its canvas and from
//! its tree, held to the same bytes.

use std::cell::RefCell;

use crate::commands::{Command, hint};
use crate::paint_parity_tests::{first_difference_in, render_with};
use crate::ui::components::context_menu::{ContextMenu, MenuItem};
use crate::ui::components::context_menu_paint::{ContextMenuRender, ContextMenuRow};
use crate::ui::components::context_menu_view::MenuRender;
use crate::ui::components::density_kit::at_scale;
use crate::ui::core::ViewPainter;
use crate::ui::strings::{self, Arg, Msg, t};
use marspot_term::language::LangId;

thread_local! {
    static SCENE: RefCell<Option<MenuRender>> = const { RefCell::new(None) };
}

fn paint_scene(p: &mut ViewPainter<'_>) {
    SCENE.with(|s| {
        let s = s.borrow();
        let m = s.as_ref().expect("a scene");
        m.surface.borrow_mut().frame(p, m);
    });
}

fn entry(msg: Msg) -> MenuItem {
    MenuItem::entry(t(msg), 0)
}

/// The menus the app has, by the rows it gives them.
fn menus() -> Vec<(&'static str, Vec<MenuItem>)> {
    let move_band = |others: usize, pointless: bool| {
        let mut rows: Vec<MenuItem> =
            (0..others).map(|n| MenuItem::entry(&strings::text(Arg::MoveToWindow(n + 2)), 0)).collect();
        let new = entry(Msg::MenuMoveToNewWindow);
        rows.push(if pointless { new.disabled() } else { new });
        rows
    };
    let pane = |selection: bool, others: usize, pointless: bool| {
        let copy = entry(Msg::MenuCopy).with_shortcut(&hint(Command::Copy));
        let mut rows = vec![
            if selection { copy } else { copy.disabled() },
            entry(Msg::MenuPaste).with_shortcut(&hint(Command::Paste)),
            MenuItem::divider(),
            entry(Msg::MenuClearScrollback),
            MenuItem::divider(),
            entry(Msg::MenuNewPane),
            entry(Msg::MenuClosePane),
            MenuItem::divider(),
        ];
        rows.extend(move_band(others, pointless));
        rows
    };
    let mut sidebar = vec![MenuItem::divider(), entry(Msg::MenuClosePane), MenuItem::divider()];
    sidebar.extend(move_band(1, false));
    let mut badge: Vec<MenuItem> = (1u8..=3)
        .map(|n| MenuItem::entry(&strings::text(Arg::ProfileRow { n, current: n == 2 }), 0))
        .collect();
    badge.push(MenuItem::entry(&strings::text(Arg::HandOffTo("codex 2")), 0));
    badge.push(MenuItem::entry(&strings::text(Arg::SwitchToProfile(4)), 0));
    vec![
        ("pane", pane(true, 0, false)),
        ("pane, nothing selected, the only pane", pane(false, 0, true)),
        ("pane, two other windows", pane(true, 2, false)),
        ("sidebar row", sidebar),
        (
            "title strip",
            vec![
                entry(Msg::MenuToggleSidebar).with_shortcut(&hint(Command::ToggleSidebar)),
                entry(Msg::MenuOpenLayout),
            ],
        ),
        ("link", vec![entry(Msg::LinkCopyUrl), entry(Msg::LinkOpenUrl)]),
        ("file link", vec![entry(Msg::LinkCopyPath), entry(Msg::LinkOpenFile), entry(Msg::LinkRevealFile)]),
        ("mail link", vec![entry(Msg::LinkCopyEmail), entry(Msg::LinkSendEmail)]),
        ("address link", vec![entry(Msg::LinkCopyIp), entry(Msg::LinkOpenInBrowser)]),
        ("identifier", vec![entry(Msg::LinkCopyUuid)]),
        ("badge", badge),
    ]
}

struct Scene {
    name: String,
    items: Vec<MenuItem>,
    /// In window units from the window's corner.
    anchor: (f64, f64),
    hovered: Option<usize>,
}

const WINDOW: (f64, f64) = (900.0, 560.0);

fn scenes() -> Vec<Scene> {
    let mut out = Vec::new();
    for (name, items) in menus() {
        let rows: Vec<usize> = (0..items.len()).collect();
        // every row under the pointer in turn, dividers and disabled
        // rows too, where nothing lights
        for hovered in std::iter::once(None).chain(rows.iter().copied().map(Some)) {
            out.push(Scene { name: format!("{name}, pointer on {hovered:?}"), items: items.clone(), anchor: (300.0, 120.0), hovered });
        }
        // against the corner it flips away from, against the top it
        // may not cover, and at half a pixel
        for anchor in [(WINDOW.0 - 8.0, WINDOW.1 - 8.0), (4.0, 2.0), (211.5, 97.5)] {
            let last = items.iter().rposition(|i| !i.divider && i.enabled);
            for hovered in [None, last] {
                out.push(Scene { name: format!("{name} at {anchor:?}, pointer on {hovered:?}"), items: items.clone(), anchor, hovered });
            }
        }
    }
    out
}

/// The window in device pixels and the scene's anchor in them.
fn placed(scene: &Scene, scale: f64) -> ((u32, u32), (f64, f64)) {
    (((WINDOW.0 * scale) as u32, (WINDOW.1 * scale) as u32), (scene.anchor.0 * scale, scene.anchor.1 * scale))
}

fn from_canvas(scene: &Scene, scale: f64) -> Option<Vec<u8>> {
    let ((w, h), anchor) = placed(scene, scale);
    render_with(None, w, h, |renderer, layout| {
        renderer.set_context_menu(Some(ContextMenuRender {
            scale,
            anchor_phys: anchor,
            top_inset: layout.top_inset,
            items: scene
                .items
                .iter()
                .map(|it| ContextMenuRow {
                    label: it.label.clone(),
                    shortcut_hint: it.shortcut_hint.clone(),
                    enabled: it.enabled,
                    divider: it.divider,
                })
                .collect(),
            // the canvas lights what it is told to; the app tells it
            // only of rows that act
            hovered_idx: scene.hovered.filter(|&i| !scene.items[i].divider && scene.items[i].enabled),
        }));
    })
}

fn from_tree(scene: &Scene, scale: f64, nudge: f64) -> Option<Vec<u8>> {
    let ((w, h), anchor) = placed(scene, scale);
    let out = render_with(Some(paint_scene), w, h, |_, layout| {
        let mut m = MenuRender::new((anchor.0 + nudge, anchor.1), layout.top_inset, scene.items.clone());
        m.pointer = scene.hovered.map(|i| {
            let menu = ContextMenu::layout(w as f64, h as f64, scale, anchor.0 + nudge, anchor.1, layout.top_inset, &scene.items);
            let r = menu.item_rects[i];
            (r.x + r.w * 0.5, r.y_top + r.h * 0.5)
        });
        SCENE.with(|s| *s.borrow_mut() = Some(m));
    });
    SCENE.with(|s| *s.borrow_mut() = None);
    out
}

fn in_language(lang: LangId) {
    crate::settings::set_for_test(crate::settings::Settings {
        appearance_language: Some(lang),
        ..Default::default()
    });
    strings::follow_language();
}

fn every_menu_is_the_same(lang: LangId, scale: f64) {
    in_language(lang);
    at_scale(scale, || {
        let mut compared = 0;
        for scene in scenes() {
            let (Some(a), Some(b)) = (from_canvas(&scene, scale), from_tree(&scene, scale, 0.0)) else { return };
            let w = placed(&scene, scale).0.0;
            if let Some(((x, y), n)) = first_difference_in(&a, &b, w) {
                panic!("{}: {n} pixels differ, the first at ({x}, {y})", scene.name);
            }
            compared += 1;
        }
        eprintln!("{compared} menu scenes, every byte the same");
        assert!(compared > 100, "only {compared} scenes were compared");
    });
}

#[test]
fn every_menu_from_its_tree_is_its_canvas_en_1() {
    every_menu_is_the_same(LangId::En, 1.0);
}

#[test]
fn every_menu_from_its_tree_is_its_canvas_en_2() {
    every_menu_is_the_same(LangId::En, 2.0);
}

#[test]
fn every_menu_from_its_tree_is_its_canvas_ja_1() {
    every_menu_is_the_same(LangId::Ja, 1.0);
}

#[test]
fn every_menu_from_its_tree_is_its_canvas_ja_2() {
    every_menu_is_the_same(LangId::Ja, 2.0);
}

#[test]
fn every_menu_from_its_tree_is_its_canvas_zh_1() {
    every_menu_is_the_same(LangId::ZhHans, 1.0);
}

#[test]
fn every_menu_from_its_tree_is_its_canvas_zh_2() {
    every_menu_is_the_same(LangId::ZhHans, 2.0);
}

/// The comparison has to be able to fail: a menu one pixel to the
/// right, and a menu with the pointer on another row, are different
/// pictures.
#[test]
fn a_menu_one_pixel_off_or_lit_elsewhere_is_noticed() {
    in_language(LangId::En);
    at_scale(1.0, || {
        let all = scenes();
        let scene = all.iter().find(|s| s.name == "pane, pointer on Some(1)").expect("the scene");
        let w = placed(scene, 1.0).0.0;
        let (Some(a), Some(b)) = (from_canvas(scene, 1.0), from_tree(scene, 1.0, 1.0)) else { return };
        let (_, n) = first_difference_in(&a, &b, w).expect("a menu moved a pixel drew the same bytes");
        assert!(n > 200, "only {n} pixels differ for a menu moved a pixel");

        let other = Scene { name: String::new(), items: scene.items.clone(), anchor: scene.anchor, hovered: Some(3) };
        let c = from_tree(&other, 1.0, 0.0).expect("a frame");
        let (_, n) = first_difference_in(&a, &c, w).expect("the pointer on another row drew the same bytes");
        assert!(n > 200, "only {n} pixels differ for another row lit");
    });
}

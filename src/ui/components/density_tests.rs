//! Every scene the snapshot tests draw, drawn at display scale 1 and at
//! scale 2 and held to each other: see `density_kit` for what is
//! compared and how closely.
//!
//! One test per scene, named for it.  A failure names the first thing
//! in the frame that is not where it belongs — the layer it is drawn
//! in, its place in the window, and its box at both scales.
//!
//! The number after each scene is how many borders and lines in it are
//! one physical pixel thick at both scales.  Those are meant
//! (`ui::theme::spec` lists them); the count is held so that another
//! one is a decision.  A scene that gains a card or a row gains its
//! hairlines too, and its number is changed with it.

use super::chrome_snapshot_tests as chrome;
use super::density_kit::{capture, compare, Capture};
use super::panel_snapshot_tests as panel;
use crate::render_metal::{MetalRenderer, WindowRender};
use crate::snapshot_kit::{in_language, in_theme};
use crate::ui::components::chrome_hover::ChromeHover;
use crate::ui::theme::spec;
use marspot_term::language::LangId;
use marspot_term::layout::Rect;
use marspot_term::palette::ThemeId;

/// Both frames of `scene`, or `None` where there is no Metal device.
fn both(scene: fn()) -> Option<(Capture, Capture)> {
    let Some(one) = capture(1.0, scene) else {
        eprintln!("SKIPPED: no Metal device");
        return None;
    };
    let two = capture(2.0, scene).expect("a frame at scale 2");
    assert!(one.elems.len() > 20, "{}: a frame of {} elements is not the scene", one.scene, one.elems.len());
    Some((one, two))
}

fn hold(scene: fn(), hairlines: usize) {
    let Some((one, two)) = both(scene) else { return };
    let report = compare(&one, &two, 2.0);
    if let Err(first) = report.first() {
        panic!("{first}\n({} in all)", report.findings.len());
    }
    assert_eq!(
        report.hairlines, hairlines,
        "{}: {} borders and lines are one pixel thick at both scales and the table says {hairlines}. \
         One that is meant to be the thinnest line the display draws belongs in the table in \
         `ui::theme::spec` and in this count; any other is a size that was not scaled",
        one.scene, report.hairlines,
    );
}

macro_rules! scenes_hold_at_both_scales {
    ($($test:ident = $scene:expr, $hairlines:expr;)*) => {$(
        #[test]
        fn $test() {
            hold($scene, $hairlines);
        }
    )*};
}

fn settings_ja() {
    in_language(LangId::Ja, panel::panel_snapshot_settings);
}

fn controls_zh() {
    in_language(LangId::ZhHans, panel::panel_snapshot_controls);
}

fn window_light() {
    in_theme(ThemeId::Light, chrome::chrome_snapshot_window);
}

scenes_hold_at_both_scales! {
    density_settings = panel::panel_snapshot_settings, 10;
    density_settings_ja = settings_ja, 10;
    density_shortcuts = panel::panel_snapshot_shortcuts, 21;
    density_layout = panel::panel_snapshot_layout, 7;
    density_layout_grid = panel::panel_snapshot_layout_grid, 13;
    density_process = panel::panel_snapshot_process, 5;
    density_accounts = panel::panel_snapshot_accounts, 927;
    density_accounts_narrow = panel::panel_snapshot_accounts_narrow, 927;
    density_accounts_empty = panel::panel_snapshot_accounts_empty, 1;
    density_controls = panel::panel_snapshot_controls, 5;
    density_controls_zh = controls_zh, 5;
    density_chrome_window = chrome::chrome_snapshot_window, 0;
    density_chrome_window_light = window_light, 0;
    density_chrome_search = chrome::chrome_snapshot_search, 2;
    density_chrome_menu = chrome::chrome_snapshot_menu, 2;
    density_chrome_close = chrome::chrome_snapshot_close, 0;
    density_chrome_terminal = chrome::chrome_snapshot_terminal, 0;
}

/// A sheet with one thing wrong in it: a card placed in points and
/// sized in pixels.
fn unscaled_size(p: &mut crate::ui::core::ViewPainter<'_>) {
    let at = Rect { x: spec::px(40.0), y_top: spec::px(40.0), w: spec::px(120.0), h: spec::px(60.0) };
    super::panel_chrome::card(p, at);
    super::panel_chrome::card(p, Rect { y_top: spec::px(120.0), w: 240.0, h: 120.0, ..at });
}

/// The same sheet with a radius left in pixels.
fn unscaled_radius(p: &mut crate::ui::core::ViewPainter<'_>) {
    let at = Rect { x: spec::px(40.0), y_top: spec::px(40.0), w: spec::px(120.0), h: spec::px(60.0) };
    p.fill_rounded_rect(at, super::panel_palette::accent(), 6.0, (super::panel_palette::none(), 0.0));
}

fn sheet(paint: fn(&mut crate::ui::core::ViewPainter<'_>)) -> Option<(Capture, Capture)> {
    static SHEET: std::sync::Mutex<Option<fn(&mut crate::ui::core::ViewPainter<'_>)>> = std::sync::Mutex::new(None);
    fn scene() {
        let paint = SHEET.lock().unwrap().expect("a sheet to draw");
        panel::snapshot("density_control", 600, 400, |_, _| {
            *crate::frame_build::frame::TEST_OVERLAY.lock().unwrap() = Some(paint);
        });
    }
    *SHEET.lock().unwrap() = Some(paint);
    both_small(scene)
}

/// `both`, for a sheet of a few rectangles.
fn both_small(scene: fn()) -> Option<(Capture, Capture)> {
    let Some(one) = capture(1.0, scene) else {
        eprintln!("SKIPPED: no Metal device");
        return None;
    };
    Some((one, capture(2.0, scene).expect("a frame at scale 2")))
}

/// The comparison has to be able to fail: a rectangle that keeps its
/// pixel size is found and named.
#[test]
fn density_check_catches_a_size_left_in_pixels() {
    let Some((one, two)) = sheet(unscaled_size) else { return };
    let report = compare(&one, &two, 2.0);
    let first = report.first().expect_err("a card of 240 x 120 px at both scales passed");
    assert!(first.contains("is not scaled") && first.contains("overlay rounded rects"), "{first}");
    assert!(first.contains("w 240.0 h 120.0"), "it names the card's box: {first}");
    // and only that one: the card beside it, sized in points, is fine
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
}

#[test]
fn density_check_catches_a_radius_left_in_pixels() {
    let Some((one, two)) = sheet(unscaled_radius) else { return };
    let first = compare(&one, &two, 2.0).first().expect_err("a radius of 6 px at both scales passed");
    assert!(first.contains("the radius of"), "{first}");
}

/// A window dragged to a display of another density and back: nothing
/// drawn after the move may come from before it.  One renderer and one
/// window's caches go through scale 1, 2 and 1 again; the frame at 2
/// must be the frame a renderer that only ever knew scale 2 draws, and
/// the frame back at 1 must be the first one.
#[test]
fn a_window_moved_between_densities_draws_as_one_made_there() {
    let Ok(mut renderer) = MetalRenderer::new_headless() else {
        eprintln!("SKIPPED: no Metal device");
        return;
    };
    let mut wr = WindowRender::new();
    let mut moved_to = |scale: f64| {
        capture(scale, || {
            renderer.rebuild_fonts_if_scale_changed();
            chrome::window_on(&mut renderer, &mut wr, Some(ChromeHover::Row(3)));
        })
        .expect("a frame")
    };
    let (first, at_two, back) = (moved_to(1.0), moved_to(2.0), moved_to(1.0));
    let fresh_at_two = capture(2.0, chrome::chrome_snapshot_window).expect("a frame");
    same_frame(&at_two, &fresh_at_two, "after a move from scale 1 to 2");
    same_frame(&back, &first, "after a move to scale 2 and back");
}

/// The same for a panel, whose text is measured and shaped through
/// caches the terminal's is not.
#[test]
fn a_panel_moved_between_densities_draws_as_one_made_there() {
    let Ok(mut renderer) = MetalRenderer::new_headless() else {
        eprintln!("SKIPPED: no Metal device");
        return;
    };
    let mut wr = WindowRender::new();
    let mut moved_to = |scale: f64| {
        capture(scale, || {
            renderer.rebuild_fonts_if_scale_changed();
            panel::snapshot_on(&mut renderer, &mut wr, "settings", 1000, 1360, |renderer, layout| {
                panel::open_settings(renderer, layout, None);
            });
        })
        .expect("a frame")
    };
    let (first, at_two, back) = (moved_to(1.0), moved_to(2.0), moved_to(1.0));
    let fresh_at_two = capture(2.0, panel::panel_snapshot_settings).expect("a frame");
    same_frame(&at_two, &fresh_at_two, "after a move from scale 1 to 2");
    same_frame(&back, &first, "after a move to scale 2 and back");
}

/// A panel left dragged and scrolled, and a menu left open, while the
/// window changes display: what says where they are is kept in window
/// units, so the frame at scale 2 is the one a window that was dragged
/// and scrolled there draws, and the frame back at scale 1 is the
/// first.
#[test]
fn a_panel_dragged_and_scrolled_keeps_its_place_between_densities() {
    use super::density_kit::at_scale;
    use super::panel_chrome::PanelChrome;
    use super::settings_view::SettingsRender;
    use super::surface_stack::{self, Press, SurfaceKind, SurfaceStack};
    use crate::ui::Units;

    // The state a window holds: set by a drag of 60 x 40 window units
    // and a scroll of 30, on a display of scale `k`.
    struct Held {
        stack: SurfaceStack,
        scroll: Units,
        menu: (Units, Units),
    }
    let held_at = |k: f64| {
        at_scale(k, || {
            let mut stack = SurfaceStack::default();
            stack.show(SurfaceKind::Settings);
            let at = Rect { x: 100.0 * k, y_top: 100.0 * k, w: 400.0 * k, h: 300.0 * k };
            let press = stack.on_press(150.0 * k, 104.0 * k, &PanelChrome::new(at, 0.0));
            assert_eq!(press, Some(Press::BeginDrag));
            assert!(stack.on_drag(210.0 * k, 144.0 * k, 4000.0 * k, 4000.0 * k, 0.0));
            stack.on_release();
            let menu = (Units::from_px(300.0 * k), Units::from_px(200.0 * k));
            Held { stack, scroll: Units::from_px(30.0 * k), menu }
        })
    };
    let draw = |renderer: &mut MetalRenderer, wr: &mut WindowRender, held: &Held| {
        renderer.rebuild_fonts_if_scale_changed();
        // too short to show the panel whole, so there is somewhere to scroll
        panel::snapshot_on(renderer, wr, "settings_moved", 1000, 520, |renderer, layout| {
            let sp = panel::open_settings(renderer, layout, None);
            let rect =
                surface_stack::place(sp.rect, held.stack.offset(), layout.window_w, layout.window_h, layout.top_inset);
            renderer.set_settings_panel(Some(SettingsRender { rect, scroll: held.scroll.px(), ..sp }));
        });
    };
    let Ok(mut renderer) = MetalRenderer::new_headless() else {
        eprintln!("SKIPPED: no Metal device");
        return;
    };
    let mut wr = WindowRender::new();
    let held = held_at(1.0);
    let mut moved_to = |scale: f64| capture(scale, || draw(&mut renderer, &mut wr, &held)).expect("a frame");
    let (first, at_two, back) = (moved_to(1.0), moved_to(2.0), moved_to(1.0));

    let there = held_at(2.0);
    let mut fresh = (MetalRenderer::new_headless().expect("a second renderer"), WindowRender::new());
    let fresh_at_two = capture(2.0, || draw(&mut fresh.0, &mut fresh.1, &there)).expect("a frame");
    same_frame(&at_two, &fresh_at_two, "after a move from scale 1 to 2");
    same_frame(&back, &first, "after a move to scale 2 and back");

    // The check can fail: the same panel left where it opens is another frame.
    let at_rest = Held { stack: SurfaceStack::default(), scroll: Units::default(), menu: held.menu };
    let plain = capture(1.0, || draw(&mut fresh.0, &mut fresh.1, &at_rest)).expect("a frame");
    let moved = plain.elems.iter().zip(&first.elems).filter(|(a, b)| a.edges != b.edges).count();
    assert!(
        moved > 10 || plain.elems.len() != first.elems.len(),
        "a drag and a scroll moved only {moved} elements of the frame"
    );

    // The menu's anchor: the same point of the window on both displays.
    let anchor = |k: f64, m: &(Units, Units)| at_scale(k, || (m.0.px(), m.1.px()));
    assert_eq!(anchor(1.0, &held.menu), (300.0, 200.0));
    assert_eq!(anchor(2.0, &held.menu), (600.0, 400.0));
    assert_eq!(anchor(2.0, &held.menu), anchor(2.0, &there.menu));
    assert_eq!(anchor(1.0, &held.menu), (300.0, 200.0));
}

/// Two frames that must be the same frame, element for element.
fn same_frame(got: &Capture, want: &Capture, when: &str) {
    assert_eq!(got.elems.len(), want.elems.len(), "{}: {when}, the frame holds another number of elements", want.scene);
    for (g, w) in got.elems.iter().zip(&want.elems) {
        let off = (0..4).map(|i| (g.edges[i] - w.edges[i]).abs()).fold(0.0, f64::max);
        assert!(
            off < 0.01 && g.attrs == w.attrs && g.layer == w.layer,
            "{}: {when}, {} #{} is at {:?} and a window made at this scale has it at {:?}",
            want.scene, w.layer, w.n, g.edges, w.edges,
        );
    }
}

/// The same scenes as pictures at scale 2, for eyes: `<scene>.2x.png`
/// beside the scale-1 baselines, recorded and compared the same way.
macro_rules! pictures_at_scale_2 {
    ($($test:ident = $scene:path;)*) => {$(
        #[test]
        fn $test() {
            super::density_kit::at_scale(2.0, $scene);
        }
    )*};
}

pictures_at_scale_2! {
    chrome_snapshot_window_2x = chrome::chrome_snapshot_window;
    chrome_snapshot_search_2x = chrome::chrome_snapshot_search;
    chrome_snapshot_menu_2x = chrome::chrome_snapshot_menu;
    panel_snapshot_settings_2x = panel::panel_snapshot_settings;
    panel_snapshot_shortcuts_2x = panel::panel_snapshot_shortcuts;
    panel_snapshot_layout_2x = panel::panel_snapshot_layout;
    panel_snapshot_process_2x = panel::panel_snapshot_process;
    panel_snapshot_accounts_2x = panel::panel_snapshot_accounts;
    panel_snapshot_controls_2x = panel::panel_snapshot_controls;
}

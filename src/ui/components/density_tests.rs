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
use crate::ui::components::settings_modal;
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
                panel::open_settings(renderer, layout, settings_modal::Hover::None);
            });
        })
        .expect("a frame")
    };
    let (first, at_two, back) = (moved_to(1.0), moved_to(2.0), moved_to(1.0));
    let fresh_at_two = capture(2.0, panel::panel_snapshot_settings).expect("a frame");
    same_frame(&at_two, &fresh_at_two, "after a move from scale 1 to 2");
    same_frame(&back, &first, "after a move to scale 2 and back");
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

/// The developer panel is a canvas in a window of its own, drawn in
/// the shell: every rectangle, line and text origin of its first page
/// at scale 2 is where it is at scale 1, doubled.
#[test]
fn the_developer_panels_canvas_doubles() {
    use crate::ui::components::{build_dev_panel_canvas, DevPanelState};
    use crate::ui::core::Primitive;
    let page = |scale: f64| {
        super::density_kit::at_scale(scale, || {
            let state = DevPanelState { scale, size_pt: (900.0, 700.0), origin_pt: (0.0, 0.0), ..Default::default() };
            let fonts = crate::ui::view::MockFontMetrics { cell_w_phys: 8.0 * scale, cell_h_phys: 16.0 * scale };
            let (cw, ch, ascent) = ((8.0 * scale) as f32, (16.0 * scale) as f32, (12.0 * scale) as f32);
            build_dev_panel_canvas(&state, 900.0 * scale, 700.0 * scale, cw, ch, ascent, &fonts).primitives().to_vec()
        })
    };
    let (one, two) = (page(1.0), page(2.0));
    assert_eq!(one.len(), two.len(), "the page holds another number of things at scale 2");
    assert!(one.len() > 20);
    let boxes = |p: &Primitive| match p {
        Primitive::Rect(r) => vec![r.x, r.y, r.w, r.h, r.radius, r.border.map_or(0.0, |b| b.0)],
        Primitive::Line(l) => vec![l.from.0, l.from.1, l.to.0, l.to.1, l.width],
        Primitive::Text(t) => vec![t.x, t.y],
    };
    for (n, (a, b)) in one.iter().zip(&two).enumerate() {
        for (va, vb) in boxes(a).iter().zip(boxes(b)) {
            assert!((vb - 2.0 * va).abs() <= 1.0, "primitive {n} is not scaled: {a:?} at scale 1, {b:?} at scale 2");
        }
    }
}

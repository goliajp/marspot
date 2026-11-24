//! The helper panel: what is pressed where, at both densities, and its
//! pictures.

use std::rc::Rc;

use super::chrome_snapshot_tests as chrome;
use super::controls::font_measure;
use super::density_kit::{at_scale, capture, compare};
use super::helpers_view::{self as hv, Ask, HelperGroup, HelperRow, HelpersRender, HelpersSurface, asked};
use super::panel_snapshot_tests::snapshot;
use crate::font_cache::FontCache;
use crate::render_metal::{MetalRenderer, WindowRender};
use crate::ui::theme::spec;
use crate::ui::tree::panel;
use golia_ui_core::Action;

const NOW: u64 = 1_795_000_000;

fn row(sid: u64, parent: u64, name: &str, doing: &str, minutes: Option<u64>) -> HelperRow {
    let started_unix = minutes.map(|m| NOW - m * 60);
    HelperRow { sid, parent, name: name.into(), doing: doing.into(), started_unix }
}

pub(super) fn groups() -> Vec<HelperGroup> {
    vec![
        HelperGroup {
            parent: 1,
            name: "spg#3381".into(),
            control: hv::Control::Allowed,
            rows: vec![
                row(2, 1, "spg#7c20", "codex gpt-6 · review the parser for lines that are longer than the window is wide, and report each one with its file and its number", Some(4)),
                row(3, 1, "spg#91aa", "claude · run the suite on the runner", Some(135)),
                row(4, 3, "devops#0b1f", "/Users/you/workspace/devops", None),
            ],
        },
        HelperGroup { parent: 8, name: "alt#1d0e".into(), control: hv::Control::Denied, rows: vec![row(9, 8, "alt#44f2", "gemini · 翻译发布说明", Some(0))] },
    ]
}

fn font() -> FontCache {
    FontCache::build().expect("font")
}

fn max_scroll(hp: &HelpersRender) -> f64 {
    hv::max_scroll(hp.rect, &hp.groups, &mut font_measure(&mut font()))
}

fn render(window: (f64, f64), groups: Vec<HelperGroup>, scroll: f64) -> HelpersRender {
    HelpersRender {
        rect: hv::panel_rect(window.0, window.1, &groups, 0.0, &mut font_measure(&mut font())),
        scroll,
        groups: Rc::new(groups),
        now_unix: NOW,
        pointer: None,
        surface: Default::default(),
    }
}

/// Every action of the panel found by pressing all over it, each as
/// what it asks.
fn pressed_all_over(hp: &HelpersRender) -> Vec<Ask> {
    let mut font = font();
    let mut s = HelpersSurface::default();
    s.lay_out(&mut font, hp, 0.0);
    let step = spec::px(4.0);
    let mut found: Vec<Action> = Vec::new();
    let mut y = hp.rect.y_top;
    while y < hp.rect.y_top + hp.rect.h {
        let mut x = hp.rect.x;
        while x < hp.rect.x + hp.rect.w {
            s.kept.surface.pointer_down((x, y));
            if let Some(a) = s.kept.surface.pointer_up((x, y))
                && !found.contains(&a)
            {
                found.push(a);
            }
            x += step;
        }
        y += step;
    }
    assert!(found.contains(&panel::CLOSE), "the close button");
    found.into_iter().map(asked).filter(|a| *a != Ask::None).collect()
}

#[test]
fn helper_panel_every_button_is_pressed_where_it_is_at_both_densities() {
    for scale in [1.0, 2.0] {
        at_scale(scale, || {
            let mut got = pressed_all_over(&render((1600.0 * scale, 1000.0 * scale), groups(), 0.0));
            got.sort_by_key(|a| format!("{a:?}"));
            let mut want: Vec<Ask> = (0..4).flat_map(|i| [Ask::View(i), Ask::Move(i), Ask::Close(i)]).collect();
            want.extend((0..2).flat_map(|g| [Ask::CloseAll(g), Ask::Control(g)]));
            want.sort_by_key(|a| format!("{a:?}"));
            assert_eq!(got, want, "at scale {scale}");
        });
    }
}

#[test]
fn helper_panel_has_no_control_button_for_a_directory_another_line_denies() {
    for (scale, w) in [(1.0, 1600.0), (1.0, 640.0), (2.0, 1600.0)] {
        at_scale(scale, || {
            let mut gs = groups();
            let whole = render((w * scale, 1600.0 * scale), gs.clone(), 0.0).rect.h;
            gs[1].control = hv::Control::DeniedAbove;
            let hp = render((w * scale, 1600.0 * scale), gs, 0.0);
            assert_eq!(hp.rect.h < whole, w < 700.0, "one button less to stack");
            let got = pressed_all_over(&hp);
            assert!(got.contains(&Ask::Control(0)) && got.contains(&Ask::CloseAll(1)), "{got:?}");
            assert!(!got.contains(&Ask::Control(1)), "{got:?}");
        });
    }
}

#[test]
fn helper_panel_keeps_its_top_edge() {
    for scale in [1.0, 2.0] {
        at_scale(scale, || {
            let (w, h) = (1600.0 * scale, 1000.0 * scale);
            let rect = |g: &[HelperGroup]| hv::panel_rect(w, h, g, 0.0, &mut font_measure(&mut font()));
            let (tall, short) = (rect(&groups()), rect(&groups()[1..]));
            assert!(short.y_top > tall.y_top);
            let kept = hv::kept_at(short, tall.y_top, h, 0.0);
            assert_eq!((kept.y_top, kept.h), (tall.y_top, short.h));
            // grown again from low down: still in the window
            let grown = hv::kept_at(tall, h - short.h, h, 0.0);
            assert!(grown.y_top + grown.h <= h && grown.y_top < h - short.h);
        });
    }
}

/// Too narrow for the two buttons side by side: both are still there.
#[test]
fn helper_panel_stacks_its_buttons_in_a_narrow_window() {
    at_scale(1.0, || {
        let got = pressed_all_over(&render((640.0, 1600.0), groups(), 0.0));
        for want in [Ask::CloseAll(0), Ask::Control(0), Ask::CloseAll(1), Ask::Control(1), Ask::View(3), Ask::Move(3), Ask::Close(3)] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
    });
}

#[test]
fn helper_panel_is_as_tall_as_its_rows_until_the_window_is_not() {
    at_scale(1.0, || {
        let whole = render((1600.0, 1400.0), groups(), 0.0);
        assert_eq!(max_scroll(&whole), 0.0);
        let short = render((1600.0, 420.0), groups(), 0.0);
        let max = max_scroll(&short);
        assert!(max > 0.0);
        // scrolled to the end, the last group's buttons are there
        let end = render((1600.0, 420.0), groups(), max);
        let got = pressed_all_over(&end);
        assert!(got.contains(&Ask::CloseAll(1)) && got.contains(&Ask::Control(1)), "{got:?}");
        assert!(!got.contains(&Ask::Move(0)), "{got:?}");
    });
}

#[test]
fn helper_panel_describes_again_once_a_minute_and_not_between() {
    let mut font = font();
    let mut s = HelpersSurface::default();
    let mut hp = render((1600.0, 1000.0), groups(), 0.0);
    let mut builds = |hp: &HelpersRender| {
        s.lay_out(&mut font, hp, 0.0);
        s.kept.surface.counters().build
    };
    let first = builds(&hp);
    hp.now_unix = NOW / 60 * 60 + 59;
    assert_eq!(builds(&hp), first);
    hp.now_unix += 1;
    assert_eq!(builds(&hp), first + 1);
}

#[test]
fn panel_snapshot_helpers() {
    snapshot("helpers", 1600, 1100, |renderer, layout| {
        let hp = render((layout.window_w, layout.window_h), groups(), 0.0);
        // on the first row's close button
        let chrome = super::panel_chrome::PanelChrome::new(hp.rect, 0.0);
        let x = chrome.content.x + chrome.content.w - spec::px(spec::card::ROW_PAD_X + 10.0);
        let y = chrome.content.y_top + super::panel_chrome::heading_height() + spec::px(hv::metric::ROW_H * 0.5);
        renderer.set_helpers_panel(Some(HelpersRender { pointer: Some((x, y)), ..hp }));
    });
}

#[test]
fn panel_snapshot_helpers_empty() {
    snapshot("helpers_empty", 1600, 500, |renderer, layout| {
        renderer.set_helpers_panel(Some(render((layout.window_w, layout.window_h), Vec::new(), 0.0)));
    });
}

/// The `+N` after two panes' titles and the sidebar's count.
#[test]
fn panel_snapshot_helpers_chrome() {
    chrome::window_with_helpers(3, &[2, 0, 0, 1]);
}

crate::snapshot_kit::snapshots_in_other_themes! {
    panel_snapshot_helpers_light, panel_snapshot_helpers_contrast = panel_snapshot_helpers;
}

#[test]
fn panel_snapshot_helpers_2x() {
    at_scale(2.0, panel_snapshot_helpers);
}

#[test]
fn panel_snapshot_helpers_chrome_2x() {
    at_scale(2.0, panel_snapshot_helpers_chrome);
}

/// The same frame at scale 1 and 2: everything twice as far out and
/// twice as large.
fn holds_at_both(scene: fn(), what: &str) {
    let Some(one) = capture(1.0, scene) else {
        eprintln!("SKIPPED: no Metal device");
        return;
    };
    let two = capture(2.0, scene).expect("a frame at scale 2");
    assert!(one.elems.len() > 20, "{what}: a frame of {} elements", one.elems.len());
    if let Err(first) = compare(&one, &two, 2.0).first() {
        panic!("{what}: {first}");
    }
}

#[test]
fn panel_snapshot_helpers_hold_at_both_densities() {
    holds_at_both(panel_snapshot_helpers, "the panel");
    holds_at_both(panel_snapshot_helpers_chrome, "the chrome");
}

/// A press that comes before the next frame lands on the mark the last
/// frame drew, and the mark goes when the count does.
#[test]
fn helper_panel_mark_and_count_are_pressed_where_they_are_drawn() {
    use crate::ui::components::chrome_target::ChromeTarget;
    let Ok(mut renderer) = MetalRenderer::new_headless() else {
        eprintln!("SKIPPED: no Metal device");
        return;
    };
    let mut mark = None;
    let drawn = capture(1.0, || {
        let mut wr = WindowRender::new();
        let counts: (u32, &[u32]) = (3, &[2, 0, 0, 1]);
        chrome::helpers_on(&mut renderer, &mut wr, counts, |_, _, _| {});
        chrome::helpers_on(&mut renderer, &mut wr, counts, |r, wr, shape| {
            let c = shape.layout.cells[0];
            let y = c.y_top + 4.0;
            mark = (0..c.w as usize)
                .map(|dx| (c.x + dx as f64, y))
                .find(|p| wr.chrome_at(r.font_mut(), shape, *p) == Some(ChromeTarget::Helpers(0)));
            let bottom = (shape.layout.sidebar_w * 0.5, shape.layout.window_h - 4.0);
            assert_eq!(wr.chrome_at(r.font_mut(), shape, bottom), Some(ChromeTarget::AllHelpers));
            wr.set_helpers(0, [0, 0, 0, 0].into_iter());
            if let Some(p) = mark {
                assert_eq!(wr.chrome_at(r.font_mut(), shape, p), Some(ChromeTarget::Title(0)));
            }
            assert_eq!(wr.chrome_at(r.font_mut(), shape, bottom), None);
        });
    });
    assert!(drawn.is_some() && mark.is_some(), "the mark is somewhere on the first strip");
}

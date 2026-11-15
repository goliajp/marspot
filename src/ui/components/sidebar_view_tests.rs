//! The sidebar drawn from its tree against the sidebar painted by hand,
//! and what a frame of it costs.

use super::sidebar_view::{SidebarFrame, SidebarSurface};
use super::toolbar_view_tests::{Frame, Stage};
use crate::frame_alloc_tests::counted;
use crate::frame_build::chrome::paint_sidebar;
use crate::layout::Layout;
use crate::render::SidebarEntry;
use marspot_term::session::SessionState;
use crate::ui::components::chrome_hover::ChromeHover;
use crate::ui::components::density_kit::at_scale;

const NAMES: [&str; 9] = [
    "smix",
    "devops",
    "insight-mono-a-name-longer-than-any-row",
    "alt",
    "日本語の名前がとても長いペイン",
    "torajs-lane-biz",
    "",
    "混合 mixed 名前",
    "kevy",
];

const STATES: [SessionState; 3] = [SessionState::Active, SessionState::Idle, SessionState::Exited];

fn rows(n: usize) -> Vec<SidebarEntry<'static>> {
    (0..n).map(|i| SidebarEntry { label: NAMES[i % NAMES.len()], state: STATES[i % 3] }).collect()
}

fn rows_of(r: &[SidebarEntry<'static>]) -> Vec<SidebarEntry<'static>> {
    r.iter().map(|e| SidebarEntry { label: e.label, state: e.state }).collect()
}

fn layout(stage: &Stage, (w, h): (f64, f64), scale: f64, n: usize) -> Layout {
    Layout::build(
        w,
        h,
        crate::ui::SIDEBAR_W_LOGICAL * scale,
        crate::HEADER_PT * scale,
        crate::ui::CELL_TITLE_PT * scale,
        3,
        3,
        stage.font.cell_w,
        stage.font.cell_h,
    )
    .with_chrome(scale, n, None)
}

/// The pointer nowhere, on the add button, on a few rows and on their
/// close buttons, and on the toolbar.
fn hovers(n: usize) -> Vec<Option<ChromeHover>> {
    let mut out = vec![None, Some(ChromeHover::Add), Some(ChromeHover::Toolbar(0))];
    for i in [0, 1, n / 2, n.saturating_sub(1), n] {
        out.push(Some(ChromeHover::Row(i)));
        out.push(Some(ChromeHover::Close(i)));
    }
    out
}

#[test]
fn the_sidebar_from_its_tree_is_the_sidebar_painted_by_hand() {
    for scale in [1.0, 2.0] {
        at_scale(scale, || {
            let Some(mut stage) = Stage::new() else { return };
            let mut compared = 0;
            // roomy, short (rows run off the bottom), narrow
            for (w, h) in [(1800.0, 1100.0), (1200.0, 300.0), (500.0, 900.0)] {
                let window = (w * scale, h * scale);
                // one pane (cannot be closed), a few, a full sidebar, at the cap
                for n in [1, 4, 9, crate::ui::SESSION_COUNT_HARD_CAP] {
                    let l = layout(&stage, window, scale, n);
                    let rows = rows(n);
                    let cell_w = stage.font.cell_w as f32;
                    let mut surface = SidebarSurface::default();
                    let (mut by_hand, mut by_tree): (Frame, Frame) = Default::default();
                    for focused in [0, n / 2, n - 1, n] {
                        for hover in hovers(n) {
                            stage.draw(window, &mut by_hand, &mut |p| paint_sidebar(p, &l, &rows, focused, hover));
                            let f = SidebarFrame { layout: &l, rows: &rows, focused_idx: focused, hover, cell_w };
                            stage.draw(window, &mut by_tree, &mut |p| surface.frame(p, &f));
                            assert!(!by_hand.1.is_empty(), "scale {scale}, {window:?}, {n} rows: nothing was painted");
                            assert_eq!(by_tree, by_hand, "scale {scale}, {window:?}, {n} rows, focused {focused}, {hover:?}");
                            compared += 1;
                        }
                    }
                }
            }
            eprintln!("scale {scale}: {compared} sidebars, every instance the same");
        });
    }
}

/// The comparison can fail: another row in use, another row lit, or a
/// renamed pane is a different list of instances.
#[test]
fn a_sidebar_with_another_row_in_use_lit_or_named_is_noticed() {
    at_scale(1.0, || {
        let Some(mut stage) = Stage::new() else { return };
        let window = (1800.0, 1100.0);
        let l = layout(&stage, window, 1.0, 4);
        let rows = rows(4);
        let cell_w = stage.font.cell_w as f32;
        let (mut by_hand, mut by_tree): (Frame, Frame) = Default::default();
        stage.draw(window, &mut by_hand, &mut |p| paint_sidebar(p, &l, &rows, 1, Some(ChromeHover::Row(2))));
        let mut renamed = rows_of(&rows);
        renamed[3].label = "kevx";
        for (r, focused, hover) in [
            (&rows, 0, Some(ChromeHover::Row(2))),
            (&rows, 1, Some(ChromeHover::Row(3))),
            (&renamed, 1, Some(ChromeHover::Row(2))),
        ] {
            let f = SidebarFrame { layout: &l, rows: r, focused_idx: focused, hover, cell_w };
            stage.draw(window, &mut by_tree, &mut |p| SidebarSurface::default().frame(p, &f));
            assert_ne!(by_tree, by_hand, "focused {focused}, {hover:?} drew what the hand did");
        }
    });
}

/// A frame in which nothing on the sidebar changed paints nothing and
/// takes nothing from the heap; one in which a row lights, or a pane is
/// renamed, repaints without an allocation once the buffers have grown.
#[test]
fn sidebar_surface_frame() {
    at_scale(1.0, || {
        let Some(mut stage) = Stage::new() else { return };
        let window = (1800.0, 1100.0);
        let l = layout(&stage, window, 1.0, 9);
        let rows = rows(9);
        let mut renamed = rows_of(&rows);
        renamed[2].label = "insight";
        let cell_w = stage.font.cell_w as f32;
        let mut surface = SidebarSurface::default();
        let mut out: Frame = Default::default();
        let rest = SidebarFrame { layout: &l, rows: &rows, focused_idx: 4, hover: None, cell_w };
        let lit = SidebarFrame { hover: Some(ChromeHover::Close(2)), ..rest };
        let other = SidebarFrame { rows: &renamed, ..rest };
        for f in [&rest, &lit, &other, &rest, &lit, &other, &rest] {
            stage.draw(window, &mut out, &mut |p| surface.frame(p, f));
        }
        let warm = surface.kept.surface.counters();
        let (n, _) = counted(|| stage.draw(window, &mut out, &mut |p| surface.frame(p, &rest)));
        assert_eq!(surface.kept.surface.counters(), warm, "a frame with nothing new ran a stage");
        assert_eq!(n, 0, "a frame with nothing new allocated {n} times");
        assert!(out.1.len() >= 9, "the sidebar has to have been painted");
        let (n, _) = counted(|| stage.draw(window, &mut out, &mut |p| surface.frame(p, &lit)));
        assert_eq!(n, 0, "a close button lighting allocated {n} times");
        let (n, _) = counted(|| stage.draw(window, &mut out, &mut |p| surface.frame(p, &other)));
        assert_eq!(n, 0, "a renamed pane allocated {n} times");
        assert_eq!(surface.kept.surface.counters().build, warm.build + 2);
    });
}

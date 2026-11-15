//! What a frame of the sidebar costs drawn from its tree.

use super::sidebar_view::{SidebarFrame, SidebarSurface};
use super::toolbar_view_tests::{Frame, Stage};
use crate::frame_alloc_tests::counted;
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

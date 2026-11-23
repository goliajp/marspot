//! What a frame of the sidebar costs drawn from its tree.

use super::sidebar_view::{SidebarFrame, SidebarSurface};
use super::toolbar_view_tests::{Frame, Stage};
use crate::frame_alloc_tests::counted;
use crate::layout::Layout;
use crate::render::SidebarEntry;
use marspot_term::session::SessionState;
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
/// takes nothing from the heap; one in which a row lights paints again
/// and describes nothing, one in which a pane is renamed describes the
/// sidebar again, and neither allocates once the buffers have grown.
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
        let (rows, renamed): (&[SidebarEntry], &[SidebarEntry]) = (&rows, &renamed);
        let rest = SidebarFrame { layout: &l, rows: &rows, focused_idx: 4, quiet: false, cell_w };
        let other = SidebarFrame { rows: &renamed, ..rest };
        let c = l.close_session_rects[2];
        let (on, off) = ((c.x + c.w * 0.5, c.y_top + c.h * 0.5), (900.0, 600.0));
        for (f, p) in [(&rest, off), (&rest, on), (&other, off), (&rest, off), (&rest, on), (&other, off), (&rest, off)] {
            surface.kept.surface.pointer_move(p);
            stage.draw(window, &mut out, &mut |pa| surface.frame(pa, f));
        }
        let warm = surface.kept.surface.counters();
        let (n, _) = counted(|| stage.draw(window, &mut out, &mut |p| surface.frame(p, &rest)));
        assert_eq!(surface.kept.surface.counters(), warm, "a frame with nothing new ran a stage");
        assert_eq!(n, 0, "a frame with nothing new allocated {n} times");
        assert!(out.1.len() >= 9, "the sidebar has to have been painted");
        let (n, _) = counted(|| {
            surface.kept.surface.pointer_move(on);
            stage.draw(window, &mut out, &mut |p| surface.frame(p, &rest))
        });
        assert_eq!(n, 0, "a close button lighting allocated {n} times");
        let c = surface.kept.surface.counters();
        assert_eq!((c.build, c.layout, c.paint), (warm.build, warm.layout, warm.paint + 1), "a hover is a paint");
        let (n, _) = counted(|| {
            surface.kept.surface.pointer_move(off);
            stage.draw(window, &mut out, &mut |p| surface.frame(p, &other))
        });
        assert_eq!(n, 0, "a renamed pane allocated {n} times");
        assert_eq!(surface.kept.surface.counters().build, warm.build + 1);
    });
}

/// With more panes than the column is tall, the row that counts the
/// helpers is what is drawn and pressed at the bottom, at both
/// densities; nothing of a pane's row is drawn there.
#[test]
fn helper_panel_count_is_on_top_of_the_rows_that_reach_it() {
    use super::sidebar_view::{CLOSE, HELPERS, ROW};
    for scale in [1.0, 2.0] {
        at_scale(scale, || {
            let Some(mut stage) = Stage::new() else { return };
            let window = (1400.0 * scale, 900.0 * scale);
            let row_h = marspot_term::layout::sidebar_row_h();
            let cell_w = stage.font.cell_w as f32;
            let mut picture = |n: usize| {
                let l = layout(&stage, window, scale, n);
                let rows = rows(n);
                let rows: &[SidebarEntry] = &rows;
                let f = SidebarFrame { layout: &l, rows: &rows, focused_idx: n, quiet: false, cell_w };
                let mut s = SidebarSurface::default();
                s.set_helpers(3);
                let mut out: Frame = Default::default();
                stage.draw(window, &mut out, &mut |p| s.frame(p, &f));
                let top = (window.1 - row_h) as f32;
                let low: Vec<_> = out.1.iter().filter(|g| g.origin[1] + g.size[1] > top).map(|g| g.origin).collect();
                let hits: Vec<Option<u32>> = (0..4)
                    .flat_map(|i| (1..20).map(move |x| (x, i)))
                    .map(|(x, i)| (l.sidebar_w * x as f64 / 20.0, window.1 - (i as f64 + 0.5) * row_h * 0.25))
                    .map(|p| s.kept.surface.hit(p).map(|h| h.action.id))
                    .collect();
                (low, hits, s.kept.surface.hit((l.sidebar_w * 0.25, window.1 - row_h * 2.5)).map(|h| h.action.id))
            };
            let (few, _, _) = picture(1);
            assert!(!few.is_empty(), "the label is drawn");
            for n in [0, 60] {
                let (many, hits, above) = picture(n);
                assert_eq!(many, few, "{n} rows at scale {scale}");
                assert!(hits.iter().all(|h| *h == Some(HELPERS)), "{n} rows at scale {scale}: {hits:?}");
                if n > 0 {
                    assert!(matches!(above, Some(ROW | CLOSE)), "the rows reach it: {above:?}");
                }
            }
        });
    }
}

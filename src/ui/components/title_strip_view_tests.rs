//! What a frame of the title strips costs drawn from their tree.

use super::title_strip_view::{TitleStripSurface, TitleStrips};
use super::toolbar_view_tests::{Frame, Stage};
use crate::frame_alloc_tests::counted;
use crate::layout::Layout;
use crate::render::SessionView;
use crate::terminal::Terminal;
use crate::ui::components::density_kit::at_scale;

/// `(title, badge)`: a plain name and badge, a name too long for its
/// strip, wide characters on both sides, a badge with no space, a badge
/// wider than the pane, no title (nothing drawn), no badge.
const STRIPS: [(&str, &str); 9] = [
    ("smix", "P1 3f9c2a1e"),
    ("insight-mono-a-name-much-longer-than-any-strip-is-wide", "P2 77d0e4b2"),
    ("日本語の名前", "Ｐ３ 全角"),
    ("alt", "P4"),
    ("torajs", "P5 a-badge-wider-than-any-pane-in-a-three-by-three-grid-could-ever-be"),
    ("", "P6 0c41aa90"),
    ("devops", ""),
    ("混合 mixed", "P7 9e02b7c3"),
    ("kevy", ""),
];

fn views<'a>(term: &'a Terminal, strips: &'a [(&'a str, &'a str)], focused: usize) -> Vec<SessionView<'a>> {
    strips
        .iter()
        .enumerate()
        .map(|(i, &(title, right_badge))| SessionView {
            grid: term.grid(),
            view_offset: 0,
            cursor_visible: false,
            focused: i == focused,
            title,
            selection: None,
            ime_preedit: "",
            dormant: false,
            recede: 0,
            scrim: 0.0,
            right_badge,
            agent_tui: false,
            cwd: "",
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
            seq: 0,
        })
        .collect()
}

fn layout(stage: &Stage, (w, h): (f64, f64), scale: f64, title_pt: f64) -> Layout {
    Layout::build(
        w,
        h,
        crate::ui::SIDEBAR_W_LOGICAL * scale,
        crate::HEADER_PT * scale,
        title_pt * scale,
        3,
        3,
        stage.font.cell_w,
        stage.font.cell_h,
    )
    .with_chrome(scale, 9, None)
}

/// A frame in which no strip changed paints nothing and takes nothing
/// from the heap; a new badge or another pane in use repaints without
/// an allocation once the buffers have grown.
#[test]
fn title_strip_surface_frame() {
    at_scale(1.0, || {
        let Some(mut stage) = Stage::new() else { return };
        let term = Terminal::new(20, 5);
        let window = (1800.0, 1100.0);
        let l = layout(&stage, window, 1.0, crate::ui::CELL_TITLE_PT);
        let mut other = STRIPS;
        other[0].1 = "P1 ffffffff";
        let rest = views(&term, &STRIPS, 4);
        let moved = views(&term, &STRIPS, 5);
        let badged = views(&term, &other, 4);
        let mut surface = TitleStripSurface::default();
        let mut out: Frame = Default::default();
        let frames = [
            TitleStrips { layout: &l, views: &rest, window_focused: true },
            TitleStrips { layout: &l, views: &moved, window_focused: true },
            TitleStrips { layout: &l, views: &badged, window_focused: true },
        ];
        for s in frames.iter().cycle().take(7) {
            stage.draw(window, &mut out, &mut |p| surface.frame(p, s));
        }
        let warm = surface.kept.surface.counters();
        let (n, _) = counted(|| stage.draw(window, &mut out, &mut |p| surface.frame(p, &frames[0])));
        assert_eq!(surface.kept.surface.counters(), warm, "a frame with nothing new ran a stage");
        assert_eq!(n, 0, "a frame with nothing new allocated {n} times");
        assert!(out.1.len() >= 20, "the strips have to have been painted");
        for s in &frames[1..] {
            let (n, _) = counted(|| stage.draw(window, &mut out, &mut |p| surface.frame(p, s)));
            assert_eq!(n, 0, "a changed strip allocated {n} times");
        }
        assert_eq!(surface.kept.surface.counters().build, warm.build + 2);
    });
}

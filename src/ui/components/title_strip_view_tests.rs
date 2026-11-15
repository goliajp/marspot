//! The title strips drawn from their tree against the strips each pane
//! painted by hand, and what a frame of them costs.

use super::title_strip_view::{TitleStripSurface, TitleStrips};
use super::toolbar_view_tests::{Frame, Stage};
use crate::frame_alloc_tests::counted;
use crate::frame_build::session_marks::paint_title_strip;
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

/// What every pane painted of its strip, in pane order.
fn by_hand(stage: &mut Stage, window: (f64, f64), into: &mut Frame, l: &Layout, views: &[SessionView], focused_window: bool) {
    stage.draw(window, into, &mut |p| {
        for (view, rect) in views.iter().zip(&l.cells) {
            paint_title_strip(
                rect,
                view,
                view.focused && focused_window,
                p.cell_w,
                p.cell_h,
                p.ascent,
                p.atlas_w,
                p.atlas_h,
                p.font,
                p.atlas,
                p.cells,
                p.glyphs,
                l.gutter as f32,
                l.padding as f32,
                l.cell_title_h as f32,
            );
        }
    });
}

#[test]
fn the_title_strips_from_their_tree_are_the_strips_painted_by_hand() {
    for scale in [1.0, 2.0] {
        at_scale(scale, || {
            let Some(mut stage) = Stage::new() else { return };
            let term = Terminal::new(20, 5);
            let mut compared = 0;
            // roomy, narrow panes, wide; a strip of no height draws nothing
            for (w, h) in [(1800.0, 1100.0), (700.0, 500.0), (3200.0, 1800.0)] {
                for title_pt in [crate::ui::CELL_TITLE_PT, 0.0] {
                    let window = (w * scale, h * scale);
                    let l = layout(&stage, window, scale, title_pt);
                    let mut surface = TitleStripSurface::default();
                    let (mut hand, mut tree): (Frame, Frame) = Default::default();
                    for focused in [0, 2, 4, 9] {
                        for window_focused in [true, false] {
                            let vs = views(&term, &STRIPS, focused);
                            by_hand(&mut stage, window, &mut hand, &l, &vs, window_focused);
                            let s = TitleStrips { layout: &l, views: &vs, window_focused };
                            stage.draw(window, &mut tree, &mut |p| surface.frame(p, &s));
                            assert_eq!(tree, hand, "scale {scale}, {window:?}, title {title_pt}, focused {focused}/{window_focused}");
                            compared += 1;
                        }
                    }
                }
            }
            eprintln!("scale {scale}: {compared} sets of strips, every instance the same");
        });
    }
}

/// The comparison can fail: another pane in use, or a badge changed,
/// is a different list of instances.
#[test]
fn title_strips_with_another_pane_in_use_or_badge_are_noticed() {
    at_scale(1.0, || {
        let Some(mut stage) = Stage::new() else { return };
        let term = Terminal::new(20, 5);
        let window = (1800.0, 1100.0);
        let l = layout(&stage, window, 1.0, crate::ui::CELL_TITLE_PT);
        let (mut hand, mut tree): (Frame, Frame) = Default::default();
        let vs = views(&term, &STRIPS, 0);
        by_hand(&mut stage, window, &mut hand, &l, &vs, true);
        assert!(!hand.1.is_empty() && !hand.0.is_empty(), "nothing was painted");
        let mut other = STRIPS;
        other[3].1 = "P9";
        for (strips, focused) in [(&STRIPS, 1), (&other, 0)] {
            let vs = views(&term, strips, focused);
            let s = TitleStrips { layout: &l, views: &vs, window_focused: true };
            stage.draw(window, &mut tree, &mut |p| TitleStripSurface::default().frame(p, &s));
            assert_ne!(tree, hand, "focused {focused} drew what the hand did");
        }
    });
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

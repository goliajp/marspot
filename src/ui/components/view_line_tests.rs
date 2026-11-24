//! Where the header line of a lone pane showing a helper is drawn, at
//! both display scales.

use super::toolbar_view::Toolbar;
use super::toolbar_view_tests::{Frame, Stage};
use super::view_line::{paint, place};
use crate::layout::Layout;
use crate::ui::components::density_kit::at_scale;

const TEXT: &str = "spg › codex gpt-5 fix the parser  ⌘W";

fn lone(stage: &Stage, (w, h): (f64, f64), scale: f64, panes: usize) -> Layout {
    let title_h = if panes > 1 { crate::ui::CELL_TITLE_PT * scale } else { 0.0 };
    let (cols, rows) = if panes > 1 { (2, 1) } else { (1, 1) };
    Layout::build(w, h, crate::ui::SIDEBAR_W_LOGICAL * scale, crate::HEADER_PT * scale, title_h, cols, rows, stage.font.cell_w, stage.font.cell_h)
        .with_chrome(scale, panes, None)
}

fn cell(stage: &Stage) -> (f32, f32, f32) {
    (stage.font.cell_w as f32, stage.font.cell_h as f32, stage.font.ascent as f32)
}

/// The line's glyphs as `[left, top, right, bottom]`, all of them.
fn bounds(out: &Frame) -> Option<[f32; 4]> {
    out.1.iter().fold(None, |b, g| {
        let (x0, y0, x1, y1) = (g.origin[0], g.origin[1], g.origin[0] + g.size[0], g.origin[1] + g.size[1]);
        Some(b.map_or([x0, y0, x1, y1], |b: [f32; 4]| [b[0].min(x0), b[1].min(y0), b[2].max(x1), b[3].max(y1)]))
    })
}

/// At scale 1 and 2: in the header, right of the toolbar, left of the
/// version label, over the middle of the pane; the pane keeps every row.
#[test]
fn peek_line_sits_on_the_header_row_at_both_scales() {
    let mut baselines = Vec::new();
    for scale in [1.0, 2.0] {
        at_scale(scale, || {
            let Some(mut stage) = Stage::new() else { return };
            let window = (1400.0 * scale, 900.0 * scale);
            let l = lone(&stage, window, scale, 1);
            let at = place(&l, TEXT, cell(&stage)).expect("a lone pane in a window with a header says it");
            assert!(!at.clipped && at.bytes == TEXT.len());
            let mut out = Frame::default();
            stage.draw(window, &mut out, &mut |p| paint(p, &l, TEXT));
            let [x0, y0, x1, y1] = bounds(&out).expect("the line is drawn");
            let buttons_end = l.toolbar_buttons().iter().map(|b| b.x + b.w).fold(0.0, f64::max) as f32;
            let version_x = Toolbar::of(&l, [false; 5], cell(&stage)).label.unwrap().0;
            assert!(x0 >= buttons_end && x1 <= version_x, "{x0}..{x1} between {buttons_end} and {version_x}");
            assert!(y0 >= 0.0 && y1 <= l.top_inset as f32, "{y0}..{y1} in a header {} tall", l.top_inset);
            let pane = l.cells[0];
            let mid = (pane.x + pane.w * 0.5) as f32;
            assert!(((x0 + x1) * 0.5 - mid).abs() <= 2.0 * stage.font.cell_w as f32, "centred over the pane");
            let full = Layout::build(window.0, window.1, l.sidebar_w, l.top_inset, 0.0, 1, 1, stage.font.cell_w, stage.font.cell_h);
            assert_eq!((pane.cols, pane.rows), (full.cells[0].cols, full.cells[0].rows), "no row is taken");
            baselines.push(at.baseline);
        });
    }
    if let [one, two] = baselines[..] {
        assert!((two - 2.0 * one).abs() <= 2.0, "the baseline doubles: {one} and {two}");
    }
}

/// Nothing to say, panes with strips of their own, no header: no line.
#[test]
fn peek_line_is_not_drawn_without_a_lone_pane_viewing_and_a_header() {
    at_scale(1.0, || {
        let Some(mut stage) = Stage::new() else { return };
        let window = (1400.0, 900.0);
        let c = cell(&stage);
        assert_eq!(place(&lone(&stage, window, 1.0, 1), "", c), None);
        let two = lone(&stage, window, 1.0, 2);
        assert_eq!(place(&two, TEXT, c), None, "the title strip says it");
        let mut out = Frame::default();
        stage.draw(window, &mut out, &mut |p| paint(p, &two, TEXT));
        assert!(out.1.is_empty());
        let bare = Layout::build(1400.0, 900.0, 0.0, 0.0, 0.0, 1, 1, stage.font.cell_w, stage.font.cell_h);
        assert_eq!(place(&bare, TEXT, c), None);
    });
}

/// A button the layout does not show (zero wide) does not push the
/// line right, wherever its rect is.
#[test]
fn peek_line_ignores_a_hidden_button() {
    at_scale(1.0, || {
        let Some(stage) = Stage::new() else { return };
        let l = lone(&stage, (1400.0, 900.0), 1.0, 1);
        let shown = place(&l, TEXT, cell(&stage)).unwrap();
        let mut hidden = l.clone();
        hidden.settings_button_rect.w = 0.0;
        hidden.settings_button_rect.x = 1000.0;
        let mut cut = l.clone();
        cut.settings_button_rect.w = 0.0;
        let at = place(&hidden, TEXT, cell(&stage)).unwrap();
        assert_eq!(at, place(&cut, TEXT, cell(&stage)).unwrap());
        assert!(at.x < 1000.0, "{at:?}");
        assert_eq!(at.x, shown.x, "centred as before: {at:?}");
    });
}

/// A window too narrow for the line cuts it with an ellipsis and keeps
/// it off the toolbar and the version label.
#[test]
fn peek_line_is_cut_short_in_a_narrow_window() {
    at_scale(1.0, || {
        let Some(mut stage) = Stage::new() else { return };
        let window = (720.0, 400.0);
        let l = lone(&stage, window, 1.0, 1);
        let long = TEXT.repeat(4);
        let at = place(&l, &long, cell(&stage)).expect("room for some of it");
        assert!(at.clipped && at.bytes < long.len());
        let mut out = Frame::default();
        stage.draw(window, &mut out, &mut |p| paint(p, &l, &long));
        let [x0, _, x1, _] = bounds(&out).unwrap();
        let buttons_end = l.toolbar_buttons().iter().map(|b| b.x + b.w).fold(0.0, f64::max) as f32;
        let version_x = Toolbar::of(&l, [false; 5], cell(&stage)).label.unwrap().0;
        assert!(x0 >= buttons_end && x1 <= version_x, "{x0}..{x1} between {buttons_end} and {version_x}");
    });
}

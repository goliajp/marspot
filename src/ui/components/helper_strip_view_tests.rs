//! A strip under a pane's terminal: its picture, its pictures' sizes at
//! two densities, and the scrollbar that stops above it.

use super::chrome_snapshot_tests::chrome_snapshot_helper_strip;
use super::density_kit::{at_scale, capture, compare};
use super::scrollbar::Scrollbar;
use crate::layout::{cell_grid, Layout};
use crate::render::{HistoryView, StripView};
use marspot_term::grid::{Cell, CellAttrs, Grid};

/// A strip as an agent draws its own list: a blank row, the pane's own
/// row, then one per helper.
pub(crate) fn strip_scene(cols: u16, rows: u16) -> Grid {
    let mut g = Grid::with_scrollback(cols, rows, 0);
    let mut put = |col: u16, row: u16, text: &str, dim: bool| {
        for (c, ch) in (col..cols).zip(text.chars()) {
            g.set_cell(c, row, Cell { ch, attrs: CellAttrs { dim, ..CellAttrs::default() } });
        }
    };
    put(2, 1, "● main", false);
    for row in 2..rows {
        put(2, row, "○ codex", true);
        put(12, row, "review the diff for errors", false);
        put(cols - 26, row, "1m 32s · ↓ 115.3k tokens", true);
    }
    g
}

/// `strip` right under what `screen` holds.
pub(crate) fn strip_under<'a>(screen: &Grid, strip: &'a Grid) -> StripView<'a> {
    StripView { grid: strip, seq: 1, top_row: screen.last_used_row() + 1 }
}

#[test]
fn helper_strip_snapshot_2x() {
    at_scale(2.0, chrome_snapshot_helper_strip);
}

#[test]
fn helper_strip_holds_at_both_densities() {
    let Some(one) = capture(1.0, chrome_snapshot_helper_strip) else {
        eprintln!("SKIPPED: no Metal device");
        return;
    };
    let two = capture(2.0, chrome_snapshot_helper_strip).expect("a frame at scale 2");
    if let Err(first) = compare(&one, &two, 2.0).first() {
        panic!("{first}");
    }
}

#[test]
fn helper_strip_is_below_the_scrollbars_track() {
    at_scale(1.0, || {
        let (w, h) = (8.43, 16.6);
        let plain = Layout::build(900.0, 800.0, 0.0, 40.0, 0.0, 1, 1, w, h);
        let l = plain.clone().with_strips(&[4], w, h);
        let history = HistoryView { offset: 1, len: 500 };
        let bar = |l: &Layout| Scrollbar::of(&l.cells[0], 0.0, l.cells[0].rows, history).unwrap();
        let g = cell_grid(&l.cells[0], 0.0, l.padding, w, h);
        let track = bar(&l).track;
        let end = track.y_top + track.h;
        assert!(end < g.row_top(l.cells[0].rows), "{end}");
        assert!(bar(&l).thumb.y_top + bar(&l).thumb.h <= end);
        assert!(bar(&l).press(track.x, g.row_top(l.cells[0].rows) + 2.0).is_none());
        let before = bar(&plain).track;
        assert!(before.y_top + before.h > end + 3.0 * g.h);
    });
}

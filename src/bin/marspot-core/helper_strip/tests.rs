use super::strip::{Run, Strip as One};
use super::*;
use crate::boot_assembly_tests::Sandbox;
use crate::window_state_tests::{app_in, win};
use marspot_term::grid::{Cell, CellAttrs, Grid};
use marspot_term::session_registry as reg;
use marspot_term::terminal::Terminal;

pub(super) fn dim() -> CellAttrs {
    CellAttrs { dim: true, ..CellAttrs::default() }
}

pub(super) fn run(col: u16, text: &str) -> Run<'_> {
    Run { col, text, attrs: dim() }
}

pub(super) fn text(g: &Grid, row: u16) -> String {
    (0..g.cols()).map(|c| g.cell(c, row).ch).filter(|c| *c != '\0').collect()
}

fn screen(cols: u16, rows: u16, bytes: &[u8]) -> Terminal {
    let mut t = Terminal::new(cols, rows);
    t.feed(bytes);
    t
}

fn over(screen: &Grid, shown: u64, seq: u64, scrolled: bool) -> Over<'_> {
    Over { screen, shown, seq, scrolled, rows: 10, preedit_end: None, now: Instant::now() }
}

/// A window of one pane that gives up `rows` rows, laid out and placed
/// as a frame would.
pub(super) fn app_with_strip(sb: &Sandbox, rows: u16) -> (CoreApp, u64) {
    let sid = reg::allocate_next_session_id().unwrap();
    let mut app = app_in(sb.dir(), vec![win(1, vec![Pane::new_vacant(sid, 60, 16)])]);
    app.strips.set_rows(sid, rows);
    app.rebuild_layout(0);
    app.place_strips(0);
    (app, sid)
}

/// The middle of column 3 of row `row` of the only pane.
pub(super) fn at_row(app: &CoreApp, row: u16) -> (f64, f64) {
    let l = &app.windows[0].layout;
    let (cw, ch) = app.renderer.cell_dims();
    let g = marspot::layout::cell_grid(&l.cells[0], l.cell_title_h, l.padding, cw, ch);
    (g.x + 3.5 * g.w, g.row_top(row) + g.h * 0.5)
}

#[test]
fn the_rows_come_out_of_the_panes_terminal_and_go_back() {
    let sb = Sandbox::new();
    let (mut app, sid) = app_with_strip(&sb, 0);
    let full = app.windows[0].layout.cells[0].rows;
    assert!(full >= 20, "{full} rows");
    assert_eq!(app.windows[0].panes[0].session().grid().rows(), full);
    app.strips.set_rows(sid, 4);
    app.rebuild_layout(0);
    let c = app.windows[0].layout.cells[0];
    assert_eq!((c.rows, c.strip_rows), (full - 4, 4));
    assert_eq!(app.windows[0].panes[0].session().grid().rows(), full - 4, "the pane is told its new height");
    app.strips.set_rows(sid, 99);
    app.rebuild_layout(0);
    assert_eq!(app.windows[0].layout.cells[0].strip_rows, MAX_ROWS);
    app.retire_pane_session(sid, false);
    app.rebuild_layout(0);
    assert_eq!(app.windows[0].layout.cells[0].rows, full, "a retired session's strip goes with it");
}

#[test]
fn a_short_pane_shows_none() {
    let sb = Sandbox::new();
    let (mut app, sid) = app_with_strip(&sb, 4);
    app.windows[0].h_phys = 150.0;
    app.rebuild_layout(0);
    app.place_strips(0);
    let c = app.windows[0].layout.cells[0];
    assert!(c.rows < marspot::layout::STRIP_MIN_TOTAL_ROWS, "{} rows", c.rows);
    assert_eq!(c.strip_rows, 0);
    assert!(app.strips.view(Some(sid)).is_none(), "and nothing is drawn");
    app.windows[0].h_phys = 600.0;
    app.rebuild_layout(0);
    app.place_strips(0);
    assert_eq!(app.windows[0].layout.cells[0].strip_rows, 4);
    assert!(app.strips.view(Some(sid)).is_some());
}

/// The vacant pane's message is on row 1, so the strip is drawn at row
/// 2, over blank rows of the terminal.
#[test]
fn the_pointer_finds_the_strip_where_it_is_drawn() {
    let sb = Sandbox::new();
    let (app, sid) = app_with_strip(&sb, 4);
    assert_eq!(app.strips.view(Some(sid)).unwrap().top_row, 2);
    let rows = app.windows[0].layout.cells[0].rows;
    for row in 0..rows + 4 {
        let (x, y) = at_row(&app, row);
        let strip = app.strip_at(0, x, y);
        let cell = app.cell_pos_at(0, x, y);
        match row {
            2..=5 => assert_eq!((strip, cell), (Some((0, row - 2)), None), "row {row}"),
            r if r < rows => assert_eq!((strip, cell), (None, Some((0, 3, row))), "row {row}"),
            _ => assert_eq!((strip, cell), (None, None), "row {row}"),
        }
    }
}

#[test]
fn a_press_on_the_strip_selects_nothing_and_opens_no_menu() {
    let sb = Sandbox::new();
    let (mut app, _) = app_with_strip(&sb, 4);
    let none = Modifiers::default();
    let (x, y) = at_row(&app, 3);
    app.mouse_down(0, x, y, none);
    assert!(app.windows[0].selection.is_none() && !app.windows[0].selection_dragging);
    app.mouse_up(0, 1, x, y);
    app.mouse_right_down(0, x, y, none);
    assert!(app.windows[0].context_menu.is_none());
    assert!(app.hit_test_link_at_xy(0, x, y).is_none());
    let (x, y) = at_row(&app, 1);
    app.mouse_right_down(0, x, y, none);
    assert!(app.windows[0].context_menu.take().is_some(), "the terminal's own rows still have one");
    app.mouse_down(0, x, y, none);
    assert!(app.windows[0].selection_dragging, "and still start a selection");
}

#[test]
fn a_drag_into_the_strip_ends_on_the_row_above_it() {
    let sb = Sandbox::new();
    let (mut app, _) = app_with_strip(&sb, 4);
    let rows = app.windows[0].layout.cells[0].rows;
    let none = Modifiers::default();
    let (x, y) = at_row(&app, 0);
    app.mouse_down(0, x, y, none);
    let (x, y) = at_row(&app, 1);
    app.mouse_drag(0, x, y, 1, x, y);
    assert_eq!(app.windows[0].selection.unwrap().focus, (3, rows as u32 - 2), "counted from the bottom row");
    for row in [2, 4, rows - 2, rows + 2] {
        let (x, y) = at_row(&app, row);
        app.mouse_drag(0, x, y, 1, x, y);
        assert_eq!(app.windows[0].selection.unwrap().focus, (3, rows as u32 - 2), "row {row}: the strip is at row 2");
    }
}

#[test]
fn it_follows_the_screens_last_row_and_stops_at_the_terminals_end() {
    let mut s = One::new(3);
    s.fit(20, 3);
    let place = |s: &mut One, bytes: &[u8], seq: u64, scrolled: bool| {
        s.place(over(screen(20, 10, bytes).grid(), 1, seq, scrolled));
        s.top_row()
    };
    assert_eq!(place(&mut s, b"a\r\nb", 1, false), Some(2));
    assert_eq!(place(&mut s, b"\x1b[10;1Hx\x1b[H", 2, false), Some(10), "a full screen: under its last row");
    assert_eq!(place(&mut s, b"a", 3, true), Some(10), "history: under the last row");
    s.place(over(screen(20, 12, b"a").grid(), 1, 4, false));
    assert_eq!(s.top_row(), None, "a screen not yet cut down to size has rows there");
    assert!(s.view().is_none());
}

#[test]
fn a_screen_is_gone_through_once_per_frame_of_it() {
    let mut s = One::new(3);
    let (short, long) = (screen(20, 10, b"a"), screen(20, 10, b"a\r\nb\r\nc"));
    s.place(over(short.grid(), 1, 7, false));
    s.place(over(long.grid(), 1, 7, false));
    assert_eq!(s.top_row(), Some(1), "the same frame: not looked at again");
    s.place(over(long.grid(), 2, 7, false));
    assert_eq!(s.top_row(), Some(3), "another session at the same frame");
    s.place(over(short.grid(), 1, 8, false));
    assert_eq!(s.top_row(), Some(1), "another session's, where it is at once");
}

#[test]
fn a_line_is_written_where_it_differs_and_nowhere_else() {
    let mut s = One::new(2);
    assert!(s.fit(12, 2));
    assert!(!s.fit(12, 2), "the same size keeps what it holds");
    s.line(0, &[run(1, "ab"), run(6, "1s")]);
    assert_eq!(text(s.grid(), 0), " ab   1s    ");
    assert_eq!(s.grid().cell(6, 0), Cell { ch: '1', attrs: dim() });
    let seq = s.seq();
    s.line(0, &[run(1, "ab"), run(6, "1s")]);
    assert_eq!(s.seq(), seq, "the same line again changes nothing");
    s.line(0, &[run(1, "ab"), run(6, "2s")]);
    assert_eq!(s.seq(), seq + 1, "one cell");
    s.line(0, &[run(1, "a")]);
    assert_eq!(text(s.grid(), 0), " a          ", "what was right of it is gone");
    s.line(9, &[run(0, "x")]);
}

#[test]
fn a_wide_character_takes_two_cells_and_is_not_cut_in_half() {
    let mut s = One::new(1);
    s.fit(5, 1);
    s.line(0, &[run(0, "漢字漢")]);
    assert_eq!(text(s.grid(), 0), "漢字 ");
    assert_eq!(s.grid().cell(1, 0).ch, '\0');
    s.line(0, &[run(0, "abcdefg")]);
    assert_eq!(text(s.grid(), 0), "abcde");
}

#[test]
fn the_dev_seam_fills_every_pane() {
    let sb = Sandbox::new();
    let sids: Vec<u64> = (0..2).map(|_| reg::allocate_next_session_id().unwrap()).collect();
    let panes = sids.iter().map(|s| Pane::new_vacant(*s, 60, 16)).collect();
    let mut w = win(1, panes);
    w.grid_cols = 2;
    let mut app = app_in(sb.dir(), vec![w]);
    app.strips = Strips { dev: Some(8), ..Strips::default() };
    app.rebuild_layout(0);
    app.place_strips(0);
    for sid in sids {
        let v = app.strips.view(Some(sid)).expect("a strip");
        assert!(text(v.grid, 1).starts_with("  ⏺ main"), "{:?}", text(v.grid, 1));
        assert!(text(v.grid, 3).contains("◯ reviewer"), "{:?}", text(v.grid, 3));
    }
}

#[test]
fn a_new_strip_is_not_taken_for_the_one_it_replaces() {
    let mut strips = Strips::default();
    strips.set_rows(5, 2);
    let first = strips.by_sid[&5].seq();
    strips.set_rows(5, 0);
    assert!(strips.is_empty());
    strips.set_rows(5, 2);
    assert_ne!(strips.by_sid[&5].seq(), first);
}

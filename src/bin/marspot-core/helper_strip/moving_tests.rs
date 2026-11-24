//! When a strip moves, what is under the pointer on it, and the text
//! that is hard to lay out.

use super::strip::{Run, Strip as One};
use super::tests::{app_with_strip, at_row, dim, run, text};
use super::*;
use crate::boot_assembly_tests::Sandbox;
use crate::pointer_drag::last_heard;
use marspot_term::grid::{Cell, Grid};
use marspot_term::terminal::Terminal;

const MS: Duration = Duration::from_millis(1);

fn screen(bytes: &[u8]) -> Terminal {
    let mut t = Terminal::new(20, 10);
    t.feed(bytes);
    t
}

fn over(screen: &Grid, seq: u64, now: Instant) -> Over<'_> {
    Over { screen, shown: 1, seq, scrolled: false, rows: 10, preedit_end: None, now }
}

#[test]
fn it_goes_down_at_once_and_up_when_the_screen_stays_short() {
    let (short, long) = (screen(b"a"), screen(b"a\r\nb\r\nc"));
    let mut s = One::new(3);
    let t = Instant::now();
    let put = |s: &mut One, g: &Grid, seq: u64, ms: u32| {
        s.place(over(g, seq, t + MS * ms));
        s.top_row().unwrap()
    };
    assert_eq!(put(&mut s, short.grid(), 1, 0), 1);
    assert_eq!(put(&mut s, long.grid(), 2, 1), 3, "down at once");
    assert_eq!(s.due(), None);
    assert_eq!(put(&mut s, short.grid(), 3, 10), 3, "not yet up");
    assert_eq!(s.due(), Some(t + MS * 160));
    assert_eq!(put(&mut s, long.grid(), 4, 100), 3);
    assert_eq!(s.due(), None, "the screen filled again");
    put(&mut s, short.grid(), 5, 120);
    assert_eq!(put(&mut s, short.grid(), 6, 269), 3, "149 ms of it");
    assert!(!s.ripen(t + MS * 269));
    assert!(s.ripen(t + MS * 270), "a frame is owed");
    assert!(!s.ripen(t + MS * 271) && s.due().is_none(), "once: the loop does not spin until it is drawn");
    assert_eq!(put(&mut s, short.grid(), 7, 270), 1);
}

#[test]
fn a_terminal_that_shrinks_takes_it_along_at_once() {
    let long = screen(b"a\x1b[10;1Hx");
    let mut s = One::new(3);
    let now = Instant::now();
    s.place(over(long.grid(), 1, now));
    assert_eq!(s.top_row(), Some(10));
    let mut small = Terminal::new(20, 6);
    small.feed(b"a");
    s.place(Over { rows: 6, ..over(small.grid(), 2, now) });
    assert_eq!(s.top_row(), Some(6), "never below the terminal's end");
}

#[test]
fn the_loop_wakes_for_it() {
    let sb = Sandbox::new();
    let (mut app, sid) = app_with_strip(&sb, 4);
    assert_eq!(app.strips.wait(Instant::now()), None);
    let seq = app.windows[0].panes[0].grid_seq();
    let full = screen(b"\x1b[10;1Hx");
    let strip = app.strips.by_sid.get_mut(&sid).unwrap();
    strip.place(Over { shown: sid, ..over(full.grid(), seq + 1, Instant::now()) });
    assert_eq!(strip.top_row(), Some(10));
    app.place_strips(0);
    assert!(app.idle_wait().is_some_and(|w| w <= MS * 150));
    app.windows[0].needs_render = false;
    app.strips_tick();
    assert!(!app.windows[0].needs_render);
    std::thread::sleep(MS * 160);
    app.strips_tick();
    assert!(app.windows[0].needs_render);
    assert_eq!(app.strips.wait(Instant::now()), None);
    app.place_strips(0);
    assert_eq!(app.strips.view(Some(sid)).unwrap().top_row, 2);
}

#[test]
fn it_is_below_what_is_being_composed() {
    let sb = Sandbox::new();
    let (mut app, sid) = app_with_strip(&sb, 4);
    let before = app.strips.view(Some(sid)).unwrap().top_row;
    app.windows[0].ime_preedit = "あ".repeat(app.windows[0].layout.cells[0].cols as usize * 2);
    app.place_strips(0);
    let top = app.strips.view(Some(sid)).unwrap().top_row;
    let grid = app.windows[0].panes[0].session().grid();
    let end = preedit_last_row(&app.windows[0].ime_preedit, grid).unwrap();
    assert!(end >= before, "or this shows nothing: {end} {before}");
    assert_eq!(top, end + 1);
    let (x, y) = at_row(&app, top);
    assert_eq!(app.strip_at(0, x, y), Some((0, 0)));
    let (x, y) = at_row(&app, end);
    assert_eq!(app.strip_at(0, x, y), None);
}

#[test]
fn a_press_on_it_drops_the_selection() {
    let sb = Sandbox::new();
    let (mut app, _) = app_with_strip(&sb, 4);
    let (x, y) = at_row(&app, 0);
    app.mouse_down(0, x, y, Modifiers::default());
    let (x, y) = at_row(&app, 1);
    app.mouse_drag(0, x + 40.0, y, 1, x + 40.0, y);
    app.mouse_up(0, 1, x + 40.0, y);
    assert!(app.windows[0].selection.is_some());
    let (x, y) = at_row(&app, 3);
    app.mouse_down(0, x, y, Modifiers::default());
    assert!(app.windows[0].selection.is_none());
}

#[test]
fn the_wheel_on_it_does_not_reach_the_program() {
    let sb = Sandbox::new();
    let (mut app, sid) = app_with_strip(&sb, 4);
    let keys = WheelKeys { enter: b"e".to_vec(), up: b"u".to_vec(), down: b"d".to_vec(), marker: Vec::new() };
    app.pane_wheel_keys.insert(sid, keys);
    let (x, y) = at_row(&app, 3);
    app.mouse_moved(0, x, y);
    app.scroll(0, 400.0, true);
    assert!(app.pane_wheel_open.is_empty());
    let (x, y) = at_row(&app, 0);
    app.mouse_moved(0, x, y);
    app.scroll(0, 400.0, true);
    assert!(!app.pane_wheel_open.is_empty(), "off it, the program hears the same tick");
}

#[test]
fn a_button_let_go_on_it_is_up_where_the_program_last_heard_of() {
    assert_eq!(last_heard((0, 4, 5), Some((0, 9, 2))), (9, 2));
    assert_eq!(last_heard((0, 4, 5), Some((1, 9, 2))), (4, 5), "another pane's");
    assert_eq!(last_heard((0, 4, 5), None), (4, 5));
}

/// The same text in a strip and printed in a terminal.
#[test]
fn text_takes_the_cells_a_terminal_gives_it() {
    for t in ["か\u{3099}き\u{3099} x", "👩\u{200d}👩\u{200d}👧 x", "e\u{301}x", "⚠\u{fe0f} x", "🇯🇵x", "한국"] {
        let mut term = Terminal::new(12, 2);
        term.feed(t.as_bytes());
        term.feed(b"\r\n");
        let mut s = One::new(1);
        s.fit(12, 1);
        s.line(0, &[Run { col: 0, text: t, attrs: Default::default() }]);
        for c in 0..12 {
            let (a, b) = (s.grid().cell(c, 0), term.grid().cell(c, 0));
            let (ta, tb) = (s.grid().cluster_text(&a), term.grid().cluster_text(&b));
            assert_eq!(ta, tb, "{t} at {c}");
            assert!(ta.is_some() || a.ch == b.ch, "{t} at {c}: {a:?} {b:?}");
        }
    }
}

#[test]
fn what_has_no_width_takes_no_cell() {
    let mut s = One::new(1);
    s.fit(8, 1);
    s.line(0, &[run(0, "a\tb\x1bc\r\n\u{200b}\u{202e}\u{7f}d")]);
    assert_eq!(text(s.grid(), 0), "abcd    ");
    for lone in ["\u{301}", "\u{200d}", "\u{fe0f}", "\u{3099}"] {
        s.line(0, &[run(0, lone), run(1, "x")]);
        assert_eq!(text(s.grid(), 0), " x      ", "{lone:?}");
    }
}

#[test]
fn a_cluster_is_rewritten_where_it_differs_and_its_pool_is_bounded() {
    let mut s = One::new(1);
    s.fit(6, 1);
    let line = |s: &mut One, t: &str| s.line(0, &[run(0, t)]);
    line(&mut s, "e\u{301}x");
    let seq = s.seq();
    line(&mut s, "e\u{301}x");
    assert_eq!(s.seq(), seq);
    line(&mut s, "o\u{301}x");
    assert_eq!((s.seq(), s.grid().cluster_pool_len()), (seq + 1, 1), "in place");
    assert_eq!(s.grid().cell(1, 0), Cell { ch: 'x', attrs: dim() });
    for _ in 0..50 {
        line(&mut s, "abc");
        line(&mut s, "e\u{301}x");
    }
    assert!(s.grid().cluster_pool_len() <= 7, "{}", s.grid().cluster_pool_len());
    assert_eq!(s.grid().cluster_text(&s.grid().cell(0, 0)), Some("e\u{301}"));
}

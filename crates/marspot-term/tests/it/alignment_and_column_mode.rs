//! DECALN and DECCOLM, through the parser.
//!
//! Both are old, both are still sent, and both were being dropped.
//! DECALN was worse than dropped: `ESC # 8` reached the arm for
//! `ESC 8` and restored a saved cursor, so a program asking to paint
//! the screen got its cursor moved instead.
//!
//! vttest opens with DECALN, which is why it matters more than its age
//! suggests -- a terminal that ignores it runs every test after it
//! against a screen that still holds the shell's own echo.

use marspot_term::terminal::Terminal;

fn row(t: &Terminal, r: u16) -> String {
    let cols = t.grid().cols();
    (0..cols).map(|c| t.grid().cell(c, r).ch).collect::<String>()
}

fn screen(t: &Terminal) -> String {
    (0..t.grid().rows()).map(|r| row(t, r)).collect::<Vec<_>>().join("\n")
}

fn blank_screen(rows: usize) -> String {
    vec![" ".repeat(8); rows].join("\n")
}

fn busy() -> Terminal {
    let mut t = Terminal::new(8, 4);
    t.feed(b"\x1b[2;3r"); // a scroll region, so we can watch it go
    t.feed(b"\x1b[31mhello");
    t
}

#[test]
fn decaln_fills_the_whole_screen_with_e() {
    let mut t = busy();
    t.feed(b"\x1b#8");
    assert_eq!(screen(&t), ["EEEEEEEE"; 4].join("\n"));
}

#[test]
fn decaln_brings_the_cursor_home_and_opens_the_margins() {
    let mut t = busy();
    t.feed(b"\x1b[4;4H"); // park the cursor at the bottom
    t.feed(b"\x1b#8");
    assert_eq!(t.grid().cursor(), (0, 0), "DECALN homes the cursor");

    // The margins are back at the extremes: eight line feeds from the
    // top of a four-row screen scroll the pattern off, which a region
    // of rows 2..3 would not have allowed.
    t.feed(b"\x1b[H");
    for _ in 0..8 {
        t.feed(b"\n");
    }
    assert_eq!(row(&t, 3), " ".repeat(8), "the scroll region was still in force");
}

#[test]
fn decaln_is_not_a_cursor_restore() {
    let mut t = Terminal::new(8, 4);
    t.feed(b"\x1b[3;5H\x1b7"); // DECSC at row 3, col 5
    t.feed(b"\x1b[H");
    t.feed(b"\x1b#8");
    assert_eq!(t.grid().cursor(), (0, 0), "ESC # 8 is DECALN, not ESC 8");
    assert_eq!(row(&t, 0), "EEEEEEEE");
}

#[test]
fn decsc_and_decrc_still_work_on_their_own() {
    let mut t = Terminal::new(8, 4);
    t.feed(b"\x1b[3;5H\x1b7\x1b[H\x1b8");
    assert_eq!(t.grid().cursor(), (4, 2), "guarding on the intermediate kept DECRC");
}

#[test]
fn deccolm_clears_the_screen_both_ways() {
    for mode in [&b"\x1b[?3h"[..], b"\x1b[?3l"] {
        let mut t = busy();
        t.feed(mode);
        assert_eq!(screen(&t), blank_screen(4), "{mode:?} must clear");
        assert_eq!(t.grid().cursor(), (0, 0));
    }
}

#[test]
fn deccolm_opens_the_margins() {
    let mut t = busy();
    t.feed(b"\x1b[?3h");
    t.feed(b"row0");
    t.feed(b"\x1b[4;1H");
    t.feed(b"\n");
    assert_eq!(row(&t, 0), " ".repeat(8), "a full-height region scrolled row 0 away");
}

/// The pane's width is the window's business, not a program's.
#[test]
fn deccolm_does_not_resize_the_pane() {
    let mut t = busy();
    t.feed(b"\x1b[?3h");
    assert_eq!(t.grid().cols(), 8);
    t.feed(b"\x1b[?3l");
    assert_eq!(t.grid().cols(), 8);
}

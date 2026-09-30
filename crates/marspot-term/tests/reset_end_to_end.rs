//! `reset` works, through the parser.
//!
//! `reset(1)` sends RIS (`ESC c`) and programs recovering from a
//! confused predecessor send DECSTR (`CSI ! p`).  Both used to fall
//! through to a no-op arm, so the user-visible symptom was that
//! `reset` did nothing at all.
//!
//! The `CSI 3 J` test is not about a fix — that one already worked.
//! It is here because a stale comment in `erase_in_display` said it
//! was deferred, which is what a regression would look like if the
//! arm were ever removed.

use marspot_term::terminal::{MouseTrackingMode, Terminal};

fn row(t: &Terminal, r: u16) -> String {
    let cols = t.grid().cols();
    (0..cols).map(|c| t.grid().cell(c, r).ch).collect::<String>().trim_end().to_string()
}

/// Put the terminal into the state a crashed full-screen program
/// leaves behind: alt screen, mouse reporting on, bracketed paste on,
/// cursor hidden, application cursor keys, a scroll region, and text
/// on screen.
fn confused() -> Terminal {
    let mut t = Terminal::new(40, 6);
    t.feed(b"scrollback line\r\n");
    t.feed(b"\x1b[?1049h"); // alt screen
    t.feed(b"\x1b[?1002h\x1b[?1006h"); // mouse tracking, SGR encoding
    t.feed(b"\x1b[?2004h"); // bracketed paste
    t.feed(b"\x1b[?25l"); // cursor hidden
    t.feed(b"\x1b[?1h"); // application cursor keys
    t.feed(b"\x1b[2;4r"); // scroll region
    t.feed(b"\x1b[31mred text");
    t
}

#[test]
fn ris_puts_everything_back() {
    let mut t = confused();
    assert!(t.in_alt_screen(), "the fixture has to actually be confused");
    assert!(!t.cursor_visible());
    assert!(t.bracketed_paste_mode());
    assert_ne!(t.mouse_tracking_mode(), MouseTrackingMode::Off);

    t.feed(b"\x1bc");

    assert!(!t.in_alt_screen(), "RIS leaves the alternate buffer");
    assert!(t.cursor_visible(), "RIS shows the cursor");
    assert!(!t.bracketed_paste_mode(), "RIS clears bracketed paste");
    assert_eq!(t.mouse_tracking_mode(), MouseTrackingMode::Off, "RIS stops mouse reports");
    assert_eq!(row(&t, 0), "", "RIS blanks the screen");
    assert_eq!(t.grid().scrollback_len(), 0, "RIS drops the scrollback");
    assert_eq!(t.grid().cursor(), (0, 0), "RIS puts the cursor home");
}

#[test]
fn ris_used_to_do_nothing_at_all() {
    // The regression this exists for.  `ESC c` reached a catch-all arm
    // that discarded it, so `reset` in a pane left by a crashed TUI
    // changed nothing the user could see.
    let mut t = confused();
    t.feed(b"\x1bc");
    assert!(
        !t.in_alt_screen() && t.cursor_visible() && row(&t, 0).is_empty(),
        "RIS was swallowed"
    );
}

#[test]
fn a_soft_reset_keeps_the_screen() {
    let mut t = Terminal::new(40, 6);
    t.feed(b"\x1b[?1002h\x1b[?2004h\x1b[?25l");
    t.feed(b"text that stays");

    t.feed(b"\x1b[!p");

    assert!(!t.bracketed_paste_mode(), "DECSTR clears the modes");
    assert_eq!(t.mouse_tracking_mode(), MouseTrackingMode::Off);
    assert!(t.cursor_visible());
    assert_eq!(
        row(&t, 0),
        "text that stays",
        "DECSTR is the reset that does not touch the screen"
    );
}

#[test]
fn a_soft_reset_leaves_the_tab_stops_alone() {
    // DEC STD 070 gives tab stops to RIS, not DECSTR: a program that
    // soft-resets mid-table would otherwise lose its columns.
    let mut t = Terminal::new(40, 6);
    t.feed(b"\x1b[3G\x1bH"); // a stop at column 2
    t.feed(b"\x1b[!p");
    t.feed(b"\r\x1b[2K");
    t.feed(b"a\tb");
    assert_eq!(row(&t, 0), "a b", "the stop at column 2 survived the soft reset");

    t.feed(b"\x1bc");
    t.feed(b"a\tb");
    assert_eq!(row(&t, 0), "a       b", "RIS put the stops back to every eighth column");
}

#[test]
fn erase_display_3_drops_the_scrollback_and_keeps_the_screen() {
    let mut t = Terminal::new(40, 3);
    for i in 0..10 {
        t.feed(format!("line {i}\r\n").as_bytes());
    }
    assert!(t.grid().scrollback_len() > 0, "the fixture needs scrollback");
    let visible: Vec<String> = (0..3).map(|r| row(&t, r)).collect();

    t.feed(b"\x1b[3J");

    assert_eq!(t.grid().scrollback_len(), 0, "ED 3 drops the scrollback");
    let after: Vec<String> = (0..3).map(|r| row(&t, r)).collect();
    assert_eq!(after, visible, "ED 3 does not touch what is on screen");
}

//! What a saved cursor carries, and who shares the slot with it.
//!
//! DECSC saves more than a position.  The item that kept getting lost
//! here is the character set: a program that designates the DEC
//! line-drawing set, draws a frame, and restores was coming back with
//! line-drawing still on, so every letter it printed afterwards arrived
//! as a box part.
//!
//! The alternate screen is the same story told twice, because `?1049`
//! is defined as DECSC, the switch, and DECRC -- one saved-cursor slot,
//! not two.

use marspot_term::terminal::Terminal;

fn row(t: &Terminal, r: u16) -> String {
    let cols = t.grid().cols();
    (0..cols).map(|c| t.grid().cell(c, r).ch).collect::<String>().trim_end().to_string()
}

const GFX: &[u8] = b"\x1b(0"; // designate DEC special graphics as G0
const ASCII: &[u8] = b"\x1b(B";

#[test]
fn decrc_brings_back_the_charset_decsc_saw() {
    let mut t = Terminal::new(20, 3);
    t.feed(b"\x1b7"); // saved while plain
    t.feed(GFX);
    t.feed(b"qqq"); // drawn as a horizontal rule
    assert_eq!(row(&t, 0), "\u{2500}\u{2500}\u{2500}");
    t.feed(b"\x1b8"); // restore: back to plain
    t.feed(b"qqq");
    assert_eq!(row(&t, 0), "qqq", "the restore has to undo the charset too");
}

#[test]
fn decsc_saves_the_charset_that_is_current() {
    let mut t = Terminal::new(20, 3);
    t.feed(GFX);
    t.feed(b"\x1b7"); // saved while in graphics
    t.feed(ASCII);
    t.feed(b"\x1b8");
    t.feed(b"qqq");
    assert_eq!(row(&t, 0), "\u{2500}\u{2500}\u{2500}", "graphics was what was saved");
}

#[test]
fn leaving_the_alternate_screen_hands_the_charset_back() {
    let mut t = Terminal::new(20, 3);
    t.feed(b"\x1b[?1049h"); // TUI takes over
    t.feed(GFX);
    t.feed(b"qqq"); // draws its frame
    t.feed(b"\x1b[?1049l"); // and exits
    t.feed(b"qqq");
    assert_eq!(row(&t, 0), "qqq", "the shell must not inherit the TUI's charset");
}

/// One slot. xterm defines `?1049` as "save cursor as in DECSC", so
/// entering the alternate screen replaces whatever `ESC 7` had put
/// there rather than saving beside it.
#[test]
fn the_alternate_screen_shares_the_saved_cursor_slot() {
    let mut t = Terminal::new(20, 3);
    t.feed(b"\x1b7"); // saved while plain
    t.feed(GFX);
    t.feed(b"\x1b[?1049h"); // saves again -- in graphics this time
    t.feed(b"\x1b[?1049l");
    t.feed(b"\x1b8");
    t.feed(b"qqq");
    assert_eq!(row(&t, 0), "\u{2500}\u{2500}\u{2500}", "the later save is the one that survives");
}

/// `?1048` is DECSC and DECRC under another number.
#[test]
fn mode_1048_is_the_save_and_restore_on_their_own() {
    let mut t = Terminal::new(20, 3);
    t.feed(b"\x1b[2;5H\x1b[?1048h");
    t.feed(b"\x1b[1;1H");
    t.feed(b"\x1b[?1048l");
    assert_eq!(t.grid().cursor(), (4, 1));
    assert!(!t.in_alt_screen(), "1048 is not a buffer switch");
}

/// A pane that is replaced mid-frame comes back drawing the same set.
#[test]
fn the_charset_survives_a_snapshot() {
    let mut t = Terminal::new(20, 3);
    t.feed(b"\x1b7"); // a saved cursor holding plain
    t.feed(GFX);
    let bytes = t.serialize_snapshot();

    let mut back = Terminal::new(20, 3);
    back.apply_snapshot(&bytes).unwrap();
    back.feed(b"qqq");
    assert_eq!(row(&back, 0), "\u{2500}\u{2500}\u{2500}", "the live charset came back");
    back.feed(b"\x1b8");
    back.feed(b"qqq");
    assert_eq!(row(&back, 0), "qqq", "and so did the one the saved cursor held");
}

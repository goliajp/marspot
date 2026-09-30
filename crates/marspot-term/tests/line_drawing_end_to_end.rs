//! DEC Special Graphics (`ESC ( 0`), through the parser.
//!
//! A great many TUIs draw their frames by switching G0 to the special
//! graphics set and printing letters: `lqqqk` is the top of a box.
//! The designation used to be a no-op, so the letters went to the
//! screen as letters and the frame came out as `lqqqk`.

use marspot_term::terminal::Terminal;

fn row(t: &Terminal, r: u16) -> String {
    let cols = t.grid().cols();
    (0..cols).map(|c| t.grid().cell(c, r).ch).collect::<String>().trim_end().to_string()
}

#[test]
fn a_box_comes_out_as_a_box() {
    let mut t = Terminal::new(20, 3);
    t.feed(b"\x1b(0lqqqk\x1b(B");
    assert_eq!(row(&t, 0), "┌───┐");
}

#[test]
fn the_letters_used_to_reach_the_screen() {
    // The regression this exists for.
    let mut t = Terminal::new(20, 3);
    t.feed(b"\x1b(0lqqqk");
    assert_ne!(row(&t, 0), "lqqqk", "the designation was ignored");
}

#[test]
fn ascii_comes_back_on_esc_paren_b() {
    let mut t = Terminal::new(20, 3);
    t.feed(b"\x1b(0q\x1b(Bq");
    assert_eq!(row(&t, 0), "─q", "the second q is plain ASCII again");
}

#[test]
fn only_the_graphics_range_is_remapped() {
    // Digits and capitals are the same characters in both sets; a
    // table that remapped them would corrupt any text printed while
    // the set is active.
    let mut t = Terminal::new(20, 3);
    t.feed(b"\x1b(0AZ09\x1b(B");
    assert_eq!(row(&t, 0), "AZ09");
}

#[test]
fn a_reset_puts_the_charset_back() {
    let mut t = Terminal::new(20, 3);
    t.feed(b"\x1b(0");
    t.feed(b"\x1bc");
    t.feed(b"q");
    assert_eq!(row(&t, 0), "q", "RIS left G0 in the graphics set");
}

/// Every letter in the set maps to the character the set says it does.
///
/// The table replaced a match, and a table is the shape where an
/// off-by-one slides every glyph along by one without failing to
/// compile.  This walks the whole range against the values DEC
/// published.
#[test]
fn the_whole_table_is_what_dec_published() {
    const EXPECTED: &[(u8, char)] = &[
        (b'_', ' '), (b'`', '\u{25c6}'), (b'a', '\u{2592}'), (b'b', '\u{2409}'),
        (b'c', '\u{240c}'), (b'd', '\u{240d}'), (b'e', '\u{240a}'), (b'f', '\u{00b0}'),
        (b'g', '\u{00b1}'), (b'h', '\u{2424}'), (b'i', '\u{240b}'), (b'j', '\u{2518}'),
        (b'k', '\u{2510}'), (b'l', '\u{250c}'), (b'm', '\u{2514}'), (b'n', '\u{253c}'),
        (b'o', '\u{23ba}'), (b'p', '\u{23bb}'), (b'q', '\u{2500}'), (b'r', '\u{23bc}'),
        (b's', '\u{23bd}'), (b't', '\u{251c}'), (b'u', '\u{2524}'), (b'v', '\u{2534}'),
        (b'w', '\u{252c}'), (b'x', '\u{2502}'), (b'y', '\u{2264}'), (b'z', '\u{2265}'),
        (b'{', '\u{03c0}'), (b'|', '\u{2260}'), (b'}', '\u{00a3}'), (b'~', '\u{00b7}'),
    ];
    assert_eq!(EXPECTED.len(), 32, "the set is 0x5F..=0x7E");

    for &(byte, want) in EXPECTED {
        let mut t = Terminal::new(20, 3);
        t.feed(&[0x1b, b'(', b'0', byte]);
        assert_eq!(
            t.grid().cell(0, 0).ch,
            want,
            "{} should draw {want:?}",
            byte as char
        );
    }

    // And the byte below the range is untouched, which is the edge an
    // index gets wrong.
    let mut t = Terminal::new(20, 3);
    t.feed(b"\x1b(0^");
    assert_eq!(t.grid().cell(0, 0).ch, '^', "0x5E is not in the set");
}

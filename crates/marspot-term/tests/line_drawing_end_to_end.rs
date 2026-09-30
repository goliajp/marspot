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

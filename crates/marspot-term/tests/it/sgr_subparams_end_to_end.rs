//! Colon sub-parameters, through the parser, on a real grid.
//!
//! A colon used to land in the CSI catch-all and send the whole
//! sequence to `CsiIgnore`, so one colon anywhere threw away every
//! other parameter with it.  An app asking for a curly underline in
//! red got no styling at all — not a straight underline, none — and
//! the same sequence carrying a foreground colour lost that too.

use marspot_term::grid::Color;
use marspot_term::terminal::Terminal;

fn cell_at(t: &Terminal, col: u16, row: u16) -> marspot_term::grid::Cell {
    t.grid().cell(col, row)
}

#[test]
fn a_colon_no_longer_discards_the_whole_sequence() {
    // The regression this exists for: bold, then a curly underline,
    // then the letter.  The colon used to take the bold with it.
    let mut t = Terminal::new(20, 3);
    t.feed(b"\x1b[1;4:3mX");
    let c = cell_at(&t, 0, 0);
    assert_eq!(c.ch, 'X');
    assert!(c.attrs.bold, "the colon threw away the bold in front of it");
    assert!(c.attrs.underline, "4:3 is an underline");
}

#[test]
fn underline_style_zero_turns_it_off() {
    let mut t = Terminal::new(20, 3);
    t.feed(b"\x1b[4mA\x1b[4:0mB");
    assert!(cell_at(&t, 0, 0).attrs.underline);
    assert!(!cell_at(&t, 1, 0).attrs.underline, "4:0 is the off form");
}

#[test]
fn a_direct_colour_arrives_in_both_colon_forms() {
    // `38:2::r:g:b` carries an empty colour-space id; `38:2:r:g:b`
    // omits it.  Both are in the wild and both mean this red.
    let red = Color::rgb(220, 30, 40);

    let mut t = Terminal::new(20, 3);
    t.feed(b"\x1b[38:2::220:30:40mA");
    assert_eq!(cell_at(&t, 0, 0).attrs.fg, red, "with the empty id");

    let mut t = Terminal::new(20, 3);
    t.feed(b"\x1b[38:2:220:30:40mA");
    assert_eq!(cell_at(&t, 0, 0).attrs.fg, red, "without it");
}

#[test]
fn the_semicolon_forms_are_untouched() {
    // Every sequence that worked before has no colons in it, so every
    // parameter is its own group and nothing changed.
    let mut t = Terminal::new(20, 3);
    t.feed(b"\x1b[1;38;2;10;20;30;48;5;99mA");
    let c = cell_at(&t, 0, 0);
    assert!(c.attrs.bold);
    assert_eq!(c.attrs.fg, Color::rgb(10, 20, 30));
    assert_eq!(c.attrs.bg, Color::indexed(99));
}

#[test]
fn a_palette_colour_arrives_by_colon() {
    let mut t = Terminal::new(20, 3);
    t.feed(b"\x1b[38:5:214mA");
    assert_eq!(cell_at(&t, 0, 0).attrs.fg, Color::indexed(214));
}

#[test]
fn an_underline_colour_is_skipped_whole() {
    // 58 colours the underline, which this terminal has no attribute
    // for.  What it must not do is let its numbers fall through and
    // set the foreground instead.
    let mut t = Terminal::new(20, 3);
    t.feed(b"\x1b[38;5;9m\x1b[58:2::0:255:0mA");
    assert_eq!(
        cell_at(&t, 0, 0).attrs.fg,
        Color::indexed(9),
        "the underline colour leaked into the foreground"
    );
}

//! IRM: a printed glyph makes room for itself.
//!
//! `CSI 4 h` was reaching nothing at all -- the whole non-private SM /
//! RM family was unhandled -- so a program inserting into the middle of
//! a line overwrote what was there. vttest says its top line should
//! read `A*** ... ***B`; the B was being painted over.
//!
//! The case worth keeping an eye on is a run of printable bytes
//! arriving together. Those take a bulk lane that writes straight into
//! consecutive cells, and the first version of this fix did not turn
//! that lane off -- so inserting one character at a time worked and
//! inserting a word did not.

use marspot_term::terminal::Terminal;

fn row(t: &Terminal, r: u16) -> String {
    let cols = t.grid().cols();
    (0..cols).map(|c| t.grid().cell(c, r).ch).collect::<String>()
}

fn filled() -> Terminal {
    let mut t = Terminal::new(12, 2);
    t.feed(b"ABCDEFGHIJKL");
    t
}

#[test]
fn off_by_default_a_glyph_takes_the_cell_it_lands_on() {
    let mut t = filled();
    t.feed(b"\x1b[1;3Hx");
    assert_eq!(row(&t, 0), "ABxDEFGHIJKL");
}

#[test]
fn on_a_glyph_pushes_the_rest_of_the_line_along() {
    let mut t = filled();
    t.feed(b"\x1b[1;3H\x1b[4hx");
    assert_eq!(row(&t, 0), "ABxCDEFGHIJK", "the L fell off the end");
}

/// The lane that caught the first attempt out.
#[test]
fn a_whole_run_arriving_at_once_inserts_too() {
    let mut t = filled();
    t.feed(b"\x1b[1;3H\x1b[4hxyz");
    assert_eq!(row(&t, 0), "ABxyzCDEFGHI");
}

#[test]
fn a_run_split_across_reads_gives_the_same_line() {
    let mut whole = filled();
    whole.feed(b"\x1b[1;3H\x1b[4hxyz");
    let mut split = filled();
    split.feed(b"\x1b[1;3H\x1b[4hx");
    split.feed(b"y");
    split.feed(b"z");
    assert_eq!(row(&split, 0), row(&whole, 0));
}

#[test]
fn reset_puts_it_back() {
    let mut t = filled();
    t.feed(b"\x1b[1;3H\x1b[4h\x1b[4lx");
    assert_eq!(row(&t, 0), "ABxDEFGHIJKL");
}

#[test]
fn a_soft_reset_turns_it_off() {
    let mut t = filled();
    t.feed(b"\x1b[4h\x1b[!p");
    t.feed(b"\x1b[1;3Hx");
    assert_eq!(row(&t, 0), "ABxDEFGHIJKL");
}

/// A wide glyph makes room for both of its cells.
#[test]
fn a_wide_glyph_pushes_by_two() {
    let mut t = filled();
    t.feed("\x1b[1;3H\x1b[4h世".as_bytes());
    let r = row(&t, 0);
    assert_eq!(r.chars().next(), Some('A'));
    assert_eq!(r.chars().nth(2), Some('世'));
    assert_eq!(r.chars().nth(4), Some('C'), "C was pushed two cells, not one");
}

/// A pane replaced mid-insert comes back inserting.
#[test]
fn the_mode_survives_a_snapshot() {
    let mut t = filled();
    t.feed(b"\x1b[4h");
    let bytes = t.serialize_snapshot();
    let mut back = Terminal::new(12, 2);
    back.apply_snapshot(&bytes).unwrap();
    back.feed(b"\x1b[1;3Hx");
    assert_eq!(row(&back, 0), "ABxCDEFGHIJK");
}

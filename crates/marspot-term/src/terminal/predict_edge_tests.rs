//! Local echo at the right edge of the screen.

use super::Terminal;

fn row(t: &Terminal, r: u16) -> String {
    (0..t.grid().cols()).map(|c| t.grid().cell(c, r).ch).collect::<String>().trim_end().to_string()
}

/// zsh with a prompt one column short of the width: a key typed into
/// the last column is echoed as `key`, a space that the wrap carries to
/// the next row, and `\r\e[K` there.  The line with the prompt and the
/// key stays.
#[test]
fn a_key_typed_into_the_last_column_keeps_its_line() {
    let mut t = Terminal::new(10, 3);
    t.feed(b"prompt%> ");
    assert_eq!(t.grid().cursor(), (9, 0));
    t.predict_byte(b'q');
    t.feed(b"q \r\x1b[K");
    assert_eq!(row(&t, 0), "prompt%> q");
    assert_eq!(t.grid().cursor(), (0, 1));
}

/// A key typed while the wrap is pending lands at the start of the
/// next row, not over the last cell of this one.
#[test]
fn a_key_typed_on_a_pending_wrap_lands_on_the_next_row() {
    let mut t = Terminal::new(10, 3);
    t.feed(b"0123456789");
    t.predict_byte(b'q');
    t.feed(b"q");
    assert_eq!(row(&t, 0), "0123456789");
    assert_eq!(row(&t, 1), "q");
    assert_eq!(t.grid().cursor(), (1, 1));
}

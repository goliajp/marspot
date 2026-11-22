//! A click on an OSC 8 link, driven as far as what `open(1)` is handed.
//!
//! The click handler takes the span the hit-test returns and opens its
//! target, or its text when it has none; this does the same with the
//! span `osc8::link_at` returns, which is what the hit-test returns for
//! a declared link.

use marspot::link_open::open_arg_for;
use marspot::terminal::Terminal;

fn opened(t: &Terminal, col: u16, row: u16) -> Option<String> {
    let link = marspot::osc8::link_at(t.grid(), 0, col, row)?;
    open_arg_for(link.kind, link.target.as_deref().unwrap_or(&link.text))
}

#[test]
fn osc8_a_click_on_the_text_opens_the_uri_the_program_declared() {
    let mut t = Terminal::new(40, 3);
    t.feed(b"\x1b]8;;https://example.com\x1b\\Click me\x1b]8;;\x1b\\\n");
    for col in 0..8 {
        assert_eq!(opened(&t, col, 0).as_deref(), Some("https://example.com"), "col {col}");
    }
    assert_eq!(opened(&t, 8, 0), None);
}

/// `ls --hyperlink` names files as `file://<host>/<path>`; `open(1)` is
/// handed the path, unescaped.  `mailto:` goes to the mail client.
#[test]
fn osc8_file_and_mailto_links_open_what_they_name() {
    let mut t = Terminal::new(40, 3);
    t.feed(b"\x1b]8;;file://studio.local/tmp/a%20b.txt\x1b\\a b.txt\x1b]8;;\x1b\\ ");
    t.feed(b"\x1b]8;;file:///tmp/x\x1b\\x\x1b]8;;\x1b\\ ");
    t.feed(b"\x1b]8;;mailto:someone@example.com\x1b\\mail\x1b]8;;\x1b\\");
    assert_eq!(opened(&t, 0, 0).as_deref(), Some("/tmp/a b.txt"));
    assert_eq!(opened(&t, 8, 0).as_deref(), Some("/tmp/x"));
    assert_eq!(opened(&t, 10, 0).as_deref(), Some("mailto:someone@example.com"));
}

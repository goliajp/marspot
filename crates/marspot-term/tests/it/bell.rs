//! BEL, through the parser: a bare one rings, and the one that ends an
//! OSC string does not.

use marspot_term::terminal::Terminal;

fn rang(bytes: &[&[u8]]) -> bool {
    let mut t = Terminal::new(40, 4);
    for b in bytes {
        t.feed(b);
    }
    t.take_bell()
}

#[test]
fn a_bare_bel_rings_and_changes_nothing_on_screen() {
    let mut t = Terminal::new(40, 4);
    t.feed(b"ab\x07cd");
    assert!(t.take_bell());
    assert_eq!(t.grid().cursor(), (4, 0));
    let text: String = (0..4).map(|c| t.grid().cell(c, 0).ch).collect();
    assert_eq!(text, "abcd");
    assert!(t.take_response().is_empty());
}

#[test]
fn any_number_of_bels_before_a_take_is_one_ring() {
    let mut t = Terminal::new(40, 4);
    t.feed(b"\x07\x07x\x07");
    t.feed(b"\x07");
    assert!(t.take_bell());
    assert!(!t.take_bell(), "taking it clears it");
    t.feed(b"\x07");
    assert!(t.take_bell(), "and the next one rings again");
}

#[test]
fn no_bel_no_ring() {
    assert!(!rang(&[b"plain text\r\n\x1b[1mbold\x1b[0m"]));
}

#[test]
fn the_bel_that_ends_an_osc_string_does_not_ring() {
    assert!(!rang(&[b"\x1b]2;a title\x07"]));
    assert!(!rang(&[b"\x1b]52;c;aGVsbG8=\x07"]));
    assert!(!rang(&[b"\x1b]8;;https://example.com\x07link\x1b]8;;\x07"]));
    // the terminator in a read of its own
    assert!(!rang(&[b"\x1b]0;split", b"\x07"]));
}

#[test]
fn a_bel_after_an_osc_string_rings() {
    assert!(rang(&[b"\x1b]2;t\x07\x07"]));
    assert!(rang(&[b"\x1b]2;t\x1b\\\x07"]));
}

#[test]
fn a_bel_inside_an_ignored_string_does_not_ring() {
    // APC and PM bodies are dropped whole until ST
    assert!(!rang(&[b"\x1b_body\x07more\x1b\\"]));
    assert!(!rang(&[b"\x1b^body\x07\x1b\\"]));
}

#[test]
fn a_bel_inside_a_control_sequence_is_executed_as_xterm_does() {
    // a C0 control in the middle of a CSI is executed, and the
    // sequence goes on
    let mut t = Terminal::new(40, 4);
    t.feed(b"\x1b[1\x07mx");
    assert!(t.take_bell());
    assert!(t.grid().cell(0, 0).attrs.bold);
}

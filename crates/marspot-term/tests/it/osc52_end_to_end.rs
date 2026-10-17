//! OSC 52, through the parser.
//!
//! A program says "put this on the clipboard" by sending its text
//! base64-encoded.  Yanking in vim over ssh is the case that matters:
//! nothing but the terminal can carry that back to the machine the
//! user is sitting at.

use marspot_term::terminal::Terminal;

#[test]
fn a_program_can_put_text_on_the_clipboard() {
    let mut t = Terminal::new(40, 4);
    // "hello" — what `printf '\033]52;c;aGVsbG8=\007'` sends.
    t.feed(b"\x1b]52;c;aGVsbG8=\x07");
    assert_eq!(t.take_osc_clipboard().as_deref(), Some("hello"));
}

#[test]
fn taking_it_clears_it() {
    let mut t = Terminal::new(40, 4);
    t.feed(b"\x1b]52;c;aGVsbG8=\x07");
    assert!(t.take_osc_clipboard().is_some());
    assert!(t.take_osc_clipboard().is_none(), "a clipboard write happens once");
}

#[test]
fn the_terminator_can_be_st_as_well_as_bel() {
    let mut t = Terminal::new(40, 4);
    t.feed(b"\x1b]52;c;aGVsbG8=\x1b\\");
    assert_eq!(t.take_osc_clipboard().as_deref(), Some("hello"));
}

/// A read is refused, on purpose.
///
/// Answering `52 ; c ; ?` hands any program that can write to this pty
/// whatever the user last copied.  Staying quiet reads as a terminal
/// without the feature, which is the right amount to say.
#[test]
fn a_read_is_refused_and_says_nothing() {
    let mut t = Terminal::new(40, 4);
    t.feed(b"\x1b]52;c;?\x07");
    assert!(t.take_osc_clipboard().is_none());
    assert!(t.take_response().is_empty(), "a reply is the whole problem");
}

#[test]
fn a_payload_that_is_not_base64_is_dropped() {
    let mut t = Terminal::new(40, 4);
    t.feed(b"\x1b]52;c;not valid base64!!\x07");
    assert!(t.take_osc_clipboard().is_none());
}

/// Over the cap it is dropped whole rather than truncated: half of
/// what was copied is worse than none, because nothing says which half.
#[test]
fn an_oversized_payload_is_dropped_whole() {
    let mut t = Terminal::new(40, 4);
    let big = "A".repeat(4 * 40_000); // decodes to ~120 KB, over the 64 KiB cap
    t.feed(format!("\x1b]52;c;{big}\x07").as_bytes());
    assert!(t.take_osc_clipboard().is_none());
}

/// The targets field is whatever the program chose; the text is the
/// part after the second semicolon either way.
#[test]
fn the_targets_field_does_not_change_the_text() {
    for targets in ["c", "p", "s0", ""] {
        let mut t = Terminal::new(40, 4);
        t.feed(format!("\x1b]52;{targets};aGVsbG8=\x07").as_bytes());
        assert_eq!(
            t.take_osc_clipboard().as_deref(),
            Some("hello"),
            "targets {targets:?}"
        );
    }
}

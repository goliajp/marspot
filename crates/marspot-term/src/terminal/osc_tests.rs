use super::*;

#[test]
fn the_shape_a_program_asks_for_is_recorded_not_dropped() {
    let mut t = Terminal::new(20, 5);
    assert_eq!(t.cursor_shape(), 0);
    t.feed(b"\x1b[5 q");
    assert_eq!(t.cursor_shape(), 5, "bar");
    // What codex actually sends, 119,812 times in one session:
    // shape 0 = "this terminal's default".
    t.feed(b"\x1b[0 q");
    assert_eq!(t.cursor_shape(), 0);
    // `CSI > 0 q` is XTQVERSION, a different sequence that happens
    // to end in the same letter — it must still answer.
    t.feed(b"\x1b[>0q");
    assert!(!t.take_response().is_empty());
    assert_eq!(t.cursor_shape(), 0, "XTQVERSION is not a shape");
}

#[test]
fn a_sequence_with_an_intermediate_is_still_answered() {
    // csi_dispatch returns early for any CSI carrying an
    // intermediate, so an arm written below that point never runs.
    // XTQVERSION sat there dead while the comment beside it said it
    // replied; this is the test that would have caught it.
    let mut t = Terminal::new(20, 5);
    t.feed(b"\x1b[>0q");
    assert_eq!(
        String::from_utf8(t.take_response()).unwrap(),
        "\x1bP>|marspot\x1b\\",
        "XTQVERSION must answer"
    );
}

#[test]
fn a_color_query_gets_an_answer() {
    // An unanswered one is how a TUI ends up guessing whether it
    // is drawing on a dark terminal.
    let mut t = Terminal::new(20, 5);
    t.feed(b"\x1b]11;?\x07");
    let r = String::from_utf8(t.take_response()).unwrap();
    assert_eq!(r, format!("\x1b]11;{}\x07", crate::palette::xterm_rgb(crate::palette::DARK.bg)));

    t.feed(b"\x1b]10;?\x07");
    let r = String::from_utf8(t.take_response()).unwrap();
    assert!(r.starts_with("\x1b]10;rgb:"), "the number asked is the number answered: {r:?}");
}

#[test]
fn setting_a_color_is_not_a_query_and_gets_no_reply() {
    let mut t = Terminal::new(20, 5);
    t.feed(b"\x1b]11;rgb:0000/0000/0000\x07");
    assert!(t.take_response().is_empty());
}

#[test]
fn a_program_that_says_what_it_is_is_not_thrown_away() {
    let mut t = Terminal::new(20, 5);
    assert_eq!(t.osc_title(), "");
    t.feed(b"\x1b]0;codex \xe2\xa0\x99 building\x07");
    assert_eq!(t.osc_title(), "codex ⠙ building");
    // OSC 2 is the same title by another number.
    t.feed(b"\x1b]2;second\x07");
    assert_eq!(t.osc_title(), "second");
    // None of it reaches the screen.
    let row: String = (0..20).map(|c| t.grid().cell(c, 0).ch).collect();
    assert_eq!(row.trim_end_matches('\0').trim(), "");
}

#[test]
fn a_title_is_a_label_not_a_buffer() {
    let mut t = Terminal::new(20, 5);
    let mut b = b"\x1b]0;".to_vec();
    b.extend(std::iter::repeat_n(b'x', 10_000));
    b.push(0x07);
    t.feed(&b);
    assert_eq!(t.osc_title().chars().count(), 256);
}

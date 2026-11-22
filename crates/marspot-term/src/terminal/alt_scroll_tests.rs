use super::Terminal;

/// codex opens its transcript with `?1049h · ?1007h` and closes it
/// with `?1007l · ?1049l`.  That pair is the program stating, in
/// the standard way, that the wheel is the arrow keys here — the
/// thing three rounds of reading its headings off the screen were
/// trying to work out.
#[test]
fn the_transcript_sequence_turns_the_wheel_into_arrow_keys() {
    let mut t = Terminal::new(20, 4);
    assert!(!t.alt_scroll_mode());
    t.feed(b"\x1b[?1049h\x1b[?1007h");
    assert!(t.alt_scroll_mode());
    t.feed(b"\x1b[?1007l\x1b[?1049l");
    assert!(!t.alt_scroll_mode());
}

/// The mode belongs to a full-screen view.  A program that set it
/// and left the alternate screen without clearing it must not keep
/// the wheel away from the scrollback the user can actually see.
#[test]
fn it_does_not_apply_on_the_main_screen() {
    let mut t = Terminal::new(20, 4);
    t.feed(b"\x1b[?1049h\x1b[?1007h");
    assert!(t.alt_scroll_mode());
    t.feed(b"\x1b[?1049l");
    assert!(
        !t.alt_scroll_mode(),
        "no alternate screen, no claim on the wheel"
    );
}

/// It has to survive an image swap.  The program said it once, on
/// entering a view it is still in, and will not say it again — and
/// a pane whose plugin enters with a TOGGLE would then press that
/// toggle on an open view and shut it.
#[test]
fn it_survives_an_execv() {
    let mut before = Terminal::new(20, 4);
    before.feed(b"\x1b[?1049h\x1b[?1007h");
    assert!(before.in_alt_screen() && before.alt_scroll_mode());
    let snap = before.serialize_snapshot_live();
    let mut after = Terminal::new(20, 4);
    after.apply_snapshot(&snap).expect("snapshot applies");
    assert!(
        after.alt_scroll_mode(),
        "the wheel must still reach the program after the swap"
    );
}

/// A dead program's claim on the wheel dies with it.
#[test]
fn a_process_handover_gives_the_wheel_back() {
    let mut t = Terminal::new(20, 4);
    t.feed(b"\x1b[?1049h\x1b[?1007h");
    t.reset_process_owned_modes();
    assert!(!t.alt_scroll_mode());
}

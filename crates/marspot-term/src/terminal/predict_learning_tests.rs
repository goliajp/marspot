use super::Terminal;

/// A program that echoes at the cursor keeps the feature.
/// Measured at a real zsh prompt: 16 keystrokes, 15 hits.
#[test]
fn a_program_that_echoes_keeps_predicting() {
    let mut t = Terminal::new(40, 4);
    for c in b"echo hi" {
        assert!(t.predict_byte(*c), "prediction offered");
        t.feed(&[*c]); // the echo confirms it
    }
    assert!(t.can_predict(), "nothing here has gone wrong");
    assert_eq!(t.predictions_miss, 0);
}

/// A program that draws its own input somewhere else never
/// confirms, and the terminal stops guessing rather than putting a
/// character where the user is not looking.  Measured inside
/// codex: 23 keystrokes, 0 hits.
#[test]
fn a_program_that_never_echoes_is_given_up_on() {
    let mut t = Terminal::new(40, 4);
    for _ in 0..Terminal::PREDICT_GIVE_UP_AFTER {
        assert!(t.can_predict());
        assert!(t.predict_byte(b'a'));
        t.feed(b"X"); // a repaint, not an echo
    }
    assert!(
        !t.can_predict(),
        "three wasted keystrokes is the whole budget for finding this out"
    );
}

/// Giving up is not forever: the pane a program was quit in
/// becomes a shell again, and nothing announces that.
#[test]
fn one_keystroke_in_a_while_asks_again() {
    let mut t = Terminal::new(40, 4);
    for _ in 0..Terminal::PREDICT_GIVE_UP_AFTER {
        t.predict_byte(b'a');
        t.feed(b"X");
    }
    assert!(!t.can_predict());
    // Each declined keystroke counts toward the next probe, and
    // the one that takes the count TO the threshold is itself
    // declined — the probe is the keystroke after it.
    for _ in 0..Terminal::PREDICT_PROBE_EVERY {
        assert!(!t.predict_byte(b'b'), "still declined");
    }
    assert!(t.predict_byte(b'b'), "one probe is let through");
    t.feed(b"b"); // this program echoes now
    assert!(t.can_predict(), "and a hit hands the feature back");
}

/// The probe is spent whether or not it teaches anything, so a
/// program that still does not echo costs one stray character per
/// PREDICT_PROBE_EVERY keystrokes and not one per keystroke.
#[test]
fn a_spent_probe_does_not_repeat_immediately() {
    let mut t = Terminal::new(40, 4);
    for _ in 0..Terminal::PREDICT_GIVE_UP_AFTER {
        t.predict_byte(b'a');
        t.feed(b"X");
    }
    for _ in 0..=Terminal::PREDICT_PROBE_EVERY {
        t.predict_byte(b'b'); // the last of these is the probe
    }
    t.feed(b"X"); // the probe missed too
    assert!(
        !t.predict_byte(b'c'),
        "the next keystroke is not another probe"
    );
}

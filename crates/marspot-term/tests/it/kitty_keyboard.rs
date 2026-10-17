//! The kitty keyboard protocol, end to end: what a program asks for,
//! what it gets back, and what a keystroke then looks like.
//!
//! The reason this protocol is worth having is one key.  In the legacy
//! encoding `enter`, `shift+enter`, `ctrl+enter` and `alt+enter` are
//! all `\r`, so a program that wants "newline" on one and "send" on
//! the other cannot tell them apart — every CLI that takes multi-line
//! input has a workaround for this, and marspot had one of its own
//! (`shift+enter` sent `\n`, which is a guess about what the program
//! meant).  Under the protocol they are four distinct sequences and
//! nobody has to guess.
//!
//! Only the "disambiguate escape codes" flag is claimed, so the rest
//! of the table is about what raising it must NOT change: ordinary
//! typing, IME text, and the three keys the spec keeps on their legacy
//! bytes so a shell stays usable after a program dies with the
//! protocol still on.
use marspot_term::input_core::{
    key_event_to_bytes, KeyState, LogicalKey, MarspotKeyEvent, Modifiers, NamedKey, TermModes,
};
use marspot_term::terminal::Terminal;

fn press(logical: LogicalKey, text: Option<&str>) -> MarspotKeyEvent {
    MarspotKeyEvent {
        state: KeyState::Pressed,
        logical,
        text: text.map(str::to_owned),
        ..Default::default()
    }
}

/// The same key with a US-layout position attached — what AppKit
/// gives us for any key in the ANSI block.
fn press_at(logical: LogicalKey, text: Option<&str>, base: char) -> MarspotKeyEvent {
    MarspotKeyEvent {
        base_layout: Some(base),
        ..press(logical, text)
    }
}

fn release(logical: LogicalKey) -> MarspotKeyEvent {
    MarspotKeyEvent {
        state: KeyState::Released,
        logical,
        ..Default::default()
    }
}

/// Encode with a chosen set of flags raised.
fn with_flags(ev: &MarspotKeyEvent, m: Modifiers, flags: u8) -> Option<String> {
    let modes = TermModes { kitty_keyboard: flags, ..TermModes::default() };
    key_event_to_bytes(ev, m, modes, || None).map(|b| String::from_utf8_lossy(&b).into_owned())
}

fn named(n: NamedKey) -> MarspotKeyEvent {
    press(LogicalKey::Named(n), None)
}

fn mods(spec: &str) -> Modifiers {
    Modifiers {
        shift: spec.contains('s'),
        control: spec.contains('c'),
        alt: spec.contains('a'),
        super_: spec.contains('m'),
    }
}

/// Encode with the protocol raised (disambiguate only).
fn kitty(ev: &MarspotKeyEvent, m: Modifiers) -> Option<String> {
    let modes = TermModes { kitty_keyboard: 1, ..TermModes::default() };
    key_event_to_bytes(ev, m, modes, || None)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Encode with it down — what every program that never asks gets.
fn legacy(ev: &MarspotKeyEvent, m: Modifiers) -> Option<String> {
    key_event_to_bytes(ev, m, TermModes::default(), || None)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

#[test]
fn the_enter_family_stops_being_one_byte() {
    // The whole reason for the protocol, in four rows.
    assert_eq!(legacy(&named(NamedKey::Enter), mods("")).as_deref(), Some("\r"));
    assert_eq!(legacy(&named(NamedKey::Enter), mods("c")).as_deref(), Some("\r"));
    assert_eq!(legacy(&named(NamedKey::Enter), mods("a")).as_deref(), Some("\r"));

    assert_eq!(kitty(&named(NamedKey::Enter), mods("")).as_deref(), Some("\r"));
    assert_eq!(kitty(&named(NamedKey::Enter), mods("s")).as_deref(), Some("\x1b[13;2u"));
    assert_eq!(kitty(&named(NamedKey::Enter), mods("a")).as_deref(), Some("\x1b[13;3u"));
    assert_eq!(kitty(&named(NamedKey::Enter), mods("c")).as_deref(), Some("\x1b[13;5u"));
    assert_eq!(kitty(&named(NamedKey::Enter), mods("cs")).as_deref(), Some("\x1b[13;6u"));
}

#[test]
fn raising_the_protocol_does_not_change_typing() {
    // The failure this guards against is the loud one: a program
    // raises the protocol and the user can no longer type.
    for (ch, text) in [('a', "a"), ('z', "z"), ('1', "1"), ('-', "-")] {
        let ev = press(LogicalKey::Char(ch), Some(text));
        assert_eq!(kitty(&ev, mods("")).as_deref(), Some(text), "{ch}");
    }
    // Shift is spent producing the capital, so it is not reported.
    let shifted = press(LogicalKey::Char('a'), Some("A"));
    assert_eq!(kitty(&shifted, mods("s")).as_deref(), Some("A"));
    // IME and dead-key output arrive with no logical key at all.
    let composed = press(LogicalKey::Other, Some("中"));
    assert_eq!(kitty(&composed, mods("")).as_deref(), Some("中"));
}

#[test]
fn a_shell_stays_usable_if_a_program_dies_with_it_on() {
    // The spec keeps Enter, Tab and Backspace on their legacy bytes
    // when unmodified for exactly this reason: someone has to be able
    // to type `reset`.
    assert_eq!(kitty(&named(NamedKey::Enter), mods("")).as_deref(), Some("\r"));
    assert_eq!(kitty(&named(NamedKey::Tab), mods("")).as_deref(), Some("\t"));
    assert_eq!(kitty(&named(NamedKey::Backspace), mods("")).as_deref(), Some("\x7f"));
    let a = press(LogicalKey::Char('a'), Some("a"));
    assert_eq!(kitty(&a, mods("")).as_deref(), Some("a"));
}

#[test]
fn keys_that_were_indistinguishable_become_distinct() {
    // ctrl+i vs tab, ctrl+m vs enter: the same byte in the legacy
    // encoding, which is why no TUI can bind them separately.
    let i = press(LogicalKey::Char('i'), Some("\u{9}"));
    assert_eq!(legacy(&i, mods("c")).as_deref(), Some("\t"));
    assert_eq!(legacy(&named(NamedKey::Tab), mods("")).as_deref(), Some("\t"));
    assert_eq!(kitty(&i, mods("c")).as_deref(), Some("\x1b[105;5u"));
    assert_eq!(kitty(&named(NamedKey::Tab), mods("")).as_deref(), Some("\t"));

    // Escape stops being a prefix a program has to time out on.
    assert_eq!(legacy(&named(NamedKey::Escape), mods("")).as_deref(), Some("\x1b"));
    assert_eq!(kitty(&named(NamedKey::Escape), mods("")).as_deref(), Some("\x1b[27u"));
}

#[test]
fn the_shift_of_a_letter_is_reported_when_it_is_not_spent() {
    // ctrl+shift+a produces no text, so shift IS a modifier here.
    // The codepoint is the unshifted one — `keyboard_layout` resolves
    // that before the event ever reaches this encoder, which is the
    // contract `LogicalKey::Char` documents.
    let ev = press(LogicalKey::Char('a'), None);
    assert_eq!(kitty(&ev, mods("cs")).as_deref(), Some("\x1b[97;6u"));
}

#[test]
fn functional_keys_keep_their_legacy_finals() {
    // A program that only half-implements the protocol still reads
    // these, which is why the spec keeps the finals.
    assert_eq!(kitty(&named(NamedKey::ArrowUp), mods("")).as_deref(), Some("\x1b[A"));
    assert_eq!(kitty(&named(NamedKey::ArrowUp), mods("s")).as_deref(), Some("\x1b[1;2A"));
    assert_eq!(kitty(&named(NamedKey::Home), mods("c")).as_deref(), Some("\x1b[1;5H"));
    assert_eq!(kitty(&named(NamedKey::PageUp), mods("")).as_deref(), Some("\x1b[5~"));
    assert_eq!(kitty(&named(NamedKey::F1), mods("")).as_deref(), Some("\x1b[P"));
    // F3 is `13~`, not an `R` final — an irregularity in the table.
    assert_eq!(kitty(&named(NamedKey::F3), mods("")).as_deref(), Some("\x1b[13~"));
    assert_eq!(kitty(&named(NamedKey::F5), mods("a")).as_deref(), Some("\x1b[15;3~"));
}

#[test]
fn cmd_still_belongs_to_the_window() {
    // Cmd-T, Cmd-W, Cmd-V are the user's, in every app.  Handing them
    // to the program because it raised a keyboard protocol would take
    // the shortcuts away.
    let t = press(LogicalKey::Char('t'), Some("t"));
    assert_eq!(kitty(&t, mods("m")), None);
}

#[test]
fn a_program_can_raise_the_protocol_and_read_it_back() {
    let mut t = Terminal::new(40, 4);
    assert_eq!(t.kitty_keyboard_flags(), 0);

    // `CSI > 1 u` — push disambiguate.
    t.feed(b"\x1b[>1u");
    assert_eq!(t.kitty_keyboard_flags(), 1);
    t.feed(b"\x1b[?u");
    assert_eq!(t.take_response(), b"\x1b[?1u");

    // `CSI < u` — pop back to where we were.
    t.feed(b"\x1b[<u");
    assert_eq!(t.kitty_keyboard_flags(), 0);
}

#[test]
fn set_replaces_ors_and_clears() {
    let mut t = Terminal::new(40, 4);
    // mode 1 replaces, 2 sets bits, 3 clears them.
    t.feed(b"\x1b[=1;1u");
    assert_eq!(t.kitty_keyboard_flags(), 1);
    t.feed(b"\x1b[=1;3u");
    assert_eq!(t.kitty_keyboard_flags(), 0);
    t.feed(b"\x1b[=1;2u");
    assert_eq!(t.kitty_keyboard_flags(), 1);
    t.feed(b"\x1b[=0;1u");
    assert_eq!(t.kitty_keyboard_flags(), 0);
}

#[test]
fn only_the_flags_we_honour_are_ever_reported() {
    // A program that asks for all five gets told it has three.  The
    // protocol is built on the terminal reporting what it does, and a
    // program encodes for the reply — claiming `report all keys`
    // without implementing it would promise sequences for keys that
    // have none, composed IME text among them.
    let mut t = Terminal::new(40, 4);
    t.feed(b"\x1b[>31u\x1b[?u");
    assert_eq!(t.take_response(), b"\x1b[?7u");
    assert_eq!(t.kitty_keyboard_flags(), 7);
}

#[test]
fn the_two_programs_that_ask_get_what_they_asked_for() {
    // Measured with a pty probe: Claude Code sends `CSI > 5 u` and
    // codex sends `CSI > 7 u`, each right after reading `CSI ? u`.
    // Both are subsets of what is implemented, so both are answered
    // with exactly what they requested and neither has to fall back.
    for (who, ask, want) in [("claude", &b"\x1b[>5u"[..], &b"\x1b[?5u"[..]),
                             ("codex", &b"\x1b[>7u"[..], &b"\x1b[?7u"[..])] {
        let mut t = Terminal::new(40, 4);
        t.feed(ask);
        t.feed(b"\x1b[?u");
        assert_eq!(t.take_response(), want, "{who}");
    }
}

#[test]
fn event_types_name_press_repeat_and_release() {
    // Flag 2.  Without it a held arrow key is indistinguishable from
    // someone pressing it forty times, and a key-up is invisible.
    const F: u8 = 1 | 2;
    let down = named(NamedKey::ArrowUp);
    assert_eq!(with_flags(&down, mods(""), F).as_deref(), Some("\x1b[1;1:1A"));

    let held = MarspotKeyEvent { repeat: true, ..named(NamedKey::ArrowUp) };
    assert_eq!(with_flags(&held, mods(""), F).as_deref(), Some("\x1b[1;1:2A"));

    let up = release(LogicalKey::Named(NamedKey::ArrowUp));
    assert_eq!(with_flags(&up, mods(""), F).as_deref(), Some("\x1b[1;1:3A"));

    // A letter, where the code is written out.
    let a = press(LogicalKey::Char('a'), Some("a"));
    assert_eq!(with_flags(&a, mods("c"), F).as_deref(), Some("\x1b[97;5:1u"));
}

#[test]
fn without_the_event_flag_a_release_is_silent() {
    // And silent is what every program that never asked expects: the
    // legacy encoding has no way to say "key up" at all.
    let up = release(LogicalKey::Named(NamedKey::ArrowUp));
    assert_eq!(with_flags(&up, mods(""), 1), None);
    assert_eq!(legacy(&up, mods("")), None);
}

#[test]
fn the_three_legacy_keys_never_report_a_release() {
    // They have no legacy sequence to carry one, and the spec keeps
    // them on legacy bytes.  Reporting a release for them would send
    // a program a sequence it has no press to pair with.
    const F: u8 = 1 | 2;
    for n in [NamedKey::Enter, NamedKey::Tab, NamedKey::Backspace] {
        assert_eq!(with_flags(&release(LogicalKey::Named(n)), mods(""), F), None, "{n:?}");
    }
}

#[test]
fn alternate_keys_carry_the_shifted_value_and_the_us_position() {
    // Flag 4.  `ctrl+shift+1` on a US layout: the key is `1`, the
    // shifted value is `!`, and the position is `1` — same as the
    // key, so it is not repeated.
    const F: u8 = 1 | 4;
    let one = press_at(LogicalKey::Char('1'), Some("!"), '1');
    assert_eq!(with_flags(&one, mods("cs"), F).as_deref(), Some("[49:33;6u"));

    // Without the flag, the same press is just the key.
    assert_eq!(with_flags(&one, mods("cs"), 1).as_deref(), Some("[49;6u"));
}

#[test]
fn alternate_keys_keep_a_binding_on_the_physical_key() {
    // The reason the base-layout slot exists.  On AZERTY the key
    // where US has `z` types `w`; a program binding ctrl+z to undo
    // reads the second alternate and stays on the same finger.
    const F: u8 = 1 | 4;
    let azerty_w = press_at(LogicalKey::Char('w'), None, 'z');
    // Empty shifted slot, so two colons before the base.
    assert_eq!(with_flags(&azerty_w, mods("c"), F).as_deref(), Some("[119::122;5u"));

    // On US the two agree and nothing extra is written.
    let us_z = press_at(LogicalKey::Char('z'), None, 'z');
    assert_eq!(with_flags(&us_z, mods("c"), F).as_deref(), Some("[122;5u"));
}

#[test]
fn all_three_flags_at_once_read_as_one_sequence() {
    // What codex actually gets.  Key, shifted alternate, US
    // position, modifiers, event type — in that order, which is the
    // order the spec fixes.
    let ev = press_at(LogicalKey::Char('w'), Some("W"), 'z');
    assert_eq!(with_flags(&ev, mods("cs"), 7).as_deref(), Some("[119:87:122;6:1u"));
}

#[test]
fn the_stack_is_bounded_and_a_runaway_pop_resets_it() {
    // Both are what stops a program from spending this terminal's
    // memory or its cpu: pushes evict, and a pop larger than the
    // stack is a reset rather than a loop.
    let mut t = Terminal::new(40, 4);
    for _ in 0..50 {
        t.feed(b"\x1b[>1u");
    }
    assert_eq!(t.kitty_keyboard_flags(), 1);
    t.feed(b"\x1b[<9999u");
    assert_eq!(t.kitty_keyboard_flags(), 0);
}

#[test]
fn a_tui_cannot_leave_the_protocol_raised_over_the_shell() {
    // Each screen has its own stack.  A program that raises the
    // protocol on the alt screen and then dies without lowering it
    // must not leave the shell it drops back to encoding keys the
    // shell does not read.
    let mut t = Terminal::new(40, 4);
    t.feed(b"\x1b[?1049h\x1b[>1u");
    assert_eq!(t.kitty_keyboard_flags(), 1);
    t.feed(b"\x1b[?1049l");
    assert_eq!(t.kitty_keyboard_flags(), 0);

    // And the main screen's own state survives the round trip.
    t.feed(b"\x1b[>1u\x1b[?1049h");
    assert_eq!(t.kitty_keyboard_flags(), 0, "alt screen starts clean");
    t.feed(b"\x1b[?1049l");
    assert_eq!(t.kitty_keyboard_flags(), 1, "main screen kept its own");
}

#[test]
fn the_terminal_hands_the_encoder_what_the_program_asked_for() {
    // The join between the two halves: flags arrive as bytes from the
    // pty, and `input_modes` is what carries them to the encoder.
    // Tested here because each half passing on its own says nothing
    // about the wire between them.
    let mut t = Terminal::new(40, 4);
    let ev = named(NamedKey::Enter);
    let m = mods("s");

    // Before: marspot's own guess at what shift+enter meant.  Kept on
    // purpose — it is what a program that never asks still gets.
    let before = key_event_to_bytes(&ev, m, t.input_modes(), || None).unwrap();
    assert_eq!(&*before, b"\n");

    t.feed(b"\x1b[>1u");
    let after = key_event_to_bytes(&ev, m, t.input_modes(), || None).unwrap();
    assert_eq!(&*after, b"\x1b[13;2u");

    t.feed(b"\x1b[<u");
    let popped = key_event_to_bytes(&ev, m, t.input_modes(), || None).unwrap();
    assert_eq!(&*popped, b"\n");
}

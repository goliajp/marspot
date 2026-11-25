//! The terminfo entry: `TERM` is the borrowed `xterm-256color`, and
//! each exception the README lists is still what the code does.  The
//! session crate's own terminfo tests hold every other capability of
//! that entry to the terminal.

use marspot::grid_shm::cells::VERSION_PACKED;
use marspot::grid_shm::{GridShmReader, GridShmWriter};
use marspot::input_core::{
    LogicalKey, MarspotKeyEvent, Modifiers, NamedKey, TermModes, key_event_to_bytes,
};
use marspot::terminal::Terminal;

use super::{closed, says, the_page_says};

/// The modes after `smkx`, which oh-my-zsh sends at every prompt:
/// cursor keys and the keypad in application mode.
fn smkx() -> TermModes {
    let mut t = Terminal::new(80, 24);
    t.feed(b"\x1b[?1h\x1b=");
    assert!(
        t.screen_modes().keypad_app,
        "the probe did not turn the keypad to application mode"
    );
    t.input_modes()
}

fn send(logical: LogicalKey, text: Option<&str>) -> Vec<u8> {
    let event = MarspotKeyEvent {
        logical,
        text: text.map(String::from),
        ..Default::default()
    };
    key_event_to_bytes(&event, Modifiers::default(), smkx(), || None)
        .expect("the key sends bytes")
        .into_owned()
}

fn row(t: &Terminal) -> String {
    let s: String = (0..t.grid().cols())
        .map(|c| t.grid().cell(c, 0).ch)
        .collect();
    s.trim_end().to_string()
}

#[test]
fn term_is_the_borrowed_xterm_256color() {
    says("There is no terminfo entry of its own. `TERM` is set to `xterm-256color`");
    the_page_says("No terminfo entry of its own.");
    const PANE: &str = include_str!("../../../crates/marspot-session/src/local_session.rs");
    assert!(
        PANE.contains("set_var(\"TERM\", \"xterm-256color\")"),
        "{}",
        closed("a pane's TERM is no longer xterm-256color")
    );
}

/// Backspace is DEL; no key is the keypad's Begin; the keypad's Enter
/// is Return.  The keypad's keys reach the encoder as the main keys
/// (`app_keys` maps keypad Enter to `NamedKey::Enter`, keypad 5 to the
/// digit), so these are what the keypad sends.
#[test]
fn the_key_exceptions_hold() {
    says("Backspace sends DEL (0x7f) rather than the entry's ^H");
    says("no key sends the keypad's Begin (`kb2`), since a Mac keypad has no NumLock");
    says("the keypad's Enter sends Return, not `ESC O M`");

    assert_eq!(
        send(LogicalKey::Named(NamedKey::Backspace), None),
        b"\x7f",
        "{}",
        closed("Backspace no longer sends DEL")
    );
    assert_eq!(
        send(LogicalKey::Named(NamedKey::Enter), None),
        b"\r",
        "{}",
        closed("Enter sends ESC O M under smkx")
    );
    assert_eq!(
        send(LogicalKey::Char('5'), Some("5")),
        b"5",
        "{}",
        closed("keypad 5 sends Begin under smkx")
    );
}

/// The two keypad exceptions live in the key-code table, not in the
/// encoder: keypad Enter is mapped to the main Enter, and no keypad
/// digit or operator has a code of its own.  A key event carries no
/// "from the keypad" bit, so the table is the only place they can
/// close.
#[test]
fn the_keypad_has_no_keys_of_its_own() {
    const KEYS: &str = include_str!("../../../src/app_keys.rs");
    assert!(
        KEYS.contains("0x4C => LogicalKey::Named(NamedKey::Enter)"),
        "{}",
        closed("keypad Enter is no longer mapped to Return")
    );
    // kVK_ANSI_Keypad* : decimal, multiply, plus, clear, divide, minus,
    // equals, 0-7, 8, 9 (enter is above; 0x5A between 7 and 8 is F20)
    let keypad = [0x41, 0x43, 0x45, 0x47, 0x4B, 0x4E, 0x51]
        .into_iter()
        .chain(0x52..=0x59)
        .chain([0x5B, 0x5C]);
    for code in keypad {
        let arm = format!("0x{code:02X} =>");
        assert!(
            !KEYS.contains(&arm),
            "{}",
            closed(&format!("keypad key code {arm} has a mapping of its own"))
        );
    }
    // a named key added for the keypad (Begin, KeypadEnter) fails to
    // compile here before it can slip past the table check above
    let every = |k: NamedKey| match k {
        NamedKey::Enter
        | NamedKey::Backspace
        | NamedKey::Tab
        | NamedKey::Escape
        | NamedKey::ArrowUp
        | NamedKey::ArrowDown
        | NamedKey::ArrowLeft
        | NamedKey::ArrowRight
        | NamedKey::PageUp
        | NamedKey::PageDown
        | NamedKey::Home
        | NamedKey::End
        | NamedKey::Insert
        | NamedKey::Delete
        | NamedKey::F1
        | NamedKey::F2
        | NamedKey::F3
        | NamedKey::F4
        | NamedKey::F5
        | NamedKey::F6
        | NamedKey::F7
        | NamedKey::F8
        | NamedKey::F9
        | NamedKey::F10
        | NamedKey::F11
        | NamedKey::F12 => true,
    };
    assert!(every(NamedKey::Enter));
}

/// The media copy sequences are accepted and ignored: nothing is
/// answered and what follows `CSI 5 i` stays on screen.
#[test]
fn the_printer_sequences_are_ignored() {
    says(
        "the printer sequences are ignored, since there is no printer, so what a program prints stays on screen",
    );
    for (name, seq) in [("mc0", b"\x1b[i".as_slice()), ("mc4", b"\x1b[4i")] {
        let mut t = Terminal::new(40, 3);
        t.feed(b"ab");
        t.feed(seq);
        t.feed(b"c");
        assert_eq!(
            row(&t),
            "abc",
            "{}",
            closed(&format!("{name} did something to the screen"))
        );
        assert!(t.take_response().is_empty(), "{name} answered");
    }
    let mut t = Terminal::new(40, 3);
    t.feed(b"\x1b[5iprinted\x1b[4i");
    assert_eq!(
        row(&t),
        "printed",
        "{}",
        closed("printer controller mode took the output off the screen")
    );
}

/// The grid keeps SGR 5; the frame the renderer draws from carries
/// it nowhere, in either cell layout.
#[test]
fn blinking_text_is_drawn_steady() {
    says("Blinking text (SGR 5) is drawn steady.");
    let mut t = Terminal::new(20, 2);
    t.feed(b"\x1b[5mx");
    assert!(t.grid().cell(0, 0).attrs.blink(), "the grid dropped SGR 5");
    for packed in [false, true] {
        let g = t.grid();
        let mut w = GridShmWriter::create(g.cols(), g.rows()).unwrap();
        if packed {
            w.set_reader_frames(VERSION_PACKED);
        }
        let r = GridShmReader::from_fd(unsafe { libc::dup(w.fd()) }).unwrap();
        w.publish_screen_modes(t.screen_modes());
        w.publish(g, 0, 0);
        let mut cells = Vec::new();
        r.read(&mut cells, &mut Vec::new()).expect("a frame");
        assert_eq!(cells[0].ch, 'x');
        assert!(
            !cells[0].attrs.blink(),
            "{}",
            closed(&format!("a frame carries SGR 5 (packed: {packed})"))
        );
    }
}

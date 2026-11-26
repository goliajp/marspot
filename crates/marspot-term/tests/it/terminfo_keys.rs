//! The key capabilities: what the encoder sends for each key, with
//! the modes `smkx` puts in force, since a program reading these
//! strings has sent `smkx` first.

use marspot_term::input_core::{key_event_to_bytes, LogicalKey, MarspotKeyEvent, Modifiers, NamedKey, TermModes};

use super::{term, EXCEPTIONS};

const SHIFT: u8 = 1;
const ALT: u8 = 2;
const CTRL: u8 = 4;

fn mods(bits: u8) -> Modifiers {
    Modifiers { shift: bits & SHIFT != 0, alt: bits & ALT != 0, control: bits & CTRL != 0, super_: false }
}

/// The modes a terminal is in after `smkx`, read off the terminal.
fn smkx_modes() -> TermModes {
    let mut t = term();
    let entry = super::entry();
    let smkx = entry.iter().find_map(|(k, c)| match c {
        super::Cap::Str(s) if k == "smkx" => Some(s.clone()),
        _ => None,
    });
    t.feed(&smkx.expect("the entry has smkx"));
    t.input_modes()
}

pub fn send(logical: LogicalKey, text: Option<&str>, bits: u8, modes: TermModes) -> Vec<u8> {
    let ev = MarspotKeyEvent { logical, text: text.map(Into::into), ..Default::default() };
    key_event_to_bytes(&ev, mods(bits), modes, || None).map(|b| b.into_owned()).unwrap_or_default()
}

/// The key and modifiers a capability names.
fn named(name: &str) -> Option<(NamedKey, u8)> {
    use NamedKey::*;
    if let Some(n) = name.strip_prefix("kf").and_then(|n| n.parse::<usize>().ok()) {
        // kf13-24 are shift+F1-12, then ctrl, ctrl+shift, alt, alt+shift
        const F: [NamedKey; 12] = [F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12];
        const GROUP: [u8; 6] = [0, SHIFT, CTRL, CTRL | SHIFT, ALT, ALT | SHIFT];
        return Some((F[(n - 1) % 12], GROUP[(n - 1) / 12]));
    }
    Some(match name {
        "kcuu1" => (ArrowUp, 0),
        "kcud1" => (ArrowDown, 0),
        "kcuf1" => (ArrowRight, 0),
        "kcub1" => (ArrowLeft, 0),
        "kri" => (ArrowUp, SHIFT),
        "kind" => (ArrowDown, SHIFT),
        "kRIT" => (ArrowRight, SHIFT),
        "kLFT" => (ArrowLeft, SHIFT),
        "khome" => (Home, 0),
        "kend" => (End, 0),
        "kHOM" => (Home, SHIFT),
        "kEND" => (End, SHIFT),
        "kich1" => (Insert, 0),
        "kIC" => (Insert, SHIFT),
        "kdch1" => (Delete, 0),
        "kDC" => (Delete, SHIFT),
        "kpp" => (PageUp, 0),
        "kPRV" => (PageUp, SHIFT),
        "knp" => (PageDown, 0),
        "kNXT" => (PageDown, SHIFT),
        "kcbt" => (Tab, SHIFT),
        "kbs" => (Backspace, 0),
        _ => return None,
    })
}

pub fn key(name: &str, s: &[u8]) -> bool {
    let modes = smkx_modes();
    let exception = EXCEPTIONS.iter().any(|(n, _)| *n == name);
    match name {
        // Backspace sends DEL, whatever the entry says
        "kbs" => {
            assert!(exception);
            assert_eq!(s, b"\x08");
            assert_eq!(send(LogicalKey::Named(NamedKey::Backspace), None, 0, modes), b"\x7f");
        }
        // the keypad's Enter is Return, under smkx too
        "kent" => {
            assert!(exception);
            assert_eq!(s, b"\x1bOM");
            assert_eq!(send(LogicalKey::Named(NamedKey::Enter), None, 0, modes), b"\r");
        }
        // keypad 5 is a digit on a Mac keypad, never Begin
        "kb2" => {
            assert!(exception);
            assert_eq!(s, b"\x1bOE");
            assert_eq!(send(LogicalKey::Char('5'), Some("5"), 0, modes), b"5");
        }
        _ => {
            let Some((k, bits)) = named(name) else {
                return false;
            };
            assert_eq!(
                send(LogicalKey::Named(k), None, bits, modes),
                s,
                "{name}: {k:?} with modifiers {bits:#05b}"
            );
        }
    }
    true
}

/// `km`: the Option key is a meta key, sent as `ESC` before the key,
/// with `smm` in force and without it.
pub fn meta_key() {
    let mut t = term();
    for mode in [b"\x1b[?1034h".as_slice(), b"\x1b[?1034l"] {
        t.feed(mode);
        assert_eq!(send(LogicalKey::Char('f'), Some("f"), ALT, t.input_modes()), b"\x1bf");
    }
}

/// Outside `smkx`, Home is readline's ^A; `smkx` is what makes it the
/// entry's string.
#[test]
fn terminfo_keys_outside_keypad_transmit_mode() {
    let t = term();
    let m = t.input_modes();
    assert_eq!(send(LogicalKey::Named(NamedKey::Home), None, 0, m), b"\x01");
    assert_eq!(send(LogicalKey::Named(NamedKey::ArrowUp), None, 0, m), b"\x1b[A");
    assert!(smkx_modes().cursor_key_app);
}

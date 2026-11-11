//! The chords on keyboards that are not US ones.
//!
//! Each press is built the way the window builds one from AppKit's
//! event: the key's own character, the character Cmd leaves it with
//! and the one Shift gives it all come from `UCKeyTranslate` over the
//! layout Apple ships under that name, so the tables here are the
//! system's, not a transcription.  AppKit's `characters` is taken to
//! be that function's answer with Cmd and Option held; on the layout
//! of the machine this was written on it is, Shift ignored included.  The Japanese layout is an input method with
//! no table of its own; what a JIS keyboard types is the ABC layout
//! read as that keyboard type.

use marspot_term::input_core::{LogicalKey, MarspotKeyEvent};

use crate::commands::{for_key, Chord, Command as C, Key, CMD, CTRL, OPT, SHIFT};
use crate::keyboard_layout::{self, chord_char, installed, us_layout_char};

const US: (&str, u32) = ("com.apple.keylayout.US", installed::ANSI);
const JIS: (&str, u32) = ("com.apple.keylayout.ABC", installed::JIS);
const GERMAN: (&str, u32) = ("com.apple.keylayout.German", installed::ISO);
const AZERTY: (&str, u32) = ("com.apple.keylayout.French", installed::ISO);
const DVORAK_QWERTY_CMD: (&str, u32) = ("com.apple.keylayout.DVORAK-QWERTYCMD", installed::ANSI);
const RUSSIAN: (&str, u32) = ("com.apple.keylayout.Russian", installed::ANSI);

/// The press of `key_code` with `mods` held, and what it types.
fn press((layout, kbd): (&str, u32), key_code: u16, mods: u8) -> (MarspotKeyEvent, Option<char>) {
    assert_eq!(mods & CTRL, 0, "a control code is not in the layout's table");
    let option = if mods & OPT != 0 { keyboard_layout::OPTION } else { 0 };
    let cmd = if mods & CMD != 0 { installed::CMD } else { 0 };
    let cmd_level = installed::typed(layout, kbd, key_code, cmd | option);
    let shifted = || installed::typed(layout, kbd, key_code, keyboard_layout::SHIFT | option);
    let typed = chord_char(cmd_level, mods & SHIFT != 0, shifted);
    let event = MarspotKeyEvent {
        logical: installed::typed(layout, kbd, key_code, 0).map_or(LogicalKey::Other, LogicalKey::Char),
        text: typed.map(String::from),
        base_layout: us_layout_char(key_code),
        ..Default::default()
    };
    (event, typed)
}

/// Each `(key code, modifiers, the character that press types, the
/// command it is)` holds under `layout`.
fn check(layout: (&str, u32), want: &[(u16, u8, char, Option<C>)]) {
    for &(key_code, mods, types, command) in want {
        let (event, typed) = press(layout, key_code, mods);
        let what = format!("{} key {key_code:#04x} with {mods:#06b}", layout.0);
        assert_eq!(typed, Some(types), "{what}: the layout is not the one this table was written for");
        let got = for_key(&event, Chord { key: Key::Esc, mods }.modifiers()).map(|r| r.command);
        assert_eq!(got, command, "{what}, typing {types:?}");
    }
}

#[test]
fn a_us_keyboard_presses_the_chords_as_written() {
    check(
        US,
        &[
            (0x12, CMD, '1', Some(C::FocusPane(0))),
            (0x19, CMD, '9', Some(C::FocusPane(8))),
            (0x1D, CMD, '0', None),
            (0x21, CMD, '[', Some(C::PrevPane)),
            (0x1E, CMD, ']', Some(C::NextPane)),
            (0x2C, CMD, '/', Some(C::Shortcuts)),
            (0x2B, CMD, ',', Some(C::Settings)),
            (0x08, CMD, 'c', Some(C::Copy)),
            (0x08, SHIFT | CMD, 'c', Some(C::Accounts)),
            (0x25, SHIFT | CMD, 'l', Some(C::Layout)),
            (0x25, CMD, 'l', None),
            // Shift makes these other characters, which are no chord.
            (0x2C, SHIFT | CMD, '?', None),
            (0x12, SHIFT | CMD, '!', None),
            (0x21, SHIFT | CMD, '{', None),
            // Option types something else; the key is still H.
            (0x04, OPT | CMD, '˙', Some(C::HideOthers)),
        ],
    );
}

#[test]
fn a_jis_keyboard_has_its_brackets_one_key_to_the_right() {
    check(
        JIS,
        &[
            (0x12, CMD, '1', Some(C::FocusPane(0))),
            (0x19, CMD, '9', Some(C::FocusPane(8))),
            (0x21, CMD, '@', None),
            (0x1E, CMD, '[', Some(C::PrevPane)),
            (0x2A, CMD, ']', Some(C::NextPane)),
            (0x2C, CMD, '/', Some(C::Shortcuts)),
            (0x2B, CMD, ',', Some(C::Settings)),
            (0x5D, CMD, '¥', None),
        ],
    );
}

#[test]
fn a_german_keyboard_types_its_brackets_with_option_and_its_slash_with_shift() {
    check(
        GERMAN,
        &[
            (0x12, CMD, '1', Some(C::FocusPane(0))),
            (0x17, CMD, '5', Some(C::FocusPane(4))),
            (0x17, OPT | CMD, '[', Some(C::PrevPane)),
            (0x16, OPT | CMD, ']', Some(C::NextPane)),
            (0x1A, SHIFT | CMD, '/', Some(C::Shortcuts)),
            (0x2B, CMD, ',', Some(C::Settings)),
            (0x08, SHIFT | CMD, 'c', Some(C::Accounts)),
            // The keys a US keyboard has those characters on type
            // others here, and are no chord.
            (0x21, CMD, 'ü', None),
            (0x1E, CMD, '+', None),
            (0x2C, CMD, '-', None),
            // Y and Z have changed places; neither is a chord.
            (0x06, CMD, 'y', None),
            (0x10, CMD, 'z', None),
        ],
    );
}

#[test]
fn an_azerty_keyboard_reaches_the_panes_by_the_digit_row_shifted_or_not() {
    check(
        AZERTY,
        &[
            (0x12, CMD, '&', Some(C::FocusPane(0))),
            (0x19, CMD, 'ç', Some(C::FocusPane(8))),
            (0x12, SHIFT | CMD, '1', Some(C::FocusPane(0))),
            (0x19, SHIFT | CMD, '9', Some(C::FocusPane(8))),
            (0x1D, CMD, 'à', None),
            (0x2F, SHIFT | CMD, '/', Some(C::Shortcuts)),
            (0x2E, CMD, ',', Some(C::Settings)),
            (0x2B, CMD, ';', None),
            // Letters go by what they type, never by where a US
            // keyboard has them: the key that types A is not Quit.
            (0x0C, CMD, 'a', None),
            (0x00, CMD, 'q', Some(C::Quit)),
            (0x06, CMD, 'w', Some(C::ClosePanel)),
            (0x29, CMD, 'm', Some(C::Minimize)),
        ],
    );
}

#[test]
fn a_layout_that_changes_under_cmd_goes_by_what_cmd_types() {
    check(
        DVORAK_QWERTY_CMD,
        &[
            (0x08, CMD, 'c', Some(C::Copy)),
            (0x25, SHIFT | CMD, 'l', Some(C::Layout)),
            (0x0C, CMD, 'q', Some(C::Quit)),
            (0x21, CMD, '[', Some(C::PrevPane)),
        ],
    );
    // Without Cmd that key types `j`, and the one that types `q` is
    // elsewhere: it is not Quit with Cmd held.
    assert_eq!(installed::typed(DVORAK_QWERTY_CMD.0, DVORAK_QWERTY_CMD.1, 0x08, 0), Some('j'));
    assert_eq!(installed::typed(DVORAK_QWERTY_CMD.0, DVORAK_QWERTY_CMD.1, 0x07, 0), Some('q'));
    check(DVORAK_QWERTY_CMD, &[(0x07, CMD, 'x', None)]);
    check(RUSSIAN, &[(0x08, CMD, 'c', Some(C::Copy)), (0x12, CMD, '1', Some(C::FocusPane(0)))]);
    assert_eq!(installed::typed(RUSSIAN.0, RUSSIAN.1, 0x08, 0), Some('с'));
}

/// A press that says nothing of what it typed — a menu item replaying
/// its chord — is the key's own character.
#[test]
fn a_press_with_no_text_is_its_own_character() {
    for row in crate::commands::TABLE {
        let Some(chord) = row.chord else { continue };
        let got = for_key(&chord.key_event(), chord.modifiers()).map(|r| r.command);
        assert_eq!(got, Some(row.command));
    }
}

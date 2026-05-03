//! Translate winit input events into terminal bytes.
//!
//! Owned by the lib because `mars` (multi-session) and `mcli`
//! (single-session) both feed the same Session API.  Anything that
//! depends only on a winit `KeyEvent` + `ModifiersState` belongs
//! here; per-binary policy (which session receives the keystroke,
//! how view-offset interacts with typing, etc.) stays in the
//! caller.

use std::borrow::Cow;

use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Map a winit key press to the byte sequence we send the PTY.  Returns
/// `None` for events we don't translate (releases, modifier-only, Cmd
/// combos that the OS handles, etc.).
pub fn key_event_to_bytes(
    event: &KeyEvent,
    modifiers: ModifiersState,
) -> Option<Cow<'static, [u8]>> {
    if event.state != ElementState::Pressed {
        return None;
    }

    // Cmd combos: a few are ours (Cmd-V paste from clipboard); the rest
    // belong to the OS / app layer (Cmd-Q to quit, Cmd-C copy, etc.).
    if modifiers.super_key() {
        if let Key::Character(s) = &event.logical_key {
            if s.as_str().eq_ignore_ascii_case("v") {
                if let Some(text) = read_clipboard_text() {
                    // Paste as raw bytes; bracketed paste support comes
                    // later (DECSET ?2004) — for now CR is forwarded
                    // verbatim, which matches Terminal.app's default
                    // when bracketed paste isn't enabled.
                    return Some(Cow::Owned(text.into_bytes()));
                }
            }
        }
        return None;
    }

    // Ctrl + letter → ASCII control code (Ctrl-A = 0x01 ... Ctrl-Z = 0x1A).
    // Also: Ctrl-[ = ESC, Ctrl-\ = FS, Ctrl-] = GS, Ctrl-^ = RS, Ctrl-_ = US,
    // Ctrl-Space = NUL.  Done before the named-key match so Ctrl-anything
    // takes priority over the per-key text payload.
    if modifiers.control_key() {
        if let Key::Character(s) = &event.logical_key {
            if let Some(c) = s.chars().next() {
                let lc = c.to_ascii_lowercase();
                let code = match lc {
                    'a'..='z' => Some((lc as u8) - b'a' + 1),
                    '[' => Some(0x1b),
                    '\\' => Some(0x1c),
                    ']' => Some(0x1d),
                    '^' => Some(0x1e),
                    '_' => Some(0x1f),
                    ' ' => Some(0x00),
                    _ => None,
                };
                if let Some(code) = code {
                    return Some(Cow::Owned(vec![code]));
                }
            }
        }
    }

    match &event.logical_key {
        Key::Named(NamedKey::Enter) => Some(Cow::Borrowed(b"\r")),
        Key::Named(NamedKey::Backspace) => Some(Cow::Borrowed(b"\x7f")),
        Key::Named(NamedKey::Tab) => Some(Cow::Borrowed(b"\t")),
        Key::Named(NamedKey::Escape) => Some(Cow::Borrowed(b"\x1b")),
        Key::Named(NamedKey::ArrowUp) => Some(Cow::Borrowed(b"\x1b[A")),
        Key::Named(NamedKey::ArrowDown) => Some(Cow::Borrowed(b"\x1b[B")),
        Key::Named(NamedKey::ArrowRight) => Some(Cow::Borrowed(b"\x1b[C")),
        Key::Named(NamedKey::ArrowLeft) => Some(Cow::Borrowed(b"\x1b[D")),
        _ => event
            .text
            .as_ref()
            .map(|t| Cow::Owned(t.as_bytes().to_vec())),
    }
}

/// Read the current macOS general-pasteboard string, if any.  Used to
/// implement Cmd-V → write to PTY.  Returns None if the clipboard
/// holds non-text content (image, file URLs, etc.).
pub fn read_clipboard_text() -> Option<String> {
    unsafe {
        let pb = NSPasteboard::generalPasteboard();
        let s = pb.stringForType(NSPasteboardTypeString)?;
        Some(s.to_string())
    }
}

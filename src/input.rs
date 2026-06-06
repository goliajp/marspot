//! Translate keyboard events into terminal bytes.
//!
//! Owned by the lib because `marspot` (multi-session) and `mcli`
//! (single-session) both feed the same Session API.  Anything that
//! depends only on a key press + modifier state belongs here;
//! per-binary policy (which session receives the keystroke, how
//! view-offset interacts with typing, etc.) stays in the caller.
//!
//! Key events are described by Marspot-owned types (`MarspotKeyEvent`,
//! `LogicalKey`, `NamedKey`, `Modifiers`) so this module has no
//! window-system dependency.  The window layer (currently winit,
//! soon `app::run_app` over AppKit directly) is responsible for
//! converting its native events into these.

use std::borrow::Cow;

use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};

/// Marspot's portable key event.  Window backends (winit today,
/// AppKit-direct tomorrow) translate their native events into this.
#[derive(Clone, Debug)]
pub struct MarspotKeyEvent {
    pub state: KeyState,
    /// Modifier-independent identifier of the pressed key.
    /// `Char('a')` for `a` and `shift+a` alike; the resolved text
    /// lives in `text`.
    pub logical: LogicalKey,
    /// What the keystroke would type with current modifiers applied
    /// (e.g. `"A"` for shift+a, `" "` for space).  `None` for
    /// navigation keys / pure modifier presses / dead keys.
    pub text: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyState {
    Pressed,
    Released,
}

/// Logical-key identity, modifier-independent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogicalKey {
    /// Single character (`a`, `1`, `=`, …).  Modifier-independent —
    /// shift+a still reports `Char('a')`.
    Char(char),
    /// One of the named navigation / control keys we explicitly handle.
    Named(NamedKey),
    /// Anything else — caller falls back to `text` if present.
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamedKey {
    Enter,
    Backspace,
    Tab,
    Escape,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub control: bool,
    /// Option on macOS.
    pub alt: bool,
    /// Cmd on macOS.
    pub super_: bool,
}

impl Modifiers {
    pub fn shift_key(self) -> bool {
        self.shift
    }
    pub fn control_key(self) -> bool {
        self.control
    }
    pub fn alt_key(self) -> bool {
        self.alt
    }
    pub fn super_key(self) -> bool {
        self.super_
    }
}

/// Map a key press to the byte sequence we send the PTY.  Returns
/// `None` for events we don't translate (releases, modifier-only, Cmd
/// combos that the OS handles, etc.).
pub fn key_event_to_bytes(
    event: &MarspotKeyEvent,
    modifiers: Modifiers,
) -> Option<Cow<'static, [u8]>> {
    if event.state != KeyState::Pressed {
        return None;
    }

    // Cmd combos: a few are ours (Cmd-V paste from clipboard); the rest
    // belong to the OS / app layer (Cmd-Q to quit, Cmd-C copy, etc.).
    if modifiers.super_key() {
        if let LogicalKey::Char(c) = event.logical {
            if c.eq_ignore_ascii_case(&'v') {
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
        if let LogicalKey::Char(c) = event.logical {
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

    match &event.logical {
        LogicalKey::Named(NamedKey::Enter) => Some(Cow::Borrowed(b"\r")),
        LogicalKey::Named(NamedKey::Backspace) => Some(Cow::Borrowed(b"\x7f")),
        LogicalKey::Named(NamedKey::Tab) => Some(Cow::Borrowed(b"\t")),
        LogicalKey::Named(NamedKey::Escape) => Some(Cow::Borrowed(b"\x1b")),
        LogicalKey::Named(NamedKey::ArrowUp) => Some(Cow::Borrowed(b"\x1b[A")),
        LogicalKey::Named(NamedKey::ArrowDown) => Some(Cow::Borrowed(b"\x1b[B")),
        LogicalKey::Named(NamedKey::ArrowRight) => Some(Cow::Borrowed(b"\x1b[C")),
        LogicalKey::Named(NamedKey::ArrowLeft) => Some(Cow::Borrowed(b"\x1b[D")),
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

/// Replace the macOS general pasteboard contents with a single
/// plain-text string.  Used to implement Cmd-C → copy current
/// terminal selection.  Returns true on success.
pub fn write_clipboard_text(text: &str) -> bool {
    unsafe {
        let pb = NSPasteboard::generalPasteboard();
        // clearContents must precede setString or AppKit retains
        // any prior reps and the new write may be ignored.
        pb.clearContents();
        let s = objc2_foundation::NSString::from_str(text);
        pb.setString_forType(&s, NSPasteboardTypeString)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pressed(logical: LogicalKey, text: Option<&str>) -> MarspotKeyEvent {
        MarspotKeyEvent {
            state: KeyState::Pressed,
            logical,
            text: text.map(|s| s.to_string()),
        }
    }

    #[test]
    fn release_returns_none() {
        let ev = MarspotKeyEvent {
            state: KeyState::Released,
            logical: LogicalKey::Char('a'),
            text: Some("a".into()),
        };
        assert!(key_event_to_bytes(&ev, Modifiers::default()).is_none());
    }

    #[test]
    fn plain_char_falls_through_to_text() {
        let ev = pressed(LogicalKey::Char('a'), Some("a"));
        let out = key_event_to_bytes(&ev, Modifiers::default()).unwrap();
        assert_eq!(&*out, b"a");
    }

    #[test]
    fn ctrl_letter_yields_control_code() {
        let ev = pressed(LogicalKey::Char('c'), Some("c"));
        let mods = Modifiers {
            control: true,
            ..Default::default()
        };
        let out = key_event_to_bytes(&ev, mods).unwrap();
        assert_eq!(&*out, &[0x03]);
    }

    #[test]
    fn ctrl_bracket_yields_esc() {
        let ev = pressed(LogicalKey::Char('['), Some("["));
        let mods = Modifiers {
            control: true,
            ..Default::default()
        };
        let out = key_event_to_bytes(&ev, mods).unwrap();
        assert_eq!(&*out, &[0x1b]);
    }

    #[test]
    fn arrow_keys_yield_csi() {
        let up = pressed(LogicalKey::Named(NamedKey::ArrowUp), None);
        let down = pressed(LogicalKey::Named(NamedKey::ArrowDown), None);
        let left = pressed(LogicalKey::Named(NamedKey::ArrowLeft), None);
        let right = pressed(LogicalKey::Named(NamedKey::ArrowRight), None);
        assert_eq!(&*key_event_to_bytes(&up, Modifiers::default()).unwrap(), b"\x1b[A");
        assert_eq!(&*key_event_to_bytes(&down, Modifiers::default()).unwrap(), b"\x1b[B");
        assert_eq!(&*key_event_to_bytes(&left, Modifiers::default()).unwrap(), b"\x1b[D");
        assert_eq!(&*key_event_to_bytes(&right, Modifiers::default()).unwrap(), b"\x1b[C");
    }

    #[test]
    fn enter_returns_cr() {
        let ev = pressed(LogicalKey::Named(NamedKey::Enter), None);
        assert_eq!(&*key_event_to_bytes(&ev, Modifiers::default()).unwrap(), b"\r");
    }

    #[test]
    fn cmd_non_v_returns_none() {
        let ev = pressed(LogicalKey::Char('a'), Some("a"));
        let mods = Modifiers {
            super_: true,
            ..Default::default()
        };
        assert!(key_event_to_bytes(&ev, mods).is_none());
    }
}

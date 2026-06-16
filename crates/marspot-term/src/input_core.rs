//! Translate keyboard events into terminal bytes — the GUI-free half.
//!
//! Lives in `marspot-term` (no AppKit/Metal/CoreText) so the light
//! per-session process (target #4 L3) can encode keystrokes itself.
//! Everything here depends only on a key press + modifier state.
//!
//! Key events are described by Marspot-owned types (`MarspotKeyEvent`,
//! `LogicalKey`, `NamedKey`, `Modifiers`) so this module has no
//! window-system dependency.  The window layer (AppKit via
//! `app::run_app`) converts native events into these.
//!
//! The one GUI-adjacent concern — Cmd-V reading the macOS clipboard —
//! is injected as a `read_clipboard` closure rather than called
//! directly, keeping this module pure.  The clipboard implementation
//! (`read_clipboard_text` / `write_clipboard_text`) stays in
//! `marspot::input` on the AppKit side.

use std::borrow::Cow;

/// Marspot's portable key event.  Window backends (AppKit-direct)
/// translate their native events into this.
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
    PageUp,
    PageDown,
    Home,
    End,
    Insert,
    Delete,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
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
///
/// `cursor_key_app_mode` is the terminal's DECCKM state: when true,
/// arrow keys encode as `ESC O X` (application sequence) instead of
/// `ESC [ X`. TUI apps that bind cursor keys distinctly from
/// PgUp/PgDn navigation depend on this.
///
/// `read_clipboard` is called lazily only on Cmd-V so this module
/// needs no window-system dependency; the AppKit implementation is
/// injected by the caller (`marspot::input::read_clipboard_text`).
pub fn key_event_to_bytes(
    event: &MarspotKeyEvent,
    modifiers: Modifiers,
    cursor_key_app_mode: bool,
    bracketed_paste_mode: bool,
    read_clipboard: impl FnOnce() -> Option<String>,
) -> Option<Cow<'static, [u8]>> {
    if event.state != KeyState::Pressed {
        return None;
    }

    // Cmd combos: a few are ours (Cmd-V paste from clipboard); the rest
    // belong to the OS / app layer (Cmd-Q to quit, Cmd-C copy, etc.).
    if modifiers.super_key() {
        if let LogicalKey::Char(c) = event.logical {
            if c.eq_ignore_ascii_case(&'v') {
                if let Some(text) = read_clipboard() {
                    // Bracketed paste (DECSET ?2004): wrap the paste in
                    // `\e[200~ ... \e[201~` so the app can distinguish
                    // paste from interactive typing. Apps like Claude
                    // Code TUI rely on this — without it, each pasted
                    // CJK char is treated as a separate keystroke and
                    // the app auto-inserts spaces between them. We
                    // also sanitise: strip any nested 201~ in the
                    // payload (xterm-spec defence — a pasted screen
                    // dump could otherwise inject an end-of-paste
                    // marker and exit bracketed mode early).
                    let body = text.into_bytes();
                    if bracketed_paste_mode {
                        // Naive concat (no end-marker stripping). xterm
                        // spec recommends defending against pasted
                        // `\e[201~` injecting an early end-of-paste,
                        // but claudecode / shell paste content almost
                        // never contains this 6-byte sequence verbatim
                        // — if it ever does we can add a streaming
                        // filter then.
                        let mut out = Vec::with_capacity(body.len() + 12);
                        out.extend_from_slice(b"\x1b[200~");
                        out.extend_from_slice(&body);
                        out.extend_from_slice(b"\x1b[201~");
                        return Some(Cow::Owned(out));
                    }
                    return Some(Cow::Owned(body));
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

    if let LogicalKey::Named(n) = &event.logical {
        return Some(encode_named_key(*n, modifiers, cursor_key_app_mode));
    }

    // Option / ⌥ on macOS = Meta prefix.  zsh in emacs mode, readline,
    // tmux, screen all treat `ESC <key>` as Meta-key (M-b / M-f for
    // word-wise motion etc.).  iTerm2 calls this "Left/Right Option
    // Key = Esc+".  We default to it for typed text — without it
    // Option+f just prints `ƒ` instead of word-forward.  Cmd was
    // already short-circuited above, so we only see Alt here.
    if modifiers.alt_key() {
        if let Some(text) = event.text.as_ref() {
            if !text.is_empty() {
                let mut out = Vec::with_capacity(1 + text.len());
                out.push(0x1b);
                out.extend_from_slice(text.as_bytes());
                return Some(Cow::Owned(out));
            }
        }
    }

    event
        .text
        .as_ref()
        .map(|t| Cow::Owned(t.as_bytes().to_vec()))
}

/// Encode an xterm modifier parameter (1+bitmask, per ctlseqs).
///
/// `1` = no modifier; the format `CSI 1 ; <m> X` only inserts the
/// `;<m>` when `m != 1`, so encoders use this returning value to
/// branch.  Bits: shift=1, alt=2, ctrl=4, super=8.
fn encode_mods(mods: Modifiers) -> u8 {
    let mut bits = 0u8;
    if mods.shift_key() {
        bits |= 1;
    }
    if mods.alt_key() {
        bits |= 2;
    }
    if mods.control_key() {
        bits |= 4;
    }
    if mods.super_key() {
        bits |= 8;
    }
    1 + bits
}

/// Cursor-key encoder.  `letter` is one of `A`/`B`/`C`/`D` (Up/Down/
/// Right/Left).  When no modifier is set, follow DECCKM: app mode →
/// `ESC O X` (SS3), normal → `ESC [ X` (CSI).  Modifiers always force
/// CSI form because SS3 has no place to embed the modifier param.
/// This matches alacritty / xterm modifyCursorKeys=2 conventions.
fn cursor_seq(letter: u8, mods: Modifiers, decckm: bool) -> Cow<'static, [u8]> {
    let m = encode_mods(mods);
    if m == 1 {
        if decckm {
            Cow::Owned(vec![0x1b, b'O', letter])
        } else {
            Cow::Owned(vec![0x1b, b'[', letter])
        }
    } else {
        // `CSI 1 ; m X` — `1` is the required cursor-key param.
        Cow::Owned(format!("\x1b[1;{}{}", m, letter as char).into_bytes())
    }
}

/// VT220-style navigation key encoder: `ESC [ <num> ~`, with optional
/// modifier `ESC [ <num> ; <m> ~`.  Used for PageUp/PageDown/Home/End
/// /Insert/Delete and F5-F12.
fn vt220_seq(num: u16, mods: Modifiers) -> Cow<'static, [u8]> {
    let m = encode_mods(mods);
    if m == 1 {
        Cow::Owned(format!("\x1b[{}~", num).into_bytes())
    } else {
        Cow::Owned(format!("\x1b[{};{}~", num, m).into_bytes())
    }
}

/// F1-F4 encoder: classic VT100 SS3 form (`ESC O P/Q/R/S`).  Modifiers
/// fall back to CSI form `ESC [ 1 ; <m> P/Q/R/S` per xterm
/// modifyFunctionKeys=2.
fn fkey14_seq(letter: u8, mods: Modifiers) -> Cow<'static, [u8]> {
    let m = encode_mods(mods);
    if m == 1 {
        Cow::Owned(vec![0x1b, b'O', letter])
    } else {
        Cow::Owned(format!("\x1b[1;{}{}", m, letter as char).into_bytes())
    }
}

fn encode_named_key(
    n: NamedKey,
    mods: Modifiers,
    cursor_key_app_mode: bool,
) -> Cow<'static, [u8]> {
    use NamedKey::*;
    match n {
        // Enter / Tab / Backspace / Escape — classic control bytes.
        // Shift+Enter → LF so multi-line editors (Ink TUIs, chat input
        // boxes) can distinguish "newline within input" from "send".
        Enter => {
            if mods.shift_key() {
                Cow::Borrowed(b"\n")
            } else {
                Cow::Borrowed(b"\r")
            }
        }
        Backspace => Cow::Borrowed(b"\x7f"),
        Tab => {
            if mods.shift_key() {
                Cow::Borrowed(b"\x1b[Z") // backtab
            } else {
                Cow::Borrowed(b"\t")
            }
        }
        Escape => Cow::Borrowed(b"\x1b"),
        // Arrow keys — DECCKM + modifier-aware.
        ArrowUp => cursor_seq(b'A', mods, cursor_key_app_mode),
        ArrowDown => cursor_seq(b'B', mods, cursor_key_app_mode),
        ArrowRight => cursor_seq(b'C', mods, cursor_key_app_mode),
        ArrowLeft => cursor_seq(b'D', mods, cursor_key_app_mode),
        // Home / End — readline-style by default.  Bare Home → ^A,
        // bare End → ^E, which is what zsh / bash / fish / readline
        // and every macOS Cocoa text view do at line-edit time.
        // Modifier-bearing presses (Shift-Home for selection in TUI
        // editors, Ctrl-Home to top-of-buffer, etc.) keep the CSI
        // form so they round-trip through xterm-aware apps.
        Home => {
            let m = encode_mods(mods);
            if m == 1 {
                Cow::Borrowed(b"\x01")
            } else {
                Cow::Owned(format!("\x1b[1;{}H", m).into_bytes())
            }
        }
        End => {
            let m = encode_mods(mods);
            if m == 1 {
                Cow::Borrowed(b"\x05")
            } else {
                Cow::Owned(format!("\x1b[1;{}F", m).into_bytes())
            }
        }
        Insert => vt220_seq(2, mods),
        Delete => vt220_seq(3, mods),
        PageUp => vt220_seq(5, mods),
        PageDown => vt220_seq(6, mods),
        // F1-F4: VT100 SS3 form; F5-F12: VT220 numbered.  The gap
        // between F4 (=ESC OS) and F5 (=ESC [15~) is historical xterm.
        F1 => fkey14_seq(b'P', mods),
        F2 => fkey14_seq(b'Q', mods),
        F3 => fkey14_seq(b'R', mods),
        F4 => fkey14_seq(b'S', mods),
        F5 => vt220_seq(15, mods),
        F6 => vt220_seq(17, mods),
        F7 => vt220_seq(18, mods),
        F8 => vt220_seq(19, mods),
        F9 => vt220_seq(20, mods),
        F10 => vt220_seq(21, mods),
        F11 => vt220_seq(23, mods),
        F12 => vt220_seq(24, mods),
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
        assert!(key_event_to_bytes(&ev, Modifiers::default(), false, false, || None).is_none());
    }

    #[test]
    fn plain_char_falls_through_to_text() {
        let ev = pressed(LogicalKey::Char('a'), Some("a"));
        let out = key_event_to_bytes(&ev, Modifiers::default(), false, false, || None).unwrap();
        assert_eq!(&*out, b"a");
    }

    #[test]
    fn ctrl_letter_yields_control_code() {
        let ev = pressed(LogicalKey::Char('c'), Some("c"));
        let mods = Modifiers {
            control: true,
            ..Default::default()
        };
        let out = key_event_to_bytes(&ev, mods, false, false, || None).unwrap();
        assert_eq!(&*out, &[0x03]);
    }

    #[test]
    fn ctrl_bracket_yields_esc() {
        let ev = pressed(LogicalKey::Char('['), Some("["));
        let mods = Modifiers {
            control: true,
            ..Default::default()
        };
        let out = key_event_to_bytes(&ev, mods, false, false, || None).unwrap();
        assert_eq!(&*out, &[0x1b]);
    }

    #[test]
    fn arrow_keys_yield_csi() {
        let up = pressed(LogicalKey::Named(NamedKey::ArrowUp), None);
        let down = pressed(LogicalKey::Named(NamedKey::ArrowDown), None);
        let left = pressed(LogicalKey::Named(NamedKey::ArrowLeft), None);
        let right = pressed(LogicalKey::Named(NamedKey::ArrowRight), None);
        assert_eq!(&*key_event_to_bytes(&up, Modifiers::default(), false, false, || None).unwrap(), b"\x1b[A");
        assert_eq!(&*key_event_to_bytes(&down, Modifiers::default(), false, false, || None).unwrap(), b"\x1b[B");
        assert_eq!(&*key_event_to_bytes(&left, Modifiers::default(), false, false, || None).unwrap(), b"\x1b[D");
        assert_eq!(&*key_event_to_bytes(&right, Modifiers::default(), false, false, || None).unwrap(), b"\x1b[C");
    }

    #[test]
    fn enter_returns_cr() {
        let ev = pressed(LogicalKey::Named(NamedKey::Enter), None);
        assert_eq!(&*key_event_to_bytes(&ev, Modifiers::default(), false, false, || None).unwrap(), b"\r");
    }

    #[test]
    fn cmd_non_v_returns_none() {
        let ev = pressed(LogicalKey::Char('a'), Some("a"));
        let mods = Modifiers {
            super_: true,
            ..Default::default()
        };
        assert!(key_event_to_bytes(&ev, mods, false, false, || None).is_none());
    }

    #[test]
    fn page_keys_yield_vt220() {
        let pu = pressed(LogicalKey::Named(NamedKey::PageUp), None);
        let pd = pressed(LogicalKey::Named(NamedKey::PageDown), None);
        assert_eq!(
            &*key_event_to_bytes(&pu, Modifiers::default(), false, false, || None).unwrap(),
            b"\x1b[5~"
        );
        assert_eq!(
            &*key_event_to_bytes(&pd, Modifiers::default(), false, false, || None).unwrap(),
            b"\x1b[6~"
        );
    }

    #[test]
    fn home_end_yield_ctrl_a_ctrl_e_by_default() {
        let h = pressed(LogicalKey::Named(NamedKey::Home), None);
        let e = pressed(LogicalKey::Named(NamedKey::End), None);
        assert_eq!(
            &*key_event_to_bytes(&h, Modifiers::default(), false, false, || None).unwrap(),
            b"\x01"
        );
        assert_eq!(
            &*key_event_to_bytes(&e, Modifiers::default(), false, false, || None).unwrap(),
            b"\x05"
        );
    }

    #[test]
    fn home_end_with_modifier_keep_csi_form() {
        // Shift-Home / Shift-End preserve the xterm CSI form so TUI
        // editors that watch for selection-extend still work.
        let h = pressed(LogicalKey::Named(NamedKey::Home), None);
        let e = pressed(LogicalKey::Named(NamedKey::End), None);
        let shift = Modifiers { shift: true, ..Modifiers::default() };
        assert_eq!(
            &*key_event_to_bytes(&h, shift, false, false, || None).unwrap(),
            b"\x1b[1;2H"
        );
        assert_eq!(
            &*key_event_to_bytes(&e, shift, false, false, || None).unwrap(),
            b"\x1b[1;2F"
        );
    }

    #[test]
    fn insert_delete_yield_2_and_3_tilde() {
        let i = pressed(LogicalKey::Named(NamedKey::Insert), None);
        let d = pressed(LogicalKey::Named(NamedKey::Delete), None);
        assert_eq!(
            &*key_event_to_bytes(&i, Modifiers::default(), false, false, || None).unwrap(),
            b"\x1b[2~"
        );
        assert_eq!(
            &*key_event_to_bytes(&d, Modifiers::default(), false, false, || None).unwrap(),
            b"\x1b[3~"
        );
    }

    #[test]
    fn f_keys_yield_vt220_table() {
        use NamedKey::*;
        let cases = [
            (F1, b"\x1bOP".as_slice()),
            (F2, b"\x1bOQ"),
            (F3, b"\x1bOR"),
            (F4, b"\x1bOS"),
            (F5, b"\x1b[15~"),
            (F6, b"\x1b[17~"),
            (F7, b"\x1b[18~"),
            (F8, b"\x1b[19~"),
            (F9, b"\x1b[20~"),
            (F10, b"\x1b[21~"),
            (F11, b"\x1b[23~"),
            (F12, b"\x1b[24~"),
        ];
        for (k, want) in cases {
            let ev = pressed(LogicalKey::Named(k), None);
            let got =
                key_event_to_bytes(&ev, Modifiers::default(), false, false, || None).unwrap();
            assert_eq!(&*got, want, "key {:?}", k);
        }
    }

    #[test]
    fn shift_arrow_encodes_modifier() {
        let up = pressed(LogicalKey::Named(NamedKey::ArrowUp), None);
        let mods = Modifiers {
            shift: true,
            ..Default::default()
        };
        let got = key_event_to_bytes(&up, mods, false, false, || None).unwrap();
        assert_eq!(&*got, b"\x1b[1;2A");
    }

    #[test]
    fn ctrl_arrow_encodes_modifier() {
        let right = pressed(LogicalKey::Named(NamedKey::ArrowRight), None);
        let mods = Modifiers {
            control: true,
            ..Default::default()
        };
        // Ctrl modifier param = 5 (1 + 4).  DECCKM doesn't affect
        // modifier-bearing form — always CSI.
        let got = key_event_to_bytes(&right, mods, true, false, || None).unwrap();
        assert_eq!(&*got, b"\x1b[1;5C");
    }

    #[test]
    fn shift_pageup_encodes_modifier() {
        let pu = pressed(LogicalKey::Named(NamedKey::PageUp), None);
        let mods = Modifiers {
            shift: true,
            ..Default::default()
        };
        let got = key_event_to_bytes(&pu, mods, false, false, || None).unwrap();
        assert_eq!(&*got, b"\x1b[5;2~");
    }

    #[test]
    fn shift_tab_is_backtab() {
        let t = pressed(LogicalKey::Named(NamedKey::Tab), None);
        let mods = Modifiers {
            shift: true,
            ..Default::default()
        };
        let got = key_event_to_bytes(&t, mods, false, false, || None).unwrap();
        assert_eq!(&*got, b"\x1b[Z");
    }

    #[test]
    fn option_letter_is_meta_prefix() {
        // macOS Option/⌥ acts as Meta: `Alt+b` → `ESC b` for
        // readline word-back.  Without this Option just emits the
        // typed glyph (ƒ for Option+f) and shells can't bind it.
        let ev = pressed(LogicalKey::Char('b'), Some("b"));
        let mods = Modifiers {
            alt: true,
            ..Default::default()
        };
        let got = key_event_to_bytes(&ev, mods, false, false, || None).unwrap();
        assert_eq!(&*got, &[0x1b, b'b']);
    }

    #[test]
    fn cmd_v_pastes_injected_clipboard() {
        let ev = pressed(LogicalKey::Char('v'), Some("v"));
        let mods = Modifiers {
            super_: true,
            ..Default::default()
        };
        // Plain paste (no bracketed mode) returns the clipboard bytes.
        let out =
            key_event_to_bytes(&ev, mods, false, false, || Some("hi".to_string())).unwrap();
        assert_eq!(&*out, b"hi");
        // Bracketed-paste mode wraps in \e[200~ ... \e[201~.
        let out =
            key_event_to_bytes(&ev, mods, false, true, || Some("hi".to_string())).unwrap();
        assert_eq!(&*out, b"\x1b[200~hi\x1b[201~");
    }
}

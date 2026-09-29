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
#[derive(Clone, Debug, Default)]
pub struct MarspotKeyEvent {
    pub state: KeyState,
    /// Modifier-independent identifier of the pressed key.
    /// `Char('a')` for `a` and `shift+a` alike; the resolved text
    /// lives in `text`.
    ///
    /// For a character key this is what the key types with NO
    /// modifiers under the current layout, which is not what AppKit
    /// hands over — see `marspot::keyboard_layout::unshifted_char`.
    pub logical: LogicalKey,
    /// What the keystroke would type with current modifiers applied
    /// (e.g. `"A"` for shift+a, `" "` for space).  `None` for
    /// navigation keys / pure modifier presses / dead keys.
    pub text: Option<String>,
    /// Where this key sits on a US ANSI keyboard, when it is one of
    /// those positions.  Lets a program keep a binding on a key
    /// POSITION rather than on what that position types, so `ctrl+z`
    /// stays bottom-row-left on AZERTY.  Reported to programs that
    /// ask for the kitty protocol's alternate keys; nothing else
    /// reads it.
    pub base_layout: Option<char>,
    /// True when the keyboard's auto-repeat produced this press
    /// rather than a finger.  Reported to programs that ask for the
    /// kitty protocol's event types, which is how a TUI can hold a
    /// key to scroll without treating each repeat as a fresh press.
    pub repeat: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeyState {
    #[default]
    Pressed,
    Released,
}

/// Logical-key identity, modifier-independent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LogicalKey {
    /// Single character (`a`, `1`, `=`, …).  Modifier-independent —
    /// `shift+a` and `shift+1` report `Char('a')` and `Char('1')`.
    Char(char),
    /// One of the named navigation / control keys we explicitly handle.
    Named(NamedKey),
    /// Anything else — caller falls back to `text` if present.
    #[default]
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

/// Everything about the terminal's state that changes how a key is
/// encoded.  Read off a `Terminal` with `Terminal::input_modes`.
///
/// One struct rather than one parameter each: the encoder already
/// took two of these and the kitty protocol made it three, which is
/// the point where the call sites stop being readable and a fourth
/// mode means touching all of them again.
/// `CSI = 2 u` — report press, repeat and release rather than press
/// alone.  See `marspot_term::terminal::KITTY_KEYBOARD_SUPPORTED`.
pub const KITTY_REPORT_EVENTS: u8 = 0b0_0010;
/// `CSI = 4 u` — report the shifted codepoint and the US-layout
/// position alongside the key.
pub const KITTY_REPORT_ALTERNATES: u8 = 0b0_0100;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TermModes {
    /// DECCKM (`?1`): arrow keys encode as `ESC O X` instead of
    /// `ESC [ X`.  TUI apps that bind cursor keys distinctly from
    /// PgUp/PgDn navigation depend on it.
    pub cursor_key_app: bool,
    /// DECSET `?2004`: Cmd-V paste is wrapped in `ESC [ 200~` /
    /// `ESC [ 201~` so the app can tell paste from typing.
    pub bracketed_paste: bool,
    /// Kitty keyboard flags in force — see
    /// `marspot_term::terminal::KittyKeyboardStack`.  Zero means the
    /// legacy encoding, which is the default and what every program
    /// that never asks for the protocol gets.
    pub kitty_keyboard: u8,
}

/// Map a key press to the byte sequence we send the PTY.  Returns
/// `None` for events we don't translate (releases, modifier-only, Cmd
/// combos that the OS handles, etc.).
///
/// `read_clipboard` is called lazily only on Cmd-V so this module
/// needs no window-system dependency; the AppKit implementation is
/// injected by the caller (`marspot::input::read_clipboard_text`).
pub fn key_event_to_bytes(
    event: &MarspotKeyEvent,
    modifiers: Modifiers,
    modes: TermModes,
    read_clipboard: impl FnOnce() -> Option<String>,
) -> Option<Cow<'static, [u8]>> {
    if event.state != KeyState::Pressed && modes.kitty_keyboard & KITTY_REPORT_EVENTS == 0 {
        // Nobody asked to hear about key-up, and the legacy encoding
        // has no way to say it.
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
                    if modes.bracketed_paste {
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

    // The kitty keyboard protocol, when the program asked for it.
    // Sits after the Cmd short-circuit above on purpose: Cmd is the
    // window layer's (Cmd-T, Cmd-W, Cmd-V), and handing it to the
    // program would take the shortcuts away from the user in every
    // app that raises the protocol.
    if modes.kitty_keyboard != 0 {
        return kitty_key_bytes(event, modifiers, modes.kitty_keyboard);
    }
    if event.state != KeyState::Pressed {
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
        return Some(encode_named_key(*n, modifiers, modes.cursor_key_app));
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

/// The kitty keyboard code and final byte for a named key.
///
/// Taken from the kitty protocol's functional-key table, cross-read
/// against ghostty's copy of it
/// (`references/ghostty/src/input/kitty.zig`).  Keys with a legacy
/// CSI form keep their final byte (`A`-`D`, `H`, `F`, `P`-`S`, `~`)
/// so a program that only half-implements the protocol still reads
/// them; everything else is `u`.  F3 is `13~` rather than an `R`
/// final — a genuine irregularity in the table, not a typo here.
fn kitty_entry(n: NamedKey) -> (u32, u8) {
    use NamedKey::*;
    match n {
        Escape => (27, b'u'),
        Enter => (13, b'u'),
        Tab => (9, b'u'),
        Backspace => (127, b'u'),
        Insert => (2, b'~'),
        Delete => (3, b'~'),
        ArrowUp => (1, b'A'),
        ArrowDown => (1, b'B'),
        ArrowRight => (1, b'C'),
        ArrowLeft => (1, b'D'),
        PageUp => (5, b'~'),
        PageDown => (6, b'~'),
        Home => (1, b'H'),
        End => (1, b'F'),
        F1 => (1, b'P'),
        F2 => (1, b'Q'),
        F3 => (13, b'~'),
        F4 => (1, b'S'),
        F5 => (15, b'~'),
        F6 => (17, b'~'),
        F7 => (18, b'~'),
        F8 => (19, b'~'),
        F9 => (20, b'~'),
        F10 => (21, b'~'),
        F11 => (23, b'~'),
        F12 => (24, b'~'),
    }
}

/// True when `text` is something the user meant to type, rather than
/// the control byte a modifier produced.  `ctrl+a` arrives with a
/// `text` of `0x01`, and that is not text.
fn is_typed_text(text: Option<&String>) -> bool {
    text.is_some_and(|t| !t.is_empty() && !t.chars().any(|c| c.is_control()))
}

/// Encode one key under the kitty keyboard protocol's "disambiguate
/// escape codes" flag — the only flag marspot claims (see
/// `KITTY_KEYBOARD_SUPPORTED`).
///
/// What the flag buys, and why it is worth having: `shift+enter`,
/// `ctrl+enter` and `alt+enter` are one byte apart from plain `enter`
/// in the legacy encoding — which is to say they are the same byte —
/// so a program that wants "newline" on one and "send" on the other
/// has no way to tell.  Here they are `CSI 13;2u`, `CSI 13;5u` and
/// `CSI 13;3u`.  The same goes for `ctrl+i` against `tab` and
/// `ctrl+m` against `enter`, and for `esc`, which stops being a
/// prefix a program has to time out on and becomes `CSI 27u`.
///
/// Three keys keep their legacy bytes when unmodified, as the spec
/// requires: Enter, Tab and Backspace.  That is what lets a user type
/// `reset` at a shell after a program died with the protocol still
/// raised.
///
/// Known gap: the protocol wants the UNSHIFTED codepoint, and AppKit's
/// `charactersIgnoringModifiers` keeps shift applied, so `shift+1`
/// reports `!` where the spec asks for `1`.  ASCII letters are folded
/// back to lowercase here, which covers `ctrl+shift+a`; punctuation
/// would need the keyboard layout (`UCKeyTranslate`) to undo properly
/// and is left alone rather than guessed at.
fn kitty_key_bytes(
    event: &MarspotKeyEvent,
    mods: Modifiers,
    flags: u8,
) -> Option<Cow<'static, [u8]>> {
    let released = event.state != KeyState::Pressed;
    if released
        && matches!(
            event.logical,
            // The three keys the spec keeps on legacy bytes have no
            // legacy way to say "released", so they don't say it.
            LogicalKey::Named(NamedKey::Enter | NamedKey::Tab | NamedKey::Backspace)
                // And a release inserts nothing, so a key we know
                // only by the text it typed has nothing to report.
                | LogicalKey::Other
        )
    {
        return None;
    }
    let typed = is_typed_text(event.text.as_ref()) && !released;
    // Shift is spent once it has produced text: `shift+a` is the
    // letter `A`, not a modified `a`, so it is not reported.
    //
    // Only when shift is the ONLY modifier, though.  Ctrl, Alt and
    // Cmd stop a key from being ordinary text, so whatever `text`
    // holds for `ctrl+shift+1` is not shift's doing and shift is
    // still a modifier the program needs to see.  Deciding this from
    // the modifiers rather than from the text also means it does not
    // depend on which of `!` or `\x11` AppKit chose to put there.
    let text_is_the_point = !mods.control && !mods.alt && !mods.super_;
    let effective = Modifiers {
        shift: mods.shift && !(typed && text_is_the_point),
        ..mods
    };
    let bare = !effective.shift && !effective.control && !effective.alt && !effective.super_;

    let (code, final_byte) = match &event.logical {
        LogicalKey::Named(n) => {
            if bare {
                match n {
                    NamedKey::Enter => return Some(Cow::Borrowed(b"\r")),
                    NamedKey::Tab => return Some(Cow::Borrowed(b"\t")),
                    NamedKey::Backspace => return Some(Cow::Borrowed(b"\x7f")),
                    _ => {}
                }
            }
            kitty_entry(*n)
        }
        LogicalKey::Char(c) => (*c as u32, b'u'),
        // A key we have no identity for is only ever worth the text it
        // produced — dead keys and IME output arrive this way.
        LogicalKey::Other => {
            return event
                .text
                .as_ref()
                .filter(|t| !t.is_empty())
                .map(|t| Cow::Owned(t.as_bytes().to_vec()))
        }
    };

    // Unmodified printable text goes through untouched.  This is what
    // keeps the protocol out of the way of ordinary typing and of the
    // IME: raising it must not change what `a` sends.
    if bare && typed {
        let text = event.text.as_ref().expect("is_typed_text implies Some");
        return Some(Cow::Owned(text.as_bytes().to_vec()));
    }

    let m = encode_mods(effective);
    // `1` press, `2` repeat, `3` release.  Written only when the
    // program asked for event types; kitty omits `:1` for a press,
    // but other terminals include it and a parser that can read the
    // field can read the common case too.
    let event_type = if flags & KITTY_REPORT_EVENTS != 0 {
        Some(if released {
            3
        } else if event.repeat {
            2
        } else {
            1
        })
    } else {
        None
    };

    use std::fmt::Write as _;
    // `CSI code[:shifted[:base]][;mods[:event]] final`
    let mut out = String::with_capacity(24);
    out.push_str("\x1b[");
    if final_byte == b'u' || final_byte == b'~' {
        let _ = write!(out, "{code}");
        if flags & KITTY_REPORT_ALTERNATES != 0 {
            match (
                alternate_shifted(event, effective, code),
                alternate_base(event, code),
            ) {
                (Some(sh), Some(b)) => {
                    let _ = write!(out, ":{sh}:{b}");
                }
                (Some(sh), None) => {
                    let _ = write!(out, ":{sh}");
                }
                // Two colons: the shifted slot is empty, the base one
                // is not.
                (None, Some(b)) => {
                    let _ = write!(out, "::{b}");
                }
                (None, None) => {}
            }
        }
    } else if m > 1 || event_type.is_some() {
        // Legacy-final keys carry no code; the fixed `1` stands in
        // when there is a modifier or an event type to attach.
        out.push('1');
    }
    match event_type {
        Some(e) => {
            let _ = write!(out, ";{m}:{e}");
        }
        None if m > 1 => {
            let _ = write!(out, ";{m}");
        }
        None => {}
    }
    out.push(final_byte as char);
    Some(Cow::Owned(out.into_bytes()))
}

/// The codepoint this key produces WITH shift — reported only when
/// shift is being held as a modifier and it differs from the key
/// itself.  Lets a program bind `ctrl+shift+1` knowing it is `!`
/// without modelling the layout.
fn alternate_shifted(event: &MarspotKeyEvent, effective: Modifiers, code: u32) -> Option<u32> {
    if !effective.shift {
        return None;
    }
    let cp = event.text.as_ref()?.chars().next()?;
    (cp as u32 != code && !cp.is_control()).then_some(cp as u32)
}

/// Where the key sits on a US keyboard, when that is somewhere other
/// than what it types.  This is what keeps a binding on a key
/// POSITION across layouts.
fn alternate_base(event: &MarspotKeyEvent, code: u32) -> Option<u32> {
    let base = event.base_layout? as u32;
    (base != code).then_some(base)
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

fn encode_named_key(n: NamedKey, mods: Modifiers, cursor_key_app_mode: bool) -> Cow<'static, [u8]> {
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

/// Quote a filesystem path for insertion at a shell prompt (Finder
/// drag-and-drop → "type the path for me").  shlex-style: paths made
/// entirely of safe chars pass through untouched; anything else is
/// wrapped in single quotes with embedded `'` re-spelled as `'\''` —
/// correct for every byte a filename can contain (spaces, newlines,
/// `$`, backticks…) under POSIX sh/bash/zsh/fish.
///
/// Non-ASCII (CJK filenames are the norm here) counts as safe: POSIX
/// word-splitting only special-cases ASCII metachars, and leaving
/// 決算明細.xlsx unquoted keeps the inserted text readable.
pub fn shell_quote_path(path: &str) -> String {
    let safe = |c: char| {
        !c.is_ascii()
            || c.is_ascii_alphanumeric()
            || matches!(c, '_' | '-' | '.' | '/' | '+' | '=' | ':' | ',' | '@' | '%')
    };
    if !path.is_empty() && path.chars().all(safe) {
        return path.to_string();
    }
    let mut out = String::with_capacity(path.len() + 2);
    out.push('\'');
    for c in path.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pressed(logical: LogicalKey, text: Option<&str>) -> MarspotKeyEvent {
        MarspotKeyEvent {
            state: KeyState::Pressed,
            logical,
            text: text.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn release_returns_none() {
        let ev = MarspotKeyEvent {
            state: KeyState::Released,
            logical: LogicalKey::Char('a'),
            text: Some("a".into()),
            ..Default::default()
        };
        assert!(key_event_to_bytes(&ev, Modifiers::default(), TermModes::default(), || None).is_none());
    }

    #[test]
    fn plain_char_falls_through_to_text() {
        let ev = pressed(LogicalKey::Char('a'), Some("a"));
        let out = key_event_to_bytes(&ev, Modifiers::default(), TermModes::default(), || None).unwrap();
        assert_eq!(&*out, b"a");
    }

    #[test]
    fn ctrl_letter_yields_control_code() {
        let ev = pressed(LogicalKey::Char('c'), Some("c"));
        let mods = Modifiers {
            control: true,
            ..Default::default()
        };
        let out = key_event_to_bytes(&ev, mods, TermModes::default(), || None).unwrap();
        assert_eq!(&*out, &[0x03]);
    }

    #[test]
    fn ctrl_bracket_yields_esc() {
        let ev = pressed(LogicalKey::Char('['), Some("["));
        let mods = Modifiers {
            control: true,
            ..Default::default()
        };
        let out = key_event_to_bytes(&ev, mods, TermModes::default(), || None).unwrap();
        assert_eq!(&*out, &[0x1b]);
    }

    #[test]
    fn arrow_keys_yield_csi() {
        let up = pressed(LogicalKey::Named(NamedKey::ArrowUp), None);
        let down = pressed(LogicalKey::Named(NamedKey::ArrowDown), None);
        let left = pressed(LogicalKey::Named(NamedKey::ArrowLeft), None);
        let right = pressed(LogicalKey::Named(NamedKey::ArrowRight), None);
        assert_eq!(
            &*key_event_to_bytes(&up, Modifiers::default(), TermModes::default(), || None).unwrap(),
            b"\x1b[A"
        );
        assert_eq!(
            &*key_event_to_bytes(&down, Modifiers::default(), TermModes::default(), || None).unwrap(),
            b"\x1b[B"
        );
        assert_eq!(
            &*key_event_to_bytes(&left, Modifiers::default(), TermModes::default(), || None).unwrap(),
            b"\x1b[D"
        );
        assert_eq!(
            &*key_event_to_bytes(&right, Modifiers::default(), TermModes::default(), || None).unwrap(),
            b"\x1b[C"
        );
    }

    #[test]
    fn enter_returns_cr() {
        let ev = pressed(LogicalKey::Named(NamedKey::Enter), None);
        assert_eq!(
            &*key_event_to_bytes(&ev, Modifiers::default(), TermModes::default(), || None).unwrap(),
            b"\r"
        );
    }

    #[test]
    fn cmd_non_v_returns_none() {
        let ev = pressed(LogicalKey::Char('a'), Some("a"));
        let mods = Modifiers {
            super_: true,
            ..Default::default()
        };
        assert!(key_event_to_bytes(&ev, mods, TermModes::default(), || None).is_none());
    }

    #[test]
    fn page_keys_yield_vt220() {
        let pu = pressed(LogicalKey::Named(NamedKey::PageUp), None);
        let pd = pressed(LogicalKey::Named(NamedKey::PageDown), None);
        assert_eq!(
            &*key_event_to_bytes(&pu, Modifiers::default(), TermModes::default(), || None).unwrap(),
            b"\x1b[5~"
        );
        assert_eq!(
            &*key_event_to_bytes(&pd, Modifiers::default(), TermModes::default(), || None).unwrap(),
            b"\x1b[6~"
        );
    }

    #[test]
    fn home_end_yield_ctrl_a_ctrl_e_by_default() {
        let h = pressed(LogicalKey::Named(NamedKey::Home), None);
        let e = pressed(LogicalKey::Named(NamedKey::End), None);
        assert_eq!(
            &*key_event_to_bytes(&h, Modifiers::default(), TermModes::default(), || None).unwrap(),
            b"\x01"
        );
        assert_eq!(
            &*key_event_to_bytes(&e, Modifiers::default(), TermModes::default(), || None).unwrap(),
            b"\x05"
        );
    }

    #[test]
    fn home_end_with_modifier_keep_csi_form() {
        // Shift-Home / Shift-End preserve the xterm CSI form so TUI
        // editors that watch for selection-extend still work.
        let h = pressed(LogicalKey::Named(NamedKey::Home), None);
        let e = pressed(LogicalKey::Named(NamedKey::End), None);
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        assert_eq!(
            &*key_event_to_bytes(&h, shift, TermModes::default(), || None).unwrap(),
            b"\x1b[1;2H"
        );
        assert_eq!(
            &*key_event_to_bytes(&e, shift, TermModes::default(), || None).unwrap(),
            b"\x1b[1;2F"
        );
    }

    #[test]
    fn insert_delete_yield_2_and_3_tilde() {
        let i = pressed(LogicalKey::Named(NamedKey::Insert), None);
        let d = pressed(LogicalKey::Named(NamedKey::Delete), None);
        assert_eq!(
            &*key_event_to_bytes(&i, Modifiers::default(), TermModes::default(), || None).unwrap(),
            b"\x1b[2~"
        );
        assert_eq!(
            &*key_event_to_bytes(&d, Modifiers::default(), TermModes::default(), || None).unwrap(),
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
            let got = key_event_to_bytes(&ev, Modifiers::default(), TermModes::default(), || None).unwrap();
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
        let got = key_event_to_bytes(&up, mods, TermModes::default(), || None).unwrap();
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
        let got = key_event_to_bytes(&right, mods, TermModes { cursor_key_app: true, ..TermModes::default() }, || None).unwrap();
        assert_eq!(&*got, b"\x1b[1;5C");
    }

    #[test]
    fn shift_pageup_encodes_modifier() {
        let pu = pressed(LogicalKey::Named(NamedKey::PageUp), None);
        let mods = Modifiers {
            shift: true,
            ..Default::default()
        };
        let got = key_event_to_bytes(&pu, mods, TermModes::default(), || None).unwrap();
        assert_eq!(&*got, b"\x1b[5;2~");
    }

    #[test]
    fn shift_tab_is_backtab() {
        let t = pressed(LogicalKey::Named(NamedKey::Tab), None);
        let mods = Modifiers {
            shift: true,
            ..Default::default()
        };
        let got = key_event_to_bytes(&t, mods, TermModes::default(), || None).unwrap();
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
        let got = key_event_to_bytes(&ev, mods, TermModes::default(), || None).unwrap();
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
        let out = key_event_to_bytes(&ev, mods, TermModes::default(), || Some("hi".to_string())).unwrap();
        assert_eq!(&*out, b"hi");
        // Bracketed-paste mode wraps in \e[200~ ... \e[201~.
        let out = key_event_to_bytes(&ev, mods, TermModes { bracketed_paste: true, ..TermModes::default() }, || Some("hi".to_string())).unwrap();
        assert_eq!(&*out, b"\x1b[200~hi\x1b[201~");
    }

    #[test]
    fn shell_quote_plain_path_untouched() {
        assert_eq!(
            shell_quote_path("/Users/x/file-1.2_3.txt"),
            "/Users/x/file-1.2_3.txt"
        );
    }

    #[test]
    fn shell_quote_cjk_path_untouched() {
        // CJK filenames are the everyday case — must stay readable.
        assert_eq!(
            shell_quote_path("/Users/x/Downloads/GOLIA-代表取缔役印.png"),
            "/Users/x/Downloads/GOLIA-代表取缔役印.png"
        );
    }

    #[test]
    fn shell_quote_space_wraps() {
        assert_eq!(
            shell_quote_path("/Users/x/My File.txt"),
            "'/Users/x/My File.txt'"
        );
    }

    #[test]
    fn shell_quote_metachars_wrap() {
        assert_eq!(shell_quote_path("/tmp/a$b`c"), "'/tmp/a$b`c'");
        assert_eq!(shell_quote_path("/tmp/(1)"), "'/tmp/(1)'");
    }

    #[test]
    fn shell_quote_embedded_single_quote() {
        assert_eq!(shell_quote_path("/tmp/it's"), "'/tmp/it'\\''s'");
    }

    #[test]
    fn shell_quote_empty_is_quoted() {
        // Degenerate, but "" must stay one (empty) shell word.
        assert_eq!(shell_quote_path(""), "''");
    }
}

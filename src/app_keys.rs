//! AppKit's key events, as the rest of the program names them.

use objc2::runtime::Sel;
use objc2_app_kit::{NSEvent, NSEventModifierFlags};

use crate::input::{KeyState, LogicalKey, MarspotKeyEvent, Modifiers, NamedKey};

pub(crate) fn nsevent_modifiers(event: &NSEvent) -> Modifiers {
    let flags = { event.modifierFlags() };
    Modifiers {
        shift: flags.contains(NSEventModifierFlags::Shift),
        control: flags.contains(NSEventModifierFlags::Control),
        alt: flags.contains(NSEventModifierFlags::Option),
        super_: flags.contains(NSEventModifierFlags::Command),
    }
}

pub(crate) fn nsevent_to_mars_key(event: &NSEvent, state: KeyState) -> Option<MarspotKeyEvent> {
    let key_code = { event.keyCode() };
    let logical = match key_code {
        // Carbon HIToolbox keyCodes — stable across macOS versions.
        // Reference: <HIToolbox/Events.h> kVK_* constants.
        0x24 => LogicalKey::Named(NamedKey::Enter),
        0x4C => LogicalKey::Named(NamedKey::Enter), // numeric-keypad Enter
        0x30 => LogicalKey::Named(NamedKey::Tab),
        0x33 => LogicalKey::Named(NamedKey::Backspace),
        0x35 => LogicalKey::Named(NamedKey::Escape),
        0x7E => LogicalKey::Named(NamedKey::ArrowUp),
        0x7D => LogicalKey::Named(NamedKey::ArrowDown),
        0x7B => LogicalKey::Named(NamedKey::ArrowLeft),
        0x7C => LogicalKey::Named(NamedKey::ArrowRight),
        // Navigation cluster — VT220 / xterm contract.
        0x74 => LogicalKey::Named(NamedKey::PageUp),
        0x79 => LogicalKey::Named(NamedKey::PageDown),
        0x73 => LogicalKey::Named(NamedKey::Home),
        0x77 => LogicalKey::Named(NamedKey::End),
        // Mac keyboards don't have a separate Insert; Help (kVK_Help =
        // 0x72) sits in the Insert position on classic ANSI layouts
        // and macOS apps that need Insert read it from there.
        0x72 => LogicalKey::Named(NamedKey::Insert),
        // ForwardDelete (kVK_ForwardDelete) — fn+Delete on laptops.
        // The regular `Delete` key (top-right of letter cluster) is
        // Backspace above (kVK_Delete = 0x33).
        0x75 => LogicalKey::Named(NamedKey::Delete),
        // F1-F12 — Apple's row is permuted, not sequential.
        0x7A => LogicalKey::Named(NamedKey::F1),
        0x78 => LogicalKey::Named(NamedKey::F2),
        0x63 => LogicalKey::Named(NamedKey::F3),
        0x76 => LogicalKey::Named(NamedKey::F4),
        0x60 => LogicalKey::Named(NamedKey::F5),
        0x61 => LogicalKey::Named(NamedKey::F6),
        0x62 => LogicalKey::Named(NamedKey::F7),
        0x64 => LogicalKey::Named(NamedKey::F8),
        0x65 => LogicalKey::Named(NamedKey::F9),
        0x6D => LogicalKey::Named(NamedKey::F10),
        0x67 => LogicalKey::Named(NamedKey::F11),
        0x6F => LogicalKey::Named(NamedKey::F12),
        _ => {
            // What this key types with NO modifiers.
            //
            // `charactersIgnoringModifiers` is not that, whatever the
            // name suggests: Apple's documentation says it ignores
            // every modifier "except for Shift", so `shift+1` comes
            // back as `!`.  Ask the layout instead, and keep the
            // AppKit string only for the keys the layout has no
            // printable answer for.
            crate::keyboard_layout::unshifted_char(key_code)
                .map(LogicalKey::Char)
                .unwrap_or_else(|| {
                    let s_opt = { event.charactersIgnoringModifiers() };
                    match s_opt.and_then(|s| s.to_string().chars().next()) {
                        Some(c) => LogicalKey::Char(c),
                        None => LogicalKey::Other,
                    }
                })
        }
    };

    // Resolved text — what the keystroke would type with current
    // modifiers applied.  None for navigation keys / pure modifier
    // presses.
    let flags = event.modifierFlags();
    let text = if matches!(logical, LogicalKey::Named(_)) {
        None
    } else if flags.contains(NSEventModifierFlags::Command) {
        // a chord: named by a character Cmd would otherwise hide
        use crate::keyboard_layout::{chord_char, typed_now, OPTION, SHIFT};
        let cmd_level = event.characters().and_then(|s| s.to_string().chars().next());
        let option = if flags.contains(NSEventModifierFlags::Option) { OPTION } else { 0 };
        let shift = flags.contains(NSEventModifierFlags::Shift);
        chord_char(cmd_level, shift, || typed_now(key_code, SHIFT | option)).map(String::from)
    } else {
        { event.characters() }.map(|s| s.to_string())
    };

    // Auto-repeat is only a thing for presses; AppKit reports it as
    // false on key-up regardless.
    let repeat = state == KeyState::Pressed && { event.isARepeat() };

    Some(MarspotKeyEvent {
        state,
        logical,
        text,
        repeat,
        base_layout: crate::keyboard_layout::us_layout_char(key_code),
    })
}

/// Map a Cocoa `NSStandardKeyBindingResponding` selector — fired by
/// `interpretKeyEvents` for non-text key presses — to our NamedKey
/// enum.  Returns None for selectors we don't translate (typically
/// `noop:`, fired for ctrl-letter combos that we already routed
/// through the raw path before entering IME).
pub(crate) fn selector_named_key(selector: Sel) -> Option<NamedKey> {
    // objc2 0.6: `Sel::name()` returns `&CStr`.  Selector names are
    // always ASCII, so the lossy fallback never actually fires.
    match selector.name().to_str().unwrap_or("") {
        "insertNewline:" | "insertLineBreak:" | "insertNewlineIgnoringFieldEditor:" => {
            Some(NamedKey::Enter)
        }
        "insertTab:" | "insertTabIgnoringFieldEditor:" => Some(NamedKey::Tab),
        "deleteBackward:" => Some(NamedKey::Backspace),
        "cancelOperation:" | "complete:" => Some(NamedKey::Escape),
        "moveUp:" | "moveUpAndModifySelection:" | "moveToBeginningOfDocument:" => {
            Some(NamedKey::ArrowUp)
        }
        "moveDown:" | "moveDownAndModifySelection:" | "moveToEndOfDocument:" => {
            Some(NamedKey::ArrowDown)
        }
        "moveLeft:" | "moveLeftAndModifySelection:" | "moveBackward:" => {
            Some(NamedKey::ArrowLeft)
        }
        "moveRight:" | "moveRightAndModifySelection:" | "moveForward:" => {
            Some(NamedKey::ArrowRight)
        }
        _ => None,
    }
}

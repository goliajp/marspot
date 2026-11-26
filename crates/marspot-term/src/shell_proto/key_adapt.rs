//! Between the wire key types and the `input_core` ones: both bins
//! (shell encodes, core decodes) share one translation.

use super::key_event::{
    KeyHead, WireKeyEvent, WireKeyState, WireLogicalKind, WireNamedKey, encode_key_parts,
    mods_from_byte, mods_to_byte,
};
use crate::input_core::{KeyState, LogicalKey, MarspotKeyEvent, Modifiers, NamedKey};

pub fn mods_to_struct(b: u8) -> Modifiers {
    let (shift, ctrl, alt, super_) = mods_from_byte(b);
    Modifiers {
        shift,
        control: ctrl,
        alt,
        super_,
    }
}

pub fn struct_to_mods_byte(m: Modifiers) -> u8 {
    mods_to_byte(m.shift, m.control, m.alt, m.super_)
}

fn head_of(ev: &MarspotKeyEvent, mods: Modifiers) -> KeyHead {
    let state = match ev.state {
        KeyState::Pressed => WireKeyState::Pressed,
        KeyState::Released => WireKeyState::Released,
    };
    let (kind, key_data) = match ev.logical {
        LogicalKey::Char(c) => (WireLogicalKind::Char, c as u32),
        LogicalKey::Named(n) => (WireLogicalKind::Named, named_to_u8(n) as u32),
        LogicalKey::Other => (WireLogicalKind::Other, 0),
    };
    KeyHead {
        state,
        mods: struct_to_mods_byte(mods),
        kind,
        key_data,
    }
}

pub fn event_to_wire(ev: &MarspotKeyEvent, mods: Modifiers) -> WireKeyEvent {
    let h = head_of(ev, mods);
    WireKeyEvent {
        state: h.state,
        mods: h.mods,
        kind: h.kind,
        key_data: h.key_data,
        text: ev.text.clone().unwrap_or_default(),
        repeat: ev.repeat,
        base_layout: ev.base_layout.map_or(0, |c| c as u32),
    }
}

/// Append the key frame payload for `ev` to `out`, the bytes
/// `encode_key_event(&event_to_wire(ev, mods), ..)` would give, without
/// the intermediate copy of the text.
pub fn encode_event(ev: &MarspotKeyEvent, mods: Modifiers, window_id: u32, out: &mut Vec<u8>) {
    let text = ev.text.as_deref().unwrap_or("");
    let base = ev.base_layout.map_or(0, |c| c as u32);
    encode_key_parts(head_of(ev, mods), text, ev.repeat, base, window_id, out);
}

pub fn wire_to_event(w: WireKeyEvent) -> (MarspotKeyEvent, Modifiers) {
    let state = match w.state {
        WireKeyState::Pressed => KeyState::Pressed,
        WireKeyState::Released => KeyState::Released,
    };
    let logical = match w.kind {
        WireLogicalKind::Char => char::from_u32(w.key_data)
            .map(LogicalKey::Char)
            .unwrap_or(LogicalKey::Other),
        WireLogicalKind::Named => WireNamedKey::from_u8(w.key_data as u8)
            .map(|n| LogicalKey::Named(u8_to_named(n)))
            .unwrap_or(LogicalKey::Other),
        WireLogicalKind::Other => LogicalKey::Other,
    };
    let text = if w.text.is_empty() {
        None
    } else {
        Some(w.text)
    };
    let ev = MarspotKeyEvent {
        state,
        logical,
        text,
        repeat: w.repeat,
        base_layout: char::from_u32(w.base_layout).filter(|c| *c != '\0'),
    };
    (ev, mods_to_struct(w.mods))
}

fn named_to_u8(n: NamedKey) -> u8 {
    match n {
        NamedKey::Enter => 0,
        NamedKey::Backspace => 1,
        NamedKey::Tab => 2,
        NamedKey::Escape => 3,
        NamedKey::ArrowUp => 4,
        NamedKey::ArrowDown => 5,
        NamedKey::ArrowLeft => 6,
        NamedKey::ArrowRight => 7,
        NamedKey::PageUp => 8,
        NamedKey::PageDown => 9,
        NamedKey::Home => 10,
        NamedKey::End => 11,
        NamedKey::Insert => 12,
        NamedKey::Delete => 13,
        NamedKey::F1 => 14,
        NamedKey::F2 => 15,
        NamedKey::F3 => 16,
        NamedKey::F4 => 17,
        NamedKey::F5 => 18,
        NamedKey::F6 => 19,
        NamedKey::F7 => 20,
        NamedKey::F8 => 21,
        NamedKey::F9 => 22,
        NamedKey::F10 => 23,
        NamedKey::F11 => 24,
        NamedKey::F12 => 25,
    }
}

fn u8_to_named(w: WireNamedKey) -> NamedKey {
    match w {
        WireNamedKey::Enter => NamedKey::Enter,
        WireNamedKey::Backspace => NamedKey::Backspace,
        WireNamedKey::Tab => NamedKey::Tab,
        WireNamedKey::Escape => NamedKey::Escape,
        WireNamedKey::ArrowUp => NamedKey::ArrowUp,
        WireNamedKey::ArrowDown => NamedKey::ArrowDown,
        WireNamedKey::ArrowLeft => NamedKey::ArrowLeft,
        WireNamedKey::ArrowRight => NamedKey::ArrowRight,
        WireNamedKey::PageUp => NamedKey::PageUp,
        WireNamedKey::PageDown => NamedKey::PageDown,
        WireNamedKey::Home => NamedKey::Home,
        WireNamedKey::End => NamedKey::End,
        WireNamedKey::Insert => NamedKey::Insert,
        WireNamedKey::Delete => NamedKey::Delete,
        WireNamedKey::F1 => NamedKey::F1,
        WireNamedKey::F2 => NamedKey::F2,
        WireNamedKey::F3 => NamedKey::F3,
        WireNamedKey::F4 => NamedKey::F4,
        WireNamedKey::F5 => NamedKey::F5,
        WireNamedKey::F6 => NamedKey::F6,
        WireNamedKey::F7 => NamedKey::F7,
        WireNamedKey::F8 => NamedKey::F8,
        WireNamedKey::F9 => NamedKey::F9,
        WireNamedKey::F10 => NamedKey::F10,
        WireNamedKey::F11 => NamedKey::F11,
        WireNamedKey::F12 => NamedKey::F12,
    }
}

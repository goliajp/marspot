//! The key event frame: its wire types and codec.

use std::io;

use super::trailing_window_id;
use crate::key_text::KeyText;

// ── KeyEvent ──
//
// Layout:
//   state    : u8   (0 = Pressed, 1 = Released)
//   mods     : u8   (bit 0 shift, 1 ctrl, 2 alt, 3 super)
//   kind     : u8   (0 = Char, 1 = Named, 2 = Other)
//   pad      : u8   (reserved, 0)
//   key_data : u32  Char → codepoint;  Named → discriminant in low byte
//   text_len : u16
//   text     : text_len bytes UTF-8

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum WireKeyState {
    #[default]
    Pressed = 0,
    Released = 1,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum WireLogicalKind {
    Char = 0,
    Named = 1,
    #[default]
    Other = 2,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum WireNamedKey {
    Enter = 0,
    Backspace = 1,
    Tab = 2,
    Escape = 3,
    ArrowUp = 4,
    ArrowDown = 5,
    ArrowLeft = 6,
    ArrowRight = 7,
    PageUp = 8,
    PageDown = 9,
    Home = 10,
    End = 11,
    Insert = 12,
    Delete = 13,
    F1 = 14,
    F2 = 15,
    F3 = 16,
    F4 = 17,
    F5 = 18,
    F6 = 19,
    F7 = 20,
    F8 = 21,
    F9 = 22,
    F10 = 23,
    F11 = 24,
    F12 = 25,
}

impl WireNamedKey {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Enter,
            1 => Self::Backspace,
            2 => Self::Tab,
            3 => Self::Escape,
            4 => Self::ArrowUp,
            5 => Self::ArrowDown,
            6 => Self::ArrowLeft,
            7 => Self::ArrowRight,
            8 => Self::PageUp,
            9 => Self::PageDown,
            10 => Self::Home,
            11 => Self::End,
            12 => Self::Insert,
            13 => Self::Delete,
            14 => Self::F1,
            15 => Self::F2,
            16 => Self::F3,
            17 => Self::F4,
            18 => Self::F5,
            19 => Self::F6,
            20 => Self::F7,
            21 => Self::F8,
            22 => Self::F9,
            23 => Self::F10,
            24 => Self::F11,
            25 => Self::F12,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct WireKeyEvent {
    pub state: WireKeyState,
    pub mods: u8,
    pub kind: WireLogicalKind,
    pub key_data: u32,
    /// What the key typed; inline unless it is a long IME commit.
    pub text: KeyText,
    /// Auto-repeat.  Rides in a byte that was padding, so a reader
    /// from before this field existed sees the padding it expected.
    pub repeat: bool,
    /// US-layout position of the key, 0 when unknown.  Appended after
    /// the window id — the one place a field can be added without
    /// moving anything an older reader looks at, since that reader
    /// takes the window id from a fixed offset and stops.
    pub base_layout: u32,
}

pub fn mods_to_byte(shift: bool, ctrl: bool, alt: bool, super_: bool) -> u8 {
    (shift as u8) | ((ctrl as u8) << 1) | ((alt as u8) << 2) | ((super_ as u8) << 3)
}

pub fn mods_from_byte(b: u8) -> (bool, bool, bool, bool) {
    (
        b & 0b0001 != 0,
        b & 0b0010 != 0,
        b & 0b0100 != 0,
        b & 0b1000 != 0,
    )
}

/// Bit 0 of the byte at offset 3, which was padding until auto-repeat
/// needed somewhere to live.  A reader from before this wrote a zero
/// there and ignored it, so an old writer's frames decode as "not a
/// repeat" — which is the truth, since it had no way to know.
const KEY_FLAG_REPEAT: u8 = 0b0000_0001;

/// Append the payload of a key frame for `ev` to `out`.
///
/// Appends rather than returns so a caller can put a frame header in
/// front and reuse one buffer for every key: the key path allocates
/// nothing once `out` has grown to a key's size.
pub fn encode_key_event(ev: &WireKeyEvent, window_id: u32, out: &mut Vec<u8>) {
    let head = KeyHead {
        state: ev.state,
        mods: ev.mods,
        kind: ev.kind,
        key_data: ev.key_data,
    };
    encode_key_parts(head, &ev.text, ev.repeat, ev.base_layout, window_id, out);
}

/// The fixed fields ahead of a key frame's text.
#[derive(Clone, Copy)]
pub(super) struct KeyHead {
    pub(super) state: WireKeyState,
    pub(super) mods: u8,
    pub(super) kind: WireLogicalKind,
    pub(super) key_data: u32,
}

pub(super) fn encode_key_parts(
    head: KeyHead,
    text: &str,
    repeat: bool,
    base_layout: u32,
    window_id: u32,
    out: &mut Vec<u8>,
) {
    let text_bytes = text.as_bytes();
    out.reserve(18 + text_bytes.len());
    out.push(head.state as u8);
    out.push(head.mods);
    out.push(head.kind as u8);
    out.push(if repeat { KEY_FLAG_REPEAT } else { 0 });
    out.extend_from_slice(&head.key_data.to_le_bytes());
    out.extend_from_slice(&(text_bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(text_bytes);
    out.extend_from_slice(&window_id.to_le_bytes());
    out.extend_from_slice(&base_layout.to_le_bytes());
}

pub fn decode_key_event(payload: &[u8]) -> io::Result<(WireKeyEvent, u32)> {
    if payload.len() < 10 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("KeyEvent payload < 10 bytes (got {})", payload.len()),
        ));
    }
    let state = match payload[0] {
        0 => WireKeyState::Pressed,
        1 => WireKeyState::Released,
        v => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("KeyEvent bad state {v}"),
            ));
        }
    };
    let mods = payload[1];
    let kind = match payload[2] {
        0 => WireLogicalKind::Char,
        1 => WireLogicalKind::Named,
        2 => WireLogicalKind::Other,
        v => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("KeyEvent bad kind {v}"),
            ));
        }
    };
    let key_data = u32::from_le_bytes(payload[4..8].try_into().unwrap());
    let text_len = u16::from_le_bytes(payload[8..10].try_into().unwrap()) as usize;
    if payload.len() < 10 + text_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "KeyEvent payload truncated",
        ));
    }
    let text = std::str::from_utf8(&payload[10..10 + text_len])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
        .into();
    let repeat = payload[3] & KEY_FLAG_REPEAT != 0;
    // Absent from a writer that predates it, which is not an error:
    // zero means "no idea where this key sits", and every reader of
    // it already has to handle that.
    let base_layout = payload
        .get(10 + text_len + 4..10 + text_len + 8)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        .unwrap_or(0);
    Ok((
        WireKeyEvent {
            state,
            mods,
            kind,
            key_data,
            text,
            repeat,
            base_layout,
        },
        trailing_window_id(payload, 10 + text_len),
    ))
}

#[cfg(test)]
#[path = "key_event_tests.rs"]
mod tests;

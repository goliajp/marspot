//! Shell ↔ core wire protocol.
//!
//! Two channels carry traffic between the marspot-shell outer process
//! and the marspot-core renderer:
//!
//! 1. **IOSurface** (one-way, GPU memory) — the core writes pixels,
//!    the shell reads pixels.  No protocol; just a shared kernel
//!    object reached by ID (see `iosurface.rs`).
//! 2. **Control socket** (bidirectional, `socketpair(AF_UNIX, SOCK_STREAM)`)
//!    — keyboard / mouse / focus / resize events flow shell → core,
//!    status / health flows core → shell.  This module defines that
//!    wire format.
//!
//! Frame layout mirrors `shelld_proto`:
//!
//! ```text
//! [magic   : u32 LE  = b"MSPC"     ]  // marspot shell↔core
//! [msg_type: u32 LE                 ]
//! [len     : u32 LE  (= payload len)]
//! [payload : len bytes              ]
//! ```
//!
//! Little-endian throughout (Apple Silicon native, no byte-swap).
//! Magic ≠ `MSPS` so a misrouted shelld frame trips magic check
//! cleanly instead of being misinterpreted.
//!
//! Versioning is a `Hello`/`HelloAck` handshake carrying a u32; the
//! peers refuse to talk if they disagree.  Adding a new `MsgType`
//! with a fresh code is backward compatible; changing an existing
//! payload layout is a major bump.

use std::io::{self, Read, Write};

// ───────────────────────────────────────────────────────────────────
// Environment variables — used during the brief window between
// fork+exec and the control socket coming online.  The shell sets
// these before `Command::spawn`; the core reads them from `std::env`
// in its `main`.
// ───────────────────────────────────────────────────────────────────

/// Globally-unique IOSurface ID the core should look up.  `u32`
/// decimal.
pub const ENV_SURFACE_ID: &str = "MARSPOT_SHELL_SURFACE_ID";

/// Surface width in **physical pixels** at attach time.  Decimal
/// `usize`.
pub const ENV_SURFACE_WIDTH: &str = "MARSPOT_SHELL_SURFACE_W";

/// Surface height in **physical pixels** at attach time.  Decimal
/// `usize`.
pub const ENV_SURFACE_HEIGHT: &str = "MARSPOT_SHELL_SURFACE_H";

/// Backing scale factor of the shell window's screen (1.0, 2.0, …).
pub const ENV_SURFACE_SCALE: &str = "MARSPOT_SHELL_SURFACE_SCALE";

/// File-descriptor number where the core inherits the control-socket
/// peer end.  Defaults to `3` (the first non-stdio fd) so we don't
/// stomp on stdin/stdout/stderr.  Decimal `i32`.
pub const ENV_CONTROL_FD: &str = "MARSPOT_SHELL_CONTROL_FD";
pub const DEFAULT_CONTROL_FD: i32 = 3;

// ───────────────────────────────────────────────────────────────────
// Wire constants
// ───────────────────────────────────────────────────────────────────

pub const MAGIC: u32 = u32::from_le_bytes(*b"MSPC");
pub const PROTO_VERSION: u32 = 1;
pub const HEADER_LEN: usize = 12;
/// Sanity ceiling.  Input frames are tiny (≤256 B); a generous cap
/// rules out runaway allocations from corrupted lengths.
pub const MAX_PAYLOAD_LEN: usize = 64 * 1024;

#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MsgType {
    // ── lifecycle (1..=9) ──
    Hello = 1,
    HelloAck = 2,
    /// shell → core: liveness probe.  Payload = u32 nonce; core
    /// echoes it back in `Pong` so the shell can match request/reply
    /// and ignore stale Pongs after a respawn.
    Ping = 3,
    /// core → shell: response to `Ping`.  Payload = u32 nonce.
    Pong = 4,
    // ── input (10..=29) ──
    KeyEvent = 10,
    MouseDown = 11,
    MouseDrag = 12,
    MouseUp = 13,
    Scroll = 14,
    Preedit = 15,
    // ── window state (30..=49) ──
    Focus = 30,
    Resize = 31,
    /// core → shell: "I have rendered the first frame to the new
    /// IOSurface; you may swap the presenter now."  Payload is the
    /// surface ID the core just confirmed (u32 LE).  The shell
    /// ignores frames whose ID doesn't match its currently-pending
    /// surface — see Step 4's resize state machine.
    SurfaceReady = 32,
    /// core → shell: focused-pane caret rect in view-local physical
    /// pixels (top-left origin), or "no caret".  The shell feeds it
    /// to `MarspotAppCtx::set_caret_rect_phys` so the IME candidate
    /// window anchors under the caret even though the composition
    /// state lives in the core process.  Payload: u8 present flag +
    /// 4 × f64 LE (x, y, w, h) when present.
    CaretRect = 33,
    /// L3 (`marspot-session`) → L2 (`marspot-core`): "I published a new
    /// grid snapshot into shared memory; come read it."  Empty payload —
    /// a pure wake so L2 stays event-driven (idle CPU ~0) instead of
    /// polling the shm seq every frame.  L2 coalesces a burst of these
    /// into a single re-read + render.  See `docs/per-session-l3.md`.
    GridReady = 34,
    // ── error (200..=255) ──
    Error = 200,
}

impl MsgType {
    pub fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            1 => MsgType::Hello,
            2 => MsgType::HelloAck,
            3 => MsgType::Ping,
            4 => MsgType::Pong,
            10 => MsgType::KeyEvent,
            11 => MsgType::MouseDown,
            12 => MsgType::MouseDrag,
            13 => MsgType::MouseUp,
            14 => MsgType::Scroll,
            15 => MsgType::Preedit,
            30 => MsgType::Focus,
            31 => MsgType::Resize,
            32 => MsgType::SurfaceReady,
            33 => MsgType::CaretRect,
            34 => MsgType::GridReady,
            200 => MsgType::Error,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub msg_type: MsgType,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(msg_type: MsgType, payload: Vec<u8>) -> Self {
        Self { msg_type, payload }
    }

    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<usize> {
        let mut header = [0u8; HEADER_LEN];
        header[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        header[4..8].copy_from_slice(&(self.msg_type as u32).to_le_bytes());
        header[8..12].copy_from_slice(&(self.payload.len() as u32).to_le_bytes());
        w.write_all(&header)?;
        if !self.payload.is_empty() {
            w.write_all(&self.payload)?;
        }
        Ok(HEADER_LEN + self.payload.len())
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Option<Self>> {
        let mut header = [0u8; HEADER_LEN];
        match read_exact_or_eof(r, &mut header)? {
            ReadEnd::Eof => return Ok(None),
            ReadEnd::Full => {}
        }
        let magic = u32::from_le_bytes(header[0..4].try_into().unwrap());
        if magic != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad magic 0x{:08x}, expected 0x{:08x}", magic, MAGIC),
            ));
        }
        let type_raw = u32::from_le_bytes(header[4..8].try_into().unwrap());
        let msg_type = MsgType::from_u32(type_raw).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown msg_type {}", type_raw),
            )
        })?;
        let len = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
        if len > MAX_PAYLOAD_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("payload len {} exceeds cap {}", len, MAX_PAYLOAD_LEN),
            ));
        }
        let mut payload = vec![0u8; len];
        if len > 0 {
            r.read_exact(&mut payload)?;
        }
        Ok(Some(Frame { msg_type, payload }))
    }
}

enum ReadEnd {
    Full,
    Eof,
}

fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<ReadEnd> {
    let mut off = 0;
    while off < buf.len() {
        match r.read(&mut buf[off..]) {
            Ok(0) => {
                if off == 0 {
                    return Ok(ReadEnd::Eof);
                }
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF mid-frame"));
            }
            Ok(n) => off += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(ReadEnd::Full)
}

// ───────────────────────────────────────────────────────────────────
// Payload codecs — one encode/decode pair per MsgType.
// All multibyte integers little-endian.
// ───────────────────────────────────────────────────────────────────

/// HELLO payload: protocol version the sender speaks (u32).
pub fn encode_hello(version: u32) -> Vec<u8> {
    version.to_le_bytes().to_vec()
}
pub fn decode_hello(payload: &[u8]) -> io::Result<u32> {
    if payload.len() != 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "HELLO payload != 4 bytes",
        ));
    }
    Ok(u32::from_le_bytes(payload[0..4].try_into().unwrap()))
}

/// HELLO_ACK: same shape as HELLO — version the receiver agrees on
/// (echoed back, so the sender knows both sides are aligned).
pub fn encode_hello_ack(version: u32) -> Vec<u8> {
    version.to_le_bytes().to_vec()
}
pub fn decode_hello_ack(payload: &[u8]) -> io::Result<u32> {
    decode_hello(payload)
}

/// PING / PONG payload: u32 LE nonce.  Shell rotates the nonce so it
/// can ignore Pongs from a previous Ping that arrived after a timeout.
pub fn encode_ping(nonce: u32) -> Vec<u8> {
    nonce.to_le_bytes().to_vec()
}
pub fn decode_ping(payload: &[u8]) -> io::Result<u32> {
    if payload.len() != 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PING payload != 4 bytes",
        ));
    }
    Ok(u32::from_le_bytes(payload[0..4].try_into().unwrap()))
}
pub fn encode_pong(nonce: u32) -> Vec<u8> {
    nonce.to_le_bytes().to_vec()
}
pub fn decode_pong(payload: &[u8]) -> io::Result<u32> {
    decode_ping(payload)
}

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

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WireKeyState {
    Pressed = 0,
    Released = 1,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WireLogicalKind {
    Char = 0,
    Named = 1,
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
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct WireKeyEvent {
    pub state: WireKeyState,
    pub mods: u8,
    pub kind: WireLogicalKind,
    pub key_data: u32,
    pub text: String,
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

pub fn encode_key_event(ev: &WireKeyEvent) -> Vec<u8> {
    let text_bytes = ev.text.as_bytes();
    let mut out = Vec::with_capacity(10 + text_bytes.len());
    out.push(ev.state as u8);
    out.push(ev.mods);
    out.push(ev.kind as u8);
    out.push(0); // pad
    out.extend_from_slice(&ev.key_data.to_le_bytes());
    out.extend_from_slice(&(text_bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(text_bytes);
    out
}

pub fn decode_key_event(payload: &[u8]) -> io::Result<WireKeyEvent> {
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
            ))
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
            ))
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
        .to_string();
    Ok(WireKeyEvent {
        state,
        mods,
        kind,
        key_data,
        text,
    })
}

// ── Mouse / Scroll ──

pub fn encode_mouse(x: f64, y: f64, mods: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(17);
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    out.push(mods);
    out
}

pub fn decode_mouse(payload: &[u8]) -> io::Result<(f64, f64, u8)> {
    if payload.len() < 17 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "mouse payload < 17 bytes",
        ));
    }
    let x = f64::from_le_bytes(payload[0..8].try_into().unwrap());
    let y = f64::from_le_bytes(payload[8..16].try_into().unwrap());
    let mods = payload[16];
    Ok((x, y, mods))
}

pub fn encode_scroll(dx: f64, dy: f64, precise: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(17);
    out.extend_from_slice(&dx.to_le_bytes());
    out.extend_from_slice(&dy.to_le_bytes());
    out.push(if precise { 1 } else { 0 });
    out
}

pub fn decode_scroll(payload: &[u8]) -> io::Result<(f64, f64, bool)> {
    if payload.len() < 17 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "scroll payload < 17 bytes",
        ));
    }
    let dx = f64::from_le_bytes(payload[0..8].try_into().unwrap());
    let dy = f64::from_le_bytes(payload[8..16].try_into().unwrap());
    let precise = payload[16] != 0;
    Ok((dx, dy, precise))
}

// ── Focus / Resize / Preedit ──

pub fn encode_focus(focused: bool) -> Vec<u8> {
    vec![if focused { 1 } else { 0 }]
}
pub fn decode_focus(payload: &[u8]) -> io::Result<bool> {
    if payload.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "focus payload empty",
        ));
    }
    Ok(payload[0] != 0)
}

/// Resize payload layout:
///
/// ```text
/// [new_surface_id : u32 LE]
/// [w_phys         : f64 LE]
/// [h_phys         : f64 LE]
/// [scale          : f64 LE]
/// ```
///
/// The shell creates a fresh IOSurface at the new dimensions, then
/// sends this frame.  The core looks the surface up, rebuilds its
/// render target and layout, and acks with `SurfaceReady(id)`.
pub fn encode_resize(new_surface_id: u32, w_phys: f64, h_phys: f64, scale: f64) -> Vec<u8> {
    let mut out = Vec::with_capacity(28);
    out.extend_from_slice(&new_surface_id.to_le_bytes());
    out.extend_from_slice(&w_phys.to_le_bytes());
    out.extend_from_slice(&h_phys.to_le_bytes());
    out.extend_from_slice(&scale.to_le_bytes());
    out
}

pub fn decode_resize(payload: &[u8]) -> io::Result<(u32, f64, f64, f64)> {
    if payload.len() < 28 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "resize payload < 28 bytes",
        ));
    }
    let id = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    let w = f64::from_le_bytes(payload[4..12].try_into().unwrap());
    let h = f64::from_le_bytes(payload[12..20].try_into().unwrap());
    let s = f64::from_le_bytes(payload[20..28].try_into().unwrap());
    Ok((id, w, h, s))
}

/// SurfaceReady payload: the IOSurface ID the core has just rendered
/// to (u32 LE).  The shell uses this to switch its presenter from the
/// previous (stretched) surface to the new (crisp) one.
pub fn encode_surface_ready(surface_id: u32) -> Vec<u8> {
    surface_id.to_le_bytes().to_vec()
}

pub fn decode_surface_ready(payload: &[u8]) -> io::Result<u32> {
    if payload.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "surface_ready payload < 4 bytes",
        ));
    }
    Ok(u32::from_le_bytes(payload[0..4].try_into().unwrap()))
}

/// CaretRect payload: `[present: u8]` then, when present == 1,
/// `4 × f64 LE` (x, y, w, h) in view-local physical pixels with a
/// top-left origin — the exact tuple `set_caret_rect_phys` takes.
pub fn encode_caret_rect(rect: Option<(f64, f64, f64, f64)>) -> Vec<u8> {
    match rect {
        None => vec![0],
        Some((x, y, w, h)) => {
            let mut out = Vec::with_capacity(33);
            out.push(1);
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
            out.extend_from_slice(&w.to_le_bytes());
            out.extend_from_slice(&h.to_le_bytes());
            out
        }
    }
}

pub fn decode_caret_rect(payload: &[u8]) -> io::Result<Option<(f64, f64, f64, f64)>> {
    if payload.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "caret_rect payload empty",
        ));
    }
    if payload[0] == 0 {
        return Ok(None);
    }
    if payload.len() < 33 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "caret_rect payload < 33 bytes",
        ));
    }
    let f = |i: usize| f64::from_le_bytes(payload[i..i + 8].try_into().unwrap());
    Ok(Some((f(1), f(9), f(17), f(25))))
}

pub fn encode_preedit(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(2 + bytes.len());
    out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(bytes);
    out
}

pub fn decode_preedit(payload: &[u8]) -> io::Result<String> {
    if payload.len() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "preedit payload < 2 bytes",
        ));
    }
    let len = u16::from_le_bytes(payload[0..2].try_into().unwrap()) as usize;
    if payload.len() < 2 + len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "preedit payload truncated",
        ));
    }
    std::str::from_utf8(&payload[2..2 + len])
        .map(|s| s.to_string())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

// ───────────────────────────────────────────────────────────────────
// Adapters between the wire types and the high-level `input` types.
// Kept in this module so both bins (shell encodes, core decodes)
// share the same translation, and a future bin can pick up either
// half without redefining it.
// ───────────────────────────────────────────────────────────────────

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

pub fn event_to_wire(ev: &MarspotKeyEvent, mods: Modifiers) -> WireKeyEvent {
    let state = match ev.state {
        KeyState::Pressed => WireKeyState::Pressed,
        KeyState::Released => WireKeyState::Released,
    };
    let (kind, key_data) = match ev.logical {
        LogicalKey::Char(c) => (WireLogicalKind::Char, c as u32),
        LogicalKey::Named(n) => (WireLogicalKind::Named, named_to_u8(n) as u32),
        LogicalKey::Other => (WireLogicalKind::Other, 0),
    };
    WireKeyEvent {
        state,
        mods: struct_to_mods_byte(mods),
        kind,
        key_data,
        text: ev.text.clone().unwrap_or_default(),
    }
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
    }
}

// ───────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frame_roundtrip() {
        let f = Frame::new(MsgType::KeyEvent, vec![1, 2, 3, 4, 5]);
        let mut buf = Vec::new();
        f.write_to(&mut buf).unwrap();
        let mut cur = Cursor::new(buf);
        let read = Frame::read_from(&mut cur).unwrap().unwrap();
        assert_eq!(read.msg_type, MsgType::KeyEvent);
        assert_eq!(read.payload, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn frame_eof_clean_between_frames() {
        let mut cur: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        assert!(Frame::read_from(&mut cur).unwrap().is_none());
    }

    #[test]
    fn frame_bad_magic_errors() {
        let mut buf = vec![0u8; 12];
        buf[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        let mut cur = Cursor::new(buf);
        let err = Frame::read_from(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn hello_roundtrip() {
        let p = encode_hello(7);
        assert_eq!(decode_hello(&p).unwrap(), 7);
    }

    #[test]
    fn ping_pong_roundtrip() {
        assert_eq!(decode_ping(&encode_ping(0xCAFE_BABE)).unwrap(), 0xCAFE_BABE);
        assert_eq!(decode_pong(&encode_pong(42)).unwrap(), 42);
    }

    #[test]
    fn mods_roundtrip() {
        for b in 0u8..16 {
            let (s, c, a, u) = mods_from_byte(b);
            assert_eq!(mods_to_byte(s, c, a, u), b);
        }
    }

    #[test]
    fn key_event_char_roundtrip() {
        let ev = WireKeyEvent {
            state: WireKeyState::Pressed,
            mods: mods_to_byte(false, true, false, false),
            kind: WireLogicalKind::Char,
            key_data: 'a' as u32,
            text: "a".to_string(),
        };
        let p = encode_key_event(&ev);
        let back = decode_key_event(&p).unwrap();
        assert_eq!(back.state, ev.state);
        assert_eq!(back.mods, ev.mods);
        assert_eq!(back.kind, ev.kind);
        assert_eq!(back.key_data, ev.key_data);
        assert_eq!(back.text, ev.text);
    }

    #[test]
    fn key_event_named_arrow_roundtrip() {
        let ev = WireKeyEvent {
            state: WireKeyState::Pressed,
            mods: 0,
            kind: WireLogicalKind::Named,
            key_data: WireNamedKey::ArrowUp as u32,
            text: String::new(),
        };
        let p = encode_key_event(&ev);
        let back = decode_key_event(&p).unwrap();
        assert_eq!(back.kind, WireLogicalKind::Named);
        assert_eq!(
            WireNamedKey::from_u8(back.key_data as u8),
            Some(WireNamedKey::ArrowUp)
        );
        assert_eq!(back.text, "");
    }

    #[test]
    fn mouse_roundtrip() {
        let p = encode_mouse(123.5, -45.25, 0b0010);
        let (x, y, m) = decode_mouse(&p).unwrap();
        assert_eq!(x, 123.5);
        assert_eq!(y, -45.25);
        assert_eq!(m, 0b0010);
    }

    #[test]
    fn scroll_roundtrip() {
        let p = encode_scroll(0.0, 12.5, true);
        let (dx, dy, precise) = decode_scroll(&p).unwrap();
        assert_eq!(dx, 0.0);
        assert_eq!(dy, 12.5);
        assert!(precise);
    }

    #[test]
    fn focus_roundtrip() {
        assert!(decode_focus(&encode_focus(true)).unwrap());
        assert!(!decode_focus(&encode_focus(false)).unwrap());
    }

    #[test]
    fn resize_roundtrip() {
        let p = encode_resize(0xDEAD_BEEF, 1200.0, 800.0, 2.0);
        let (id, w, h, s) = decode_resize(&p).unwrap();
        assert_eq!(id, 0xDEAD_BEEF);
        assert_eq!(w, 1200.0);
        assert_eq!(h, 800.0);
        assert_eq!(s, 2.0);
    }

    #[test]
    fn surface_ready_roundtrip() {
        let p = encode_surface_ready(0xCAFE_BABE);
        assert_eq!(decode_surface_ready(&p).unwrap(), 0xCAFE_BABE);
    }

    #[test]
    fn preedit_roundtrip() {
        let p = encode_preedit("你好");
        let back = decode_preedit(&p).unwrap();
        assert_eq!(back, "你好");
    }

    #[test]
    fn grid_ready_is_an_empty_poke() {
        // GridReady carries no payload — it's a pure L3→L2 wake. A
        // frame round-trips through the wire with type preserved and a
        // zero-length body.
        assert_eq!(MsgType::from_u32(34), Some(MsgType::GridReady));
        let f = Frame::new(MsgType::GridReady, Vec::new());
        let mut buf = Vec::new();
        f.write_to(&mut buf).unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let back = Frame::read_from(&mut cur).unwrap().unwrap();
        assert_eq!(back.msg_type, MsgType::GridReady);
        assert!(back.payload.is_empty());
    }
}

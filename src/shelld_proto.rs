//! Wire format spoken between marspot (client) and marspot-shelld
//! (server) over a unix socket.
//!
//! Every message is a `Frame`:
//!
//! ```text
//! [magic   : u32 LE  = b"MSPS"      ]
//! [msg_type: u32 LE                  ]
//! [len     : u32 LE  (= payload len) ]
//! [payload : len bytes               ]
//! ```
//!
//! Little-endian throughout so reading on Apple Silicon costs no
//! byte-swap.  Magic is constant so a desynced reader resyncs on
//! the next frame boundary instead of mis-decoding garbage as a
//! length.  Length is u32 — current cap `MAX_PAYLOAD_LEN` 8 MiB
//! covers the largest realistic message (bytelog replay chunk)
//! and bounds attacker-controllable allocation.
//!
//! Versioning: the HELLO exchange carries a u32 protocol version on
//! each side; mismatch is fatal (server closes the connection).
//! Version-bumping rule: any change to the wire layout — adding a
//! payload field, redefining a type code, changing endianness — is
//! a major bump.  Adding a new message type with a fresh code is
//! optional: clients/servers that don't recognise it can ignore the
//! frame body via `len`.
//!
//! Authoring constraints:
//! - Encode/decode allocates only the message payload; the frame
//!   header is a fixed `[u8; 12]` on the stack.
//! - No serde / no external codec crates — self-build.
//! - All multi-byte integers go through `u32::from_le_bytes` /
//!   `to_le_bytes` to keep the code endianness-explicit.
//!
//! Phase 2 ships HELLO + LIST_SESSIONS.  Later phases add the rest.

use std::io::{self, Read, Write};

/// Magic prefix on every frame, little-endian "MSPS".
pub const MAGIC: u32 = u32::from_le_bytes(*b"MSPS");

/// Current protocol version.  Bumped on any incompatible wire change.
pub const PROTO_VERSION: u32 = 1;

/// Per-frame header length (magic + type + len, no payload).
pub const HEADER_LEN: usize = 12;

/// Sanity ceiling so a corrupted length field can't make us
/// allocate gigabytes.  Larger than the largest realistic message
/// (bytelog replay chunks are sent in batches well under this).
pub const MAX_PAYLOAD_LEN: usize = 8 * 1024 * 1024;

/// Message type codes.  Reserved ranges so future additions stay
/// readable: 1..=99 = lifecycle / control, 100..=199 = data flow,
/// 200..=255 = error / diagnostic.
#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MsgType {
    Hello = 1,
    HelloAck = 2,
    ListSessions = 3,
    ListSessionsReply = 4,
    NewSession = 5,
    NewSessionReply = 6,
    Attach = 7,
    Detach = 8,
    Kill = 9,
    Resize = 10,
    // 100..=199 reserved for high-volume data flow so a future
    // dispatcher can branch on `type >= 100` cheaply.
    Data = 100,
    Input = 101,
    Error = 200,
}

impl MsgType {
    pub fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            1 => MsgType::Hello,
            2 => MsgType::HelloAck,
            3 => MsgType::ListSessions,
            4 => MsgType::ListSessionsReply,
            5 => MsgType::NewSession,
            6 => MsgType::NewSessionReply,
            7 => MsgType::Attach,
            8 => MsgType::Detach,
            9 => MsgType::Kill,
            10 => MsgType::Resize,
            100 => MsgType::Data,
            101 => MsgType::Input,
            200 => MsgType::Error,
            _ => return None,
        })
    }
}

/// One on-the-wire frame.  Payload is owned (Vec<u8>) so a decoded
/// frame can be passed around without lifetimes; the per-frame
/// allocation cost is negligible compared to the syscall.
#[derive(Debug, Clone)]
pub struct Frame {
    pub msg_type: MsgType,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(msg_type: MsgType, payload: Vec<u8>) -> Self {
        Self { msg_type, payload }
    }

    /// Write self to `w`, including the 12-byte header.  Returns
    /// total bytes written (12 + payload.len()) on success.
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

    /// Read one frame from `r`, blocking until a full frame arrives
    /// or EOF.  Returns `Ok(None)` on a clean EOF at a frame
    /// boundary (peer disconnected without an in-flight message).
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

/// Like `read_exact`, but treats EOF at offset 0 as graceful — the
/// peer closed between messages, not mid-message.
fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<ReadEnd> {
    let mut off = 0;
    while off < buf.len() {
        match r.read(&mut buf[off..]) {
            Ok(0) => {
                if off == 0 {
                    return Ok(ReadEnd::Eof);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "EOF mid-frame",
                ));
            }
            Ok(n) => off += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(ReadEnd::Full)
}

// ---------------------------------------------------------------------------
// Typed payload helpers — each MsgType gets a tiny encode/decode pair.
// Plain functions (not a trait) keep the surface area minimal.
// ---------------------------------------------------------------------------

/// HELLO payload: protocol version the sender speaks.
pub fn encode_hello(version: u32) -> Vec<u8> {
    version.to_le_bytes().to_vec()
}

pub fn decode_hello(payload: &[u8]) -> io::Result<u32> {
    if payload.len() != 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("HELLO payload expected 4 bytes, got {}", payload.len()),
        ));
    }
    Ok(u32::from_le_bytes(payload.try_into().unwrap()))
}

/// HELLO_ACK payload: server's protocol version (so a client running
/// a future codebase can detect a downgrade), followed by a fixed
/// 8-byte git sha for diagnostics (zeros when unknown).
pub fn encode_hello_ack(version: u32, git_sha: [u8; 8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(12);
    v.extend_from_slice(&version.to_le_bytes());
    v.extend_from_slice(&git_sha);
    v
}

pub fn decode_hello_ack(payload: &[u8]) -> io::Result<(u32, [u8; 8])> {
    if payload.len() != 12 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("HELLO_ACK payload expected 12 bytes, got {}", payload.len()),
        ));
    }
    let version = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    let mut git_sha = [0u8; 8];
    git_sha.copy_from_slice(&payload[4..12]);
    Ok((version, git_sha))
}

/// One row in a LIST_SESSIONS_REPLY.  Empty title encodes as
/// `title_len = 0`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub session_id: u64,
    pub child_pid: i32,
    pub alive: bool,
    pub title: String,
}

/// LIST_SESSIONS payload: empty.  Function exists for API symmetry.
pub fn encode_list_sessions() -> Vec<u8> {
    Vec::new()
}

/// LIST_SESSIONS_REPLY payload:
/// `[count u32 LE]  [SessionInfo × count]`
/// Each SessionInfo:
/// `[session_id u64 LE] [child_pid i32 LE] [alive u8] [title_len u16 LE] [title bytes]`
pub fn encode_list_sessions_reply(sessions: &[SessionInfo]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + sessions.len() * 24);
    v.extend_from_slice(&(sessions.len() as u32).to_le_bytes());
    for s in sessions {
        v.extend_from_slice(&s.session_id.to_le_bytes());
        v.extend_from_slice(&s.child_pid.to_le_bytes());
        v.push(if s.alive { 1 } else { 0 });
        let title_bytes = s.title.as_bytes();
        if title_bytes.len() > u16::MAX as usize {
            // Silently truncate at 64 KiB; titles are display-only
            // and never realistically that large.
            let truncated = &title_bytes[..u16::MAX as usize];
            v.extend_from_slice(&(truncated.len() as u16).to_le_bytes());
            v.extend_from_slice(truncated);
        } else {
            v.extend_from_slice(&(title_bytes.len() as u16).to_le_bytes());
            v.extend_from_slice(title_bytes);
        }
    }
    v
}

pub fn decode_list_sessions_reply(payload: &[u8]) -> io::Result<Vec<SessionInfo>> {
    if payload.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "LIST_SESSIONS_REPLY missing count",
        ));
    }
    let count = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
    let mut out = Vec::with_capacity(count);
    let mut off = 4;
    for _ in 0..count {
        let need = 8 + 4 + 1 + 2;
        if off + need > payload.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "LIST_SESSIONS_REPLY entry truncated",
            ));
        }
        let session_id = u64::from_le_bytes(payload[off..off + 8].try_into().unwrap());
        off += 8;
        let child_pid = i32::from_le_bytes(payload[off..off + 4].try_into().unwrap());
        off += 4;
        let alive = payload[off] != 0;
        off += 1;
        let title_len = u16::from_le_bytes(payload[off..off + 2].try_into().unwrap()) as usize;
        off += 2;
        if off + title_len > payload.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "LIST_SESSIONS_REPLY title truncated",
            ));
        }
        let title = String::from_utf8_lossy(&payload[off..off + title_len]).into_owned();
        off += title_len;
        out.push(SessionInfo {
            session_id,
            child_pid,
            alive,
            title,
        });
    }
    Ok(out)
}

/// NEW_SESSION payload (c→s):
/// `[cols u16 LE] [rows u16 LE] [cwd_len u16 LE] [cwd bytes UTF-8]`
/// Empty cwd ("" with cwd_len=0) means "use shelld's default" (which
/// today is `$HOME`).  Env / argv overrides intentionally omitted —
/// shelld decides the shell program from `$SHELL` / `/bin/zsh` like
/// `Session::spawn` already does; the client can't override that
/// surface in v1.  Future versions extend with optional trailing
/// fields, gated on the protocol version.
pub fn encode_new_session(cols: u16, rows: u16, cwd: &str) -> Vec<u8> {
    let cwd_bytes = cwd.as_bytes();
    let cwd_len = cwd_bytes.len().min(u16::MAX as usize) as u16;
    let mut v = Vec::with_capacity(6 + cwd_len as usize);
    v.extend_from_slice(&cols.to_le_bytes());
    v.extend_from_slice(&rows.to_le_bytes());
    v.extend_from_slice(&cwd_len.to_le_bytes());
    v.extend_from_slice(&cwd_bytes[..cwd_len as usize]);
    v
}

pub fn decode_new_session(payload: &[u8]) -> io::Result<(u16, u16, String)> {
    if payload.len() < 6 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "NEW_SESSION too short",
        ));
    }
    let cols = u16::from_le_bytes(payload[0..2].try_into().unwrap());
    let rows = u16::from_le_bytes(payload[2..4].try_into().unwrap());
    let cwd_len = u16::from_le_bytes(payload[4..6].try_into().unwrap()) as usize;
    if payload.len() < 6 + cwd_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "NEW_SESSION cwd truncated",
        ));
    }
    let cwd = String::from_utf8_lossy(&payload[6..6 + cwd_len]).into_owned();
    Ok((cols, rows, cwd))
}

/// NEW_SESSION_REPLY payload (s→c):
/// `[session_id u64 LE] [child_pid i32 LE]`
/// Failure paths come back as an ERROR frame instead.
pub fn encode_new_session_reply(session_id: u64, child_pid: i32) -> Vec<u8> {
    let mut v = Vec::with_capacity(12);
    v.extend_from_slice(&session_id.to_le_bytes());
    v.extend_from_slice(&child_pid.to_le_bytes());
    v
}

pub fn decode_new_session_reply(payload: &[u8]) -> io::Result<(u64, i32)> {
    if payload.len() != 12 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "NEW_SESSION_REPLY expected 12 bytes, got {}",
                payload.len()
            ),
        ));
    }
    Ok((
        u64::from_le_bytes(payload[0..8].try_into().unwrap()),
        i32::from_le_bytes(payload[8..12].try_into().unwrap()),
    ))
}

/// ATTACH / DETACH / KILL payload: `[session_id u64 LE]`.  Same
/// three-byte layout, separate function names to make call sites
/// self-documenting.
pub fn encode_session_id(session_id: u64) -> Vec<u8> {
    session_id.to_le_bytes().to_vec()
}

pub fn decode_session_id(payload: &[u8]) -> io::Result<u64> {
    if payload.len() != 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected 8-byte session_id payload",
        ));
    }
    Ok(u64::from_le_bytes(payload.try_into().unwrap()))
}

/// RESIZE payload: `[session_id u64 LE] [cols u16 LE] [rows u16 LE]`
pub fn encode_resize(session_id: u64, cols: u16, rows: u16) -> Vec<u8> {
    let mut v = Vec::with_capacity(12);
    v.extend_from_slice(&session_id.to_le_bytes());
    v.extend_from_slice(&cols.to_le_bytes());
    v.extend_from_slice(&rows.to_le_bytes());
    v
}

pub fn decode_resize(payload: &[u8]) -> io::Result<(u64, u16, u16)> {
    if payload.len() != 12 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("RESIZE expected 12 bytes, got {}", payload.len()),
        ));
    }
    Ok((
        u64::from_le_bytes(payload[0..8].try_into().unwrap()),
        u16::from_le_bytes(payload[8..10].try_into().unwrap()),
        u16::from_le_bytes(payload[10..12].try_into().unwrap()),
    ))
}

/// DATA payload (s→c, PTY bytes):
/// `[session_id u64 LE] [bytes ...]`
/// INPUT payload (c→s, PTY input bytes) uses the same layout.  Both
/// emit one frame per chunk so backpressure works frame-by-frame.
pub fn encode_data(session_id: u64, bytes: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + bytes.len());
    v.extend_from_slice(&session_id.to_le_bytes());
    v.extend_from_slice(bytes);
    v
}

pub fn decode_data(payload: &[u8]) -> io::Result<(u64, &[u8])> {
    if payload.len() < 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "DATA/INPUT missing session_id",
        ));
    }
    let id = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    Ok((id, &payload[8..]))
}

/// ERROR payload:
/// `[code u32 LE]  [message bytes (UTF-8) until end-of-payload]`
pub fn encode_error(code: u32, message: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + message.len());
    v.extend_from_slice(&code.to_le_bytes());
    v.extend_from_slice(message.as_bytes());
    v
}

pub fn decode_error(payload: &[u8]) -> io::Result<(u32, String)> {
    if payload.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ERROR payload missing code",
        ));
    }
    let code = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    let message = String::from_utf8_lossy(&payload[4..]).into_owned();
    Ok((code, message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frame_roundtrip_empty_payload() {
        let f = Frame::new(MsgType::ListSessions, vec![]);
        let mut buf = Vec::new();
        let n = f.write_to(&mut buf).unwrap();
        assert_eq!(n, HEADER_LEN);
        let mut c = Cursor::new(buf);
        let out = Frame::read_from(&mut c).unwrap().unwrap();
        assert_eq!(out.msg_type, MsgType::ListSessions);
        assert!(out.payload.is_empty());
    }

    #[test]
    fn frame_roundtrip_with_payload() {
        let f = Frame::new(MsgType::Hello, encode_hello(PROTO_VERSION));
        let mut buf = Vec::new();
        f.write_to(&mut buf).unwrap();
        let mut c = Cursor::new(buf);
        let out = Frame::read_from(&mut c).unwrap().unwrap();
        assert_eq!(out.msg_type, MsgType::Hello);
        assert_eq!(decode_hello(&out.payload).unwrap(), PROTO_VERSION);
    }

    #[test]
    fn frame_eof_at_boundary_is_clean() {
        let mut c = Cursor::new(Vec::new());
        assert!(Frame::read_from(&mut c).unwrap().is_none());
    }

    #[test]
    fn frame_truncated_header_is_error() {
        let mut c = Cursor::new(vec![1u8, 2, 3]); // 3 bytes, less than HEADER_LEN
        let err = Frame::read_from(&mut c).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn frame_bad_magic_is_error() {
        let bad: Vec<u8> = vec![0; HEADER_LEN];
        let mut c = Cursor::new(bad);
        let err = Frame::read_from(&mut c).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn frame_unknown_msg_type_is_error() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC.to_le_bytes());
        buf.extend_from_slice(&999u32.to_le_bytes()); // bogus type
        buf.extend_from_slice(&0u32.to_le_bytes());
        let mut c = Cursor::new(buf);
        let err = Frame::read_from(&mut c).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn frame_oversized_payload_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC.to_le_bytes());
        buf.extend_from_slice(&(MsgType::Error as u32).to_le_bytes());
        buf.extend_from_slice(&((MAX_PAYLOAD_LEN as u32) + 1).to_le_bytes());
        let mut c = Cursor::new(buf);
        let err = Frame::read_from(&mut c).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn hello_ack_roundtrip() {
        let sha = [0xde, 0xad, 0xbe, 0xef, 0xca, 0xfe, 0xba, 0xbe];
        let p = encode_hello_ack(PROTO_VERSION, sha);
        let (v, s) = decode_hello_ack(&p).unwrap();
        assert_eq!(v, PROTO_VERSION);
        assert_eq!(s, sha);
    }

    #[test]
    fn list_sessions_reply_roundtrip_empty() {
        let p = encode_list_sessions_reply(&[]);
        let out = decode_list_sessions_reply(&p).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn list_sessions_reply_roundtrip_multi() {
        let inp = vec![
            SessionInfo {
                session_id: 1,
                child_pid: 1234,
                alive: true,
                title: "build".into(),
            },
            SessionInfo {
                session_id: 2,
                child_pid: 5678,
                alive: false,
                title: "logs / 中文 / emoji 🎉".into(),
            },
            SessionInfo {
                session_id: 3,
                child_pid: -1,
                alive: false,
                title: String::new(),
            },
        ];
        let p = encode_list_sessions_reply(&inp);
        let out = decode_list_sessions_reply(&p).unwrap();
        assert_eq!(out, inp);
    }

    #[test]
    fn new_session_roundtrip() {
        let p = encode_new_session(80, 24, "/Users/d/foo");
        let (c, r, w) = decode_new_session(&p).unwrap();
        assert_eq!((c, r), (80, 24));
        assert_eq!(w, "/Users/d/foo");
    }

    #[test]
    fn new_session_empty_cwd() {
        let p = encode_new_session(120, 40, "");
        let (c, r, w) = decode_new_session(&p).unwrap();
        assert_eq!((c, r), (120, 40));
        assert_eq!(w, "");
    }

    #[test]
    fn new_session_reply_roundtrip() {
        let p = encode_new_session_reply(0xDEAD_BEEF_CAFE_F00D, 1234);
        let (id, pid) = decode_new_session_reply(&p).unwrap();
        assert_eq!(id, 0xDEAD_BEEF_CAFE_F00D);
        assert_eq!(pid, 1234);
    }

    #[test]
    fn session_id_roundtrip() {
        let p = encode_session_id(42);
        assert_eq!(decode_session_id(&p).unwrap(), 42);
    }

    #[test]
    fn resize_roundtrip() {
        let p = encode_resize(7, 100, 30);
        assert_eq!(decode_resize(&p).unwrap(), (7, 100, 30));
    }

    #[test]
    fn data_roundtrip() {
        let p = encode_data(99, b"hello\x1b[31m world");
        let (id, bytes) = decode_data(&p).unwrap();
        assert_eq!(id, 99);
        assert_eq!(bytes, b"hello\x1b[31m world");
    }

    #[test]
    fn data_zero_length() {
        // Zero-byte chunks are legal — keepalive, attach-with-empty-log.
        let p = encode_data(5, b"");
        let (id, bytes) = decode_data(&p).unwrap();
        assert_eq!(id, 5);
        assert!(bytes.is_empty());
    }

    #[test]
    fn error_roundtrip() {
        let p = encode_error(42, "something exploded");
        let (code, msg) = decode_error(&p).unwrap();
        assert_eq!(code, 42);
        assert_eq!(msg, "something exploded");
    }
}

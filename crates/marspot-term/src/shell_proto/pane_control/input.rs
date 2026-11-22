//! Typing into a pane and watching what it prints.

use std::io;

use super::wire::{Rd, bad, finish, put_text};
use super::{TypedResult, WatchKind, WatchReason};

/// The most input bytes one `PaneType` carries.
pub const MAX_TYPE_BYTES: usize = 32 * 1024;

/// What a watch waits for: the part of `PaneWatch` from `watch_id` on,
/// which `PaneType` also carries when it registers a watch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchBody {
    pub watch_id: u32,
    pub kind: WatchKind,
    pub since_pos: u64,
    pub quiet_ms: u32,
    pub text: String,
}

impl WatchBody {
    /// Cancel with watch id 0: drop every watch of the session.
    pub fn is_clear_all(&self) -> bool {
        self.kind == WatchKind::Cancel && self.watch_id == 0
    }
}

fn put_watch(v: &mut Vec<u8>, w: &WatchBody, what: &str) -> io::Result<()> {
    v.extend_from_slice(&w.watch_id.to_le_bytes());
    v.push(w.kind.to_u8());
    v.extend_from_slice(&w.since_pos.to_le_bytes());
    v.extend_from_slice(&w.quiet_ms.to_le_bytes());
    put_text(v, &w.text, what)
}

fn read_watch(r: &mut Rd) -> io::Result<WatchBody> {
    Ok(WatchBody {
        watch_id: r.u32()?,
        kind: WatchKind::from_u8(r.u8()?),
        since_pos: r.u64()?,
        quiet_ms: r.u32()?,
        text: r.text()?,
    })
}

/// L1 → L2 → L3: register or cancel a watch; answered by `PaneDone`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneWatch {
    pub req_id: u32,
    pub sid: u64,
    pub watch: WatchBody,
}

pub fn encode_pane_watch(m: &PaneWatch) -> io::Result<Vec<u8>> {
    let mut v = Vec::with_capacity(31 + m.watch.text.len());
    v.extend_from_slice(&m.req_id.to_le_bytes());
    v.extend_from_slice(&m.sid.to_le_bytes());
    put_watch(&mut v, &m.watch, "pane_watch")?;
    finish(v, "pane_watch")
}

pub fn decode_pane_watch(p: &[u8]) -> io::Result<PaneWatch> {
    let mut r = Rd::new(p, "pane_watch");
    Ok(PaneWatch {
        req_id: r.u32()?,
        sid: r.u64()?,
        watch: read_watch(&mut r)?,
    })
}

/// L3 → L2 → L1: a watch ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneWatchFired {
    pub sid: u64,
    pub watch_id: u32,
    pub reason: WatchReason,
    pub pos: u64,
    /// The command's exit status; -1 on the wire when there is none.
    pub status: Option<i16>,
    /// Absolute line a text watch matched; all ones on the wire when none.
    pub line: Option<u64>,
}

pub fn encode_pane_watch_fired(m: &PaneWatchFired) -> Vec<u8> {
    // the wire's "none" values cannot also be real values
    debug_assert!(m.status != Some(-1) && m.line != Some(u64::MAX));
    let mut v = Vec::with_capacity(31);
    v.extend_from_slice(&m.sid.to_le_bytes());
    v.extend_from_slice(&m.watch_id.to_le_bytes());
    v.push(m.reason.to_u8());
    v.extend_from_slice(&m.pos.to_le_bytes());
    v.extend_from_slice(&m.status.unwrap_or(-1).to_le_bytes());
    v.extend_from_slice(&m.line.unwrap_or(u64::MAX).to_le_bytes());
    v
}

pub fn decode_pane_watch_fired(p: &[u8]) -> io::Result<PaneWatchFired> {
    let mut r = Rd::new(p, "pane_watch_fired");
    Ok(PaneWatchFired {
        sid: r.u64()?,
        watch_id: r.u32()?,
        reason: WatchReason::from_u8(r.u8()?),
        pos: r.u64()?,
        status: Some(i16::from_le_bytes(r.arr()?)).filter(|&s| s != -1),
        line: Some(r.u64()?).filter(|&l| l != u64::MAX),
    })
}

/// L1 → L2 → L3: write input into the pane's PTY; answered by
/// `PaneTyped`.  A watch here is registered when the input is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneType {
    pub req_id: u32,
    pub sid: u64,
    pub paste: bool,
    pub enter: bool,
    /// Write only while the pane's own shell is in the foreground.
    pub only_at_shell: bool,
    pub bytes: Vec<u8>,
    pub watch: Option<WatchBody>,
}

const TYPE_PASTE: u8 = 1;
const TYPE_ENTER: u8 = 1 << 1;
const TYPE_ONLY_AT_SHELL: u8 = 1 << 2;
const TYPE_WATCH: u8 = 1 << 3;

pub fn encode_pane_type(m: &PaneType) -> io::Result<Vec<u8>> {
    if m.bytes.len() > MAX_TYPE_BYTES {
        return Err(bad("pane_type", "input over 32 KiB"));
    }
    let mut v =
        Vec::with_capacity(17 + m.bytes.len() + m.watch.as_ref().map_or(0, |w| 19 + w.text.len()));
    v.extend_from_slice(&m.req_id.to_le_bytes());
    v.extend_from_slice(&m.sid.to_le_bytes());
    let flags = (m.paste as u8 * TYPE_PASTE)
        | (m.enter as u8 * TYPE_ENTER)
        | (m.only_at_shell as u8 * TYPE_ONLY_AT_SHELL)
        | (m.watch.is_some() as u8 * TYPE_WATCH);
    v.push(flags);
    v.extend_from_slice(&(m.bytes.len() as u32).to_le_bytes());
    v.extend_from_slice(&m.bytes);
    if let Some(w) = &m.watch {
        put_watch(&mut v, w, "pane_type")?;
    }
    finish(v, "pane_type")
}

pub fn decode_pane_type(p: &[u8]) -> io::Result<PaneType> {
    let mut r = Rd::new(p, "pane_type");
    let req_id = r.u32()?;
    let sid = r.u64()?;
    let flags = r.u8()?;
    let len = r.u32()? as usize;
    if len > MAX_TYPE_BYTES {
        return Err(bad("pane_type", "input over 32 KiB"));
    }
    let bytes = r.take(len)?.to_vec();
    let watch = if flags & TYPE_WATCH != 0 {
        Some(read_watch(&mut r)?)
    } else {
        None
    };
    Ok(PaneType {
        req_id,
        sid,
        paste: flags & TYPE_PASTE != 0,
        enter: flags & TYPE_ENTER != 0,
        only_at_shell: flags & TYPE_ONLY_AT_SHELL != 0,
        bytes,
        watch,
    })
}

/// L3 → L2 → L1: what became of a `PaneType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneTyped {
    pub req_id: u32,
    pub sid: u64,
    pub result: TypedResult,
    /// The terminal had echo off when the input was written.
    pub echo_off: bool,
    /// Output position when the last byte reached the PTY.
    pub pos: u64,
    pub written: u32,
}

pub fn encode_pane_typed(m: &PaneTyped) -> Vec<u8> {
    let mut v = Vec::with_capacity(26);
    v.extend_from_slice(&m.req_id.to_le_bytes());
    v.extend_from_slice(&m.sid.to_le_bytes());
    v.push(m.result.to_u8());
    v.push(m.echo_off as u8);
    v.extend_from_slice(&m.pos.to_le_bytes());
    v.extend_from_slice(&m.written.to_le_bytes());
    v
}

pub fn decode_pane_typed(p: &[u8]) -> io::Result<PaneTyped> {
    let mut r = Rd::new(p, "pane_typed");
    Ok(PaneTyped {
        req_id: r.u32()?,
        sid: r.u64()?,
        result: TypedResult::from_u8(r.u8()?),
        echo_off: r.u8()? & 1 != 0,
        pos: r.u64()?,
        written: r.u32()?,
    })
}

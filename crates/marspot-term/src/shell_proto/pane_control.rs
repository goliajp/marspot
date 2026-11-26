//! Payloads of the pane-control messages: the shell asks a core, and
//! through it a session, to open, place, size, type into and watch a
//! pane, and each request is answered.
//!
//! Numbers are little-endian.  A payload shorter than its layout is
//! refused; bytes past it are a later build's additions and are
//! ignored.  An enum byte this build does not know decodes to that
//! enum's `Unknown`, so the receiver can still answer the request it
//! came in.  Text is a u16 byte length and that many bytes of UTF-8;
//! text that is not UTF-8 is refused.  An encoder refuses a payload
//! over `MAX_PAYLOAD_LEN`, which a reader would drop the channel for.

use std::io;

mod input;
mod reset_panel;
mod retire;
mod run_trailer;
mod wire;

pub use input::*;
pub use reset_panel::*;
pub use retire::*;
pub use run_trailer::{RUN_END_MARK, run_trailer};
use wire::{Rd, finish, put_text};

/// `PaneControlCaps` bits: what the core does with these messages.
pub const CAP_TYPE_AND_WATCH: u32 = 1;
pub const CAP_SHADOWS: u32 = 1 << 1;
pub const CAP_PEEK: u32 = 1 << 2;
pub const CAP_HELPER_PANEL: u32 = 1 << 3;
/// Takes `HelperStrip`, and sizes a shadow pane opened 0×0 by its top pane.
pub const CAP_HELPER_STRIP: u32 = 1 << 4;
/// Opens a pane in a new window of its own (`PaneSpawn::new_window`).
pub const CAP_NEW_WINDOW: u32 = 1 << 5;

macro_rules! wire_enum {
    ($(#[$m:meta])* $name:ident { $($var:ident = $v:literal),* $(,)? }) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum $name {
            $($var,)*
            /// A value from a later build, kept so it encodes back unchanged.
            Unknown(u8),
        }
        impl $name {
            pub fn from_u8(b: u8) -> Self {
                match b {
                    $($v => Self::$var,)*
                    other => Self::Unknown(other),
                }
            }
            pub fn to_u8(self) -> u8 {
                match self {
                    $(Self::$var => $v,)*
                    Self::Unknown(b) => b,
                }
            }
        }
    };
}

mod helper_strip;
pub use helper_strip::*;

wire_enum!(WatchKind { Cancel = 0, Text = 1, Quiet = 2, Prompt = 3, Exit = 4, Change = 5, CommandEnd = 6 });
wire_enum!(WatchReason { Met = 1, SessionEnded = 2, Restarting = 3 });
wire_enum!(RetireMode { Delete = 0, Recycle = 1 });
wire_enum!(PlaceWhere { Shadow = 0, Window = 1, Peek = 2, Unpeek = 3 });
wire_enum!(TypedResult { Written = 0, PtyQueueFull = 1, QueueFull = 2, NotReading = 3, NotAtShell = 4 });
wire_enum!(DoneCode { Ok = 0, NotAttached = 1, NoSuchSession = 2, WatchTableFull = 3, OldSession = 4, Other = 5 });
wire_enum!(PanelAct { CloseHelper = 1, CloseAllOfParent = 2, ToggleCwdControl = 3 });
// `Flip`: a build before this one, which did not say
wire_enum!(CwdControl { Flip = 0, Deny = 1, Allow = 2 });

/// L1 → L2: open a pane; answered by `PaneSpawned`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneSpawn {
    pub req_id: u32,
    pub shadow: bool,
    /// Leave the keyboard focus where it is.
    pub no_focus: bool,
    /// A helper's pane: its size, when none is given, is the core's to
    /// choose.
    pub helper: bool,
    /// In a new window of its own, the only pane there; `parent_sid`
    /// and `shadow` are not read.
    pub new_window: bool,
    pub parent_sid: u64,
    pub idle_s: u32,
    pub cols: u16,
    pub rows: u16,
    pub cwd: String,
}

const SPAWN_SHADOW: u8 = 1;
const SPAWN_NO_FOCUS: u8 = 1 << 1;
const SPAWN_HELPER: u8 = 1 << 2;
const SPAWN_NEW_WINDOW: u8 = 1 << 3;

pub fn encode_pane_spawn(m: &PaneSpawn) -> io::Result<Vec<u8>> {
    let mut v = Vec::with_capacity(23 + m.cwd.len());
    v.extend_from_slice(&m.req_id.to_le_bytes());
    v.push((m.shadow as u8 * SPAWN_SHADOW) | (m.no_focus as u8 * SPAWN_NO_FOCUS) | (m.helper as u8 * SPAWN_HELPER)
        | (m.new_window as u8 * SPAWN_NEW_WINDOW));
    v.extend_from_slice(&m.parent_sid.to_le_bytes());
    v.extend_from_slice(&m.idle_s.to_le_bytes());
    v.extend_from_slice(&m.cols.to_le_bytes());
    v.extend_from_slice(&m.rows.to_le_bytes());
    put_text(&mut v, &m.cwd, "pane_spawn")?;
    finish(v, "pane_spawn")
}

pub fn decode_pane_spawn(p: &[u8]) -> io::Result<PaneSpawn> {
    let mut r = Rd::new(p, "pane_spawn");
    let req_id = r.u32()?;
    let flags = r.u8()?;
    Ok(PaneSpawn {
        req_id,
        shadow: flags & SPAWN_SHADOW != 0,
        no_focus: flags & SPAWN_NO_FOCUS != 0,
        helper: flags & SPAWN_HELPER != 0,
        new_window: flags & SPAWN_NEW_WINDOW != 0,
        parent_sid: r.u64()?,
        idle_s: r.u32()?,
        cols: r.u16()?,
        rows: r.u16()?,
        cwd: r.text()?,
    })
}

/// L2 → L1: the pane `PaneSpawn` asked for, or why there is none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneSpawned {
    pub req_id: u32,
    pub ok: bool,
    pub sid: u64,
    pub err: String,
    /// The window the pane opened in; 0 for a shadow pane, or from a
    /// build before this one.
    pub window_id: u32,
}

pub fn encode_pane_spawned(m: &PaneSpawned) -> io::Result<Vec<u8>> {
    let mut v = Vec::with_capacity(19 + m.err.len());
    v.extend_from_slice(&m.req_id.to_le_bytes());
    v.push(m.ok as u8);
    v.extend_from_slice(&m.sid.to_le_bytes());
    put_text(&mut v, &m.err, "pane_spawned")?;
    v.extend_from_slice(&m.window_id.to_le_bytes());
    finish(v, "pane_spawned")
}

pub fn decode_pane_spawned(p: &[u8]) -> io::Result<PaneSpawned> {
    let mut r = Rd::new(p, "pane_spawned");
    let (req_id, ok, sid, err) = (r.u32()?, r.u8()? != 0, r.u64()?, r.text()?);
    // a build before this one ends at the text
    let window_id = r.u32().unwrap_or(0);
    Ok(PaneSpawned { req_id, ok, sid, err, window_id })
}

/// L1 → L2: move a pane between a window and the shadows, or show it
/// inside `host_sid`'s pane; answered by `PaneDone`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanePlace {
    pub req_id: u32,
    pub sid: u64,
    pub to: PlaceWhere,
    pub host_sid: u64,
    /// Show it even if the user types while it waits to be shown.
    pub force: bool,
}

/// The text of a `PaneDone` for a view given up because the user typed.
pub const USER_TYPING_TEXT: &str = "user_typing";

pub fn encode_pane_place(m: &PanePlace) -> Vec<u8> {
    let mut v = Vec::with_capacity(22);
    v.extend_from_slice(&m.req_id.to_le_bytes());
    v.extend_from_slice(&m.sid.to_le_bytes());
    v.push(m.to.to_u8());
    v.extend_from_slice(&m.host_sid.to_le_bytes());
    v.push(m.force as u8);
    v
}

pub fn decode_pane_place(p: &[u8]) -> io::Result<PanePlace> {
    let mut r = Rd::new(p, "pane_place");
    Ok(PanePlace {
        req_id: r.u32()?,
        sid: r.u64()?,
        to: PlaceWhere::from_u8(r.u8()?),
        host_sid: r.u64()?,
        // a build before this one, which did not say
        force: p.len() > 21 && r.u8()? != 0,
    })
}

/// L2 → L3: the session's shadow parent (0: no longer a shadow);
/// answered by `PaneDone`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionShadow {
    pub req_id: u32,
    pub parent_sid: u64,
    pub idle_s: u32,
}

pub fn encode_session_shadow(m: &SessionShadow) -> Vec<u8> {
    let mut v = Vec::with_capacity(16);
    v.extend_from_slice(&m.req_id.to_le_bytes());
    v.extend_from_slice(&m.parent_sid.to_le_bytes());
    v.extend_from_slice(&m.idle_s.to_le_bytes());
    v
}

pub fn decode_session_shadow(p: &[u8]) -> io::Result<SessionShadow> {
    let mut r = Rd::new(p, "session_shadow");
    Ok(SessionShadow {
        req_id: r.u32()?,
        parent_sid: r.u64()?,
        idle_s: r.u32()?,
    })
}

/// L1 → L2: resize a shadow pane; answered by `PaneDone`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneSize {
    pub req_id: u32,
    pub sid: u64,
    pub cols: u16,
    pub rows: u16,
}

pub fn encode_pane_size(m: &PaneSize) -> Vec<u8> {
    let mut v = Vec::with_capacity(16);
    v.extend_from_slice(&m.req_id.to_le_bytes());
    v.extend_from_slice(&m.sid.to_le_bytes());
    v.extend_from_slice(&m.cols.to_le_bytes());
    v.extend_from_slice(&m.rows.to_le_bytes());
    v
}

pub fn decode_pane_size(p: &[u8]) -> io::Result<PaneSize> {
    let mut r = Rd::new(p, "pane_size");
    Ok(PaneSize {
        req_id: r.u32()?,
        sid: r.u64()?,
        cols: r.u16()?,
        rows: r.u16()?,
    })
}

/// The answer to a request that has no answer of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneDone {
    pub req_id: u32,
    pub code: DoneCode,
    pub text: String,
}

pub fn encode_pane_done(m: &PaneDone) -> io::Result<Vec<u8>> {
    let mut v = Vec::with_capacity(7 + m.text.len());
    v.extend_from_slice(&m.req_id.to_le_bytes());
    v.push(m.code.to_u8());
    put_text(&mut v, &m.text, "pane_done")?;
    finish(v, "pane_done")
}

pub fn decode_pane_done(p: &[u8]) -> io::Result<PaneDone> {
    let mut r = Rd::new(p, "pane_done");
    Ok(PaneDone {
        req_id: r.u32()?,
        code: DoneCode::from_u8(r.u8()?),
        text: r.text()?,
    })
}

/// L2 → L1, once after `HelloAck`: the `CAP_*` bits the core does.
pub fn encode_pane_control_caps(caps: u32) -> Vec<u8> {
    caps.to_le_bytes().to_vec()
}

pub fn decode_pane_control_caps(p: &[u8]) -> io::Result<u32> {
    Rd::new(p, "pane_control_caps").u32()
}

/// L2 → L1: a button on the helper panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelperPanelAct {
    pub sid: u64,
    pub act: PanelAct,
    /// What `ToggleCwdControl` is to leave the directory as.
    pub want: CwdControl,
}

pub fn encode_helper_panel_act(m: &HelperPanelAct) -> Vec<u8> {
    let mut v = Vec::with_capacity(10);
    v.extend_from_slice(&m.sid.to_le_bytes());
    v.push(m.act.to_u8());
    v.push(m.want.to_u8());
    v
}

pub fn decode_helper_panel_act(p: &[u8]) -> io::Result<HelperPanelAct> {
    let mut r = Rd::new(p, "helper_panel_act");
    Ok(HelperPanelAct {
        sid: r.u64()?,
        act: PanelAct::from_u8(r.u8()?),
        want: CwdControl::from_u8(if p.len() > 9 { r.u8()? } else { 0 }),
    })
}

#[cfg(test)]
mod tests;

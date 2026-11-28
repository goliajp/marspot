//! `SessionShadow`: a core tells a session whose child it is and how
//! long it may sit idle.

use std::io;

use super::wire::Rd;

/// A child pane's idle timeout that is none: one the user made a child.
/// An L3 from before it reads it as seconds, about 136 years.
pub const SHADOW_IDLE_NEVER: u32 = u32::MAX;
/// The idle timeout of a shadow pane an agent opened or moved there,
/// and what an `idle_s` of 0 means on every path, as it always did.
pub const SHADOW_IDLE_DEFAULT_S: u32 = 6 * 3600;

/// The text of an L3's `PaneDone` Ok to a `SessionShadow`: the timeout
/// it had before, so a move undone can put it back.  An L3 from before
/// it answers "", read as unknown.
pub fn shadow_was_text(idle_s: u32) -> String {
    format!("was_idle_s={idle_s}")
}

pub fn parse_shadow_was(text: &str) -> Option<u32> {
    text.strip_prefix("was_idle_s=")?.parse().ok()
}

/// L2 → L3: the session's shadow parent (0: no longer a shadow) and how
/// long it may sit idle (0: the default, `SHADOW_IDLE_NEVER`: no limit);
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

//! `PaneRetire`: the shell asks a core to end a pane.

use std::io;

use super::RetireMode;
use super::wire::Rd;

/// L1 → L2: end a pane; answered by `PaneDone`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneRetire {
    pub req_id: u32,
    pub sid: u64,
    pub mode: RetireMode,
    /// End it only while it is a shadow pane: one in a window by now is
    /// refused with [`IN_A_WINDOW_TEXT`].
    pub shadow_only: bool,
}

const RETIRE_SHADOW_ONLY: u8 = 1;

/// The text of a `PaneDone` refusing a `shadow_only` retire.
pub const IN_A_WINDOW_TEXT: &str = "in_a_window";

pub fn encode_pane_retire(m: &PaneRetire) -> Vec<u8> {
    let mut v = Vec::with_capacity(14);
    v.extend_from_slice(&m.req_id.to_le_bytes());
    v.extend_from_slice(&m.sid.to_le_bytes());
    v.push(m.mode.to_u8());
    v.push(if m.shadow_only { RETIRE_SHADOW_ONLY } else { 0 });
    v
}

pub fn decode_pane_retire(p: &[u8]) -> io::Result<PaneRetire> {
    let mut r = Rd::new(p, "pane_retire");
    Ok(PaneRetire {
        req_id: r.u32()?,
        sid: r.u64()?,
        mode: RetireMode::from_u8(r.u8()?),
        // a build before this one, which did not say; other bits are a later one's
        shadow_only: p.len() > 13 && r.u8()? & RETIRE_SHADOW_ONLY != 0,
    })
}

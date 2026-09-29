//! Small session-related value types that live here (instead of in
//! `shelld_client`) now that L4 shelld is being retired in RFC-003.
//!
//! Both types existed inside `shelld_client` originally because that
//! module owned the wire that produced them.  RFC-003 moves the
//! producer into per-session `marspot-session` processes, but
//! consumers (renderer, pane mirror, GUI status) still need the same
//! shapes — so this module just re-homes them.

use std::time::Duration;

/// How recently we have to have seen PTY output to count as "active".
/// Mirrors the original `shelld_client::ACTIVE_WINDOW`.
pub const ACTIVE_WINDOW: Duration = Duration::from_secs(2);

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SessionState {
    Active,
    Idle,
    Exited,
}

/// RFC-002 §8 reply slot: cells from a single historic scrollback
/// page.  `line_start..line_start+line_count` is the row range.
/// `line_count == 0` = "no more history past this point".
#[derive(Debug, Clone)]
pub struct PendingPage {
    pub line_start: u32,
    pub line_count: u32,
    pub body: Vec<u8>,
}

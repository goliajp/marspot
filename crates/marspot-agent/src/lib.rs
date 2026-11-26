//! marspot-agent: what every agent CLI shares, as pure code.
//!
//! `quota` decides which account each pane should be on and carries the
//! simulator that checks those decisions.  Nothing here reads a file,
//! starts a thread or knows which agent it serves.

pub mod quota;
pub mod sched;

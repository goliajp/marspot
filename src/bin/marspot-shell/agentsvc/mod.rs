//! Services every agent shares, on the shell side: each one reaches an
//! agent only through what the caller hands it, never by name.

pub(crate) mod install;
pub(crate) mod reset;
pub(crate) mod sched;

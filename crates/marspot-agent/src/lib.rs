//! marspot-agent: account choice for the agent CLIs, as pure code.
//!
//! `quota` decides which account each pane should be on and carries the
//! simulator that checks those decisions; `sched` is the selector they
//! are weighed by.  Nothing here reads a file or starts a thread.  Parts
//! of `quota` still describe one agent's accounts (Claude's usage feed,
//! its Fable window, its refusals); those move out to the per-agent
//! adapters, leaving only what every agent shares.

pub mod quota;
pub mod sched;

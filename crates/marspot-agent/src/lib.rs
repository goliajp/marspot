//! marspot-agent: account choice for the agent CLIs, as pure code.
//!
//! `quota` decides which account each pane should be on and carries the
//! simulator that checks those decisions; `sched` is the selector they
//! are weighed by; `types` are what the agent adapters and the agent
//! services share, among them the state a session record is folded
//! into.  Nothing here reads a file or starts a thread.  Parts
//! of `quota` still describe one agent's accounts (Claude's usage feed,
//! its Fable window, its refusals); those move out to the per-agent
//! adapters, leaving only what every agent shares.

pub mod detmath;
pub mod quota;
pub mod sched;
pub mod types;

//! marspot-adapters: what is particular to each agent CLI, as pure code.
//!
//! `claude` folds Claude Code's session records (one JSON object per
//! line) into the shared `RecordFold`.

pub mod claude;

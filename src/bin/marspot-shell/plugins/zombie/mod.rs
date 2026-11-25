//! The zombie watch: finds processes an agent left behind and subagents
//! stuck with nothing to wake them, that almost certainly nobody uses
//! any more, and records what it would tell the pane they belong to.
//! It never ends them itself.
//!
//! The rules and the record reading are `marspot::zombie`; here are the
//! plugin that runs them on the supervisor's clock (`plugin`) on a
//! thread of their own (`checker`), the scene a check builds from the
//! observation (`scene`), which pane a process belongs to (`attrib`),
//! the dry run's record (`dryrun`), the pacing kept on disk (`store`)
//! and ending a process a session asks to end (`stop`).  This build
//! only records: `zombie.notify` is `dryrun` (the default) or `off`.

pub(crate) mod attrib;
mod checker;
mod dryrun;
mod plugin;
mod scene;
pub(crate) mod stop;
mod store;

pub use plugin::ZombiePlugin;

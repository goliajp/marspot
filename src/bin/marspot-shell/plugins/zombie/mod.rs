//! The zombie watch: finds processes an agent left behind and subagents
//! stuck with nothing to wake them, that almost certainly nobody uses
//! any more, and tells the session of the pane they belong to, or in
//! the dry run records what it would tell.  It never ends them itself.
//!
//! The rules and the record reading are `marspot::zombie`; here are the
//! plugin that runs them on the supervisor's clock (`plugin`) on a
//! thread of their own (`checker`), the scene a check builds from the
//! observation (`scene`), which pane a process belongs to (`attrib`),
//! when and what a pane is told (`teller`, sent by `deliver`), the dry
//! run's record (`dryrun`), the pacing kept on disk (`store`), the
//! record of what closed panes left running (`orphans`) and ending a
//! process a session asks to end (`stop`).  `zombie.notify`
//! is `dryrun` (the default), `live` or `off`.

pub(crate) mod attrib;
mod checker;
mod deliver;
mod dryrun;
mod orphans;
mod plugin;
mod scene;
pub(crate) mod stop;
mod store;
mod teller;

pub use plugin::ZombiePlugin;

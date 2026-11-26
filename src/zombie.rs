//! The zombie watch's rules and the records they read, apart from the
//! shell that runs them on a timer.
//!
//! The shell builds a [`check::Scene`] from its process observation and
//! the claude panes it knows, keeps each pane's records current with
//! [`files::PaneFiles`], and asks a [`check::Watch`] for suspects.  The
//! rules ([`rules`]) are pure functions; the readings they need come
//! through a [`probe::Probe`], so a saved scene can be replayed.

pub mod check;
pub mod exclude;
pub mod files;
pub mod limits;
pub mod orphans;
mod outside;
mod pane;
pub mod probe;
pub mod records;
pub mod rules;
pub mod tail;
pub mod tell;
pub mod transcript;

pub use pane::task_of;

#[cfg(test)]
mod tests;

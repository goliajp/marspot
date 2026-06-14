//! Supervisor event log — thin shim over `marspot::logx::event` so
//! existing `sup_log::log(tag, detail)` call sites keep their two-arg
//! shape while routing into the structured `marspot.log` stream.
//!
//! Until this turn, this module dual-wrote a legacy
//! `supervisor.log` TSV alongside the logx event so the twenty
//! soak / integration scripts grepping that file kept working through
//! the migration. With every script now anchored on `\t<TAG>\t` in
//! `marspot.log`, the legacy file is no longer produced — the GC
//! sweep in `marspot::logx::gc::sweep_legacy_supervisor_log` retires
//! any leftover one from older installs.
//!
//! Future call sites should use `lx_event!` directly to attach
//! structured fields; this shim exists only for the dozens of
//! pre-existing `sup_log::log(tag, detail)` callers in marspot-shell.

/// Append one supervisor event into the structured `marspot.log`
/// stream. Cheap; never panics; never blocks meaningfully.
pub fn log(tag: &str, detail: &str) {
    marspot::logx::event(marspot::logx::Level::Info, tag, detail, &[]);
}

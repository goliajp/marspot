//! Supervisor event log — thin shim over `marspot::logx`.
//!
//! Pre-`logx`, this module owned its own append-only TSV file at
//! `paths::supervisor_log()` with a 2 MiB in-place head-trim. The 33
//! call sites across `marspot-shell/main.rs` learned to call
//! `sup_log::log("TAG", "detail")` and we want to keep the readability
//! of that pattern, but the actual writing now goes through `logx`
//! so the events land in the same TSV stream as shelld / core / session
//! and inherit rotation, compression, and GC for free.
//!
//! `marspot::logx::event` at `Info` level forces UPPER_SNAKE tags
//! through the same line format every other component uses; the
//! `EXECV_INVOKE` line shape in the daemon's log is byte-for-byte the
//! shape lines from this shim will take.
//!
//! Old `supervisor.log` files left over from previous installations
//! are not removed by this code — the logx GC sweep ages them out
//! after `MARSPOT_LOG_GC_AGE_D` (default 7d).

/// Append one supervisor event. `tag` is short UPPER_SNAKE; `detail`
/// is whatever context the call site has.
pub fn log(tag: &str, detail: &str) {
    marspot::logx::event(marspot::logx::Level::Info, tag, detail, &[]);
}

#[cfg(test)]
mod tests {
    /// `sup_log::log` must be callable from any thread without panic
    /// even when logx hasn't been initialised — the shim path becomes a
    /// silent no-op, which is the right behaviour for tests that don't
    /// care about log output.
    #[test]
    fn log_is_callable_without_init() {
        super::log("SHIM_TEST", "compiled-and-callable");
    }
}

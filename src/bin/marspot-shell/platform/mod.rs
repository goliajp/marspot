//! Services the product core offers whatever runs on top of it; none
//! of them knows who uses it.

pub(crate) mod exec_hooks;
// fact logs and state directories: their first callers are the agent
// services that move onto them; until then only the tests use them
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod fact_log;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod state_store;

/// File work is never done on the main thread, which draws the windows;
/// checked in debug and test builds.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn assert_off_main(what: &str) {
    // SAFETY: pthread_main_np only reads the calling thread's identity
    debug_assert!(unsafe { libc::pthread_main_np() } == 0, "{what} on the main thread");
}

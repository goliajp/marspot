//! Services the product core offers whatever runs on top of it; none
//! of them knows who uses it.

pub(crate) mod exec_hooks;
pub(crate) mod pointer_shape;
// fact logs and state directories: their first callers are the agent
// services that move onto them; until then only the tests use them, so
// no writer of today runs through the off-main check
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod fact_log;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod state_store;

/// File work is never done on the main thread, which draws the windows;
/// checked in debug builds.  Unit tests run each test on a thread of its
/// own, so there only [`check_off_main`] itself is tested.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn assert_off_main(what: &str) {
    // SAFETY: pthread_main_np only reads the calling thread's identity
    check_off_main(unsafe { libc::pthread_main_np() } != 0, what);
}

#[cfg_attr(not(test), allow(dead_code))]
fn check_off_main(on_main: bool, what: &str) {
    debug_assert!(!on_main, "{what} on the main thread");
}

#[cfg(test)]
mod tests {
    #[test]
    #[should_panic(expected = "a fact appended on the main thread")]
    fn file_work_on_the_main_thread_is_refused() {
        super::check_off_main(true, "a fact appended");
    }

    #[test]
    fn file_work_elsewhere_passes() {
        super::check_off_main(false, "a fact appended");
        super::assert_off_main("a test thread");
    }
}

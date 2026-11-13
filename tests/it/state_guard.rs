//! A test binary that is not the library's own cannot write the
//! installed app's state either.
//!
//! The library's unit tests are compiled with the answer; this binary
//! links the library as the app does, so here the guard has to find
//! out at run time.  With nextest's mark taken away what is left is
//! the executable's name and place, which is the part that went blind
//! when cargo 1.100 moved test executables: every test of the two
//! binaries could then have saved over the user's layout.
//!
//! `HOME` is the test's own directory, so a guard that fails to refuse
//! writes there and not to the record it protects.

use marspot::state::{self, SavedState};
use marspot::test_env::{EnvGuard, TempDir};

fn assert_every_write_is_refused(home: &TempDir) {
    let err = state::write(&SavedState::default()).expect_err("the layout must not be written");
    assert!(err.to_string().contains("refusing to write"), "unexpected error: {err}");
    assert!(state::write_windows(&[]).is_err());
    assert!(state::clear().is_err());
    assert!(state::clear_windows().is_err());
    assert!(!home.join("Library").exists(), "a refused write leaves nothing behind");
}

#[test]
fn a_test_binary_run_by_hand_is_refused() {
    let home = TempDir::new("guard-home");
    let mut env = EnvGuard::new();
    env.remove("MARSPOT_STATE_DIR").remove("NEXTEST").set("HOME", home.path());
    assert_every_write_is_refused(&home);
}

#[test]
fn a_test_binary_under_nextest_is_refused() {
    let home = TempDir::new("guard-home");
    let mut env = EnvGuard::new();
    env.remove("MARSPOT_STATE_DIR").set("NEXTEST", "1").set("HOME", home.path());
    assert_every_write_is_refused(&home);
}

/// A sandbox is what lets a test write at all.
#[test]
fn a_sandboxed_test_writes_into_its_sandbox() {
    let sandbox = marspot::test_env::sandbox_state_dir();
    state::write(&SavedState::default()).expect("a sandboxed write");
    assert!(sandbox.path().join("shell-state.bin").is_file());
}

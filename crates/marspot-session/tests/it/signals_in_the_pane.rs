//! A pane's shell starts with the signal dispositions a terminal gives
//! it, not the ones its L3 runs with.
//!
//! An L3 is a Rust program and ignores SIGPIPE; an ignored signal stays
//! ignored across exec.  A shell that inherited it makes `yes | head`
//! end with "yes: stdout: Broken pipe" instead of quietly.

use crate::support;

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

/// What a pane's shell running `body` (bash) wrote into the file it has
/// as `$got`.
fn said(body: &str) -> String {
    let mut sandbox = marspot_term::test_env::sandbox_state_dir();
    let got = sandbox.dir.join("got");
    let script = sandbox.dir.join("said.sh");
    std::fs::write(&script, format!("#!/bin/bash\ngot='{}'\n{body}\nsleep 5\n", got.display())).expect("write script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    sandbox.env.set("SHELL", &script);

    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_marspot-session"));
    let s = support::Session::spawn(bin, support::COLS, support::ROWS);
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        let t = std::fs::read_to_string(&got).unwrap_or_default();
        if t.ends_with('\n') || Instant::now() >= deadline {
            break t;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(s);
    status.trim().to_string()
}

#[test]
fn a_writer_to_a_closed_pipe_is_ended_by_sigpipe() {
    // 141 = 128 + SIGPIPE: `yes` was ended by the signal
    let status = said("yes 2>/dev/null | head -1 >/dev/null\necho \"${PIPESTATUS[0]}\" > \"$got\"");
    assert_eq!(status, "141", "yes's exit status in the pane's shell");
}

/// As under `nohup`, or started by a shell's background job.
#[test]
fn a_shell_can_trap_the_signals_its_session_was_started_ignoring() {
    for sig in [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM] {
        // SAFETY: this test's own process, for the session it starts
        unsafe { libc::signal(sig, libc::SIG_IGN) };
    }
    // bash lists a signal it was started ignoring as `trap -- '' SIGHUP`
    let ignored = said("echo \"ignored: $(trap -p HUP INT QUIT TERM | grep -c \"''\")\" > \"$got\"");
    assert_eq!(ignored, "ignored: 0");
}

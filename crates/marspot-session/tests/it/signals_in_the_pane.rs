//! A pane's shell starts with the signal dispositions a terminal gives
//! it, not the ones its L3 runs with.
//!
//! An L3 is a Rust program and ignores SIGPIPE; an ignored signal stays
//! ignored across exec.  A shell that inherited it makes `yes | head`
//! end with "yes: stdout: Broken pipe" instead of quietly.

use crate::support;

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

#[test]
fn a_writer_to_a_closed_pipe_is_ended_by_sigpipe() {
    let mut sandbox = marspot_term::test_env::sandbox_state_dir();
    let got = sandbox.dir.join("got");
    let script = sandbox.dir.join("pipe.sh");
    // 141 = 128 + SIGPIPE: `yes` was ended by the signal
    std::fs::write(
        &script,
        format!(
            "#!/bin/bash\nyes 2>/dev/null | head -1 >/dev/null\necho \"${{PIPESTATUS[0]}}\" > {got}\nsleep 5\n",
            got = got.display()
        ),
    )
    .expect("write script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    sandbox.env.set("SHELL", &script);

    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_marspot-session"));
    let s = support::Session::spawn(bin, support::COLS, support::ROWS);
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        let t = std::fs::read_to_string(&got).unwrap_or_default();
        if !t.trim().is_empty() || Instant::now() >= deadline {
            break t;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(s);
    assert_eq!(status.trim(), "141", "yes's exit status in the pane's shell");
}

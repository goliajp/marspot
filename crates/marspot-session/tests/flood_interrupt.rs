//! Ctrl-C reaches a program that is flooding its pane.
//!
//! An L3 used to parse until its PTY queue was empty before it went
//! back to its loop, and under `yes` the queue is never empty: the key
//! sat behind the flood for as long as the flood ran, which was
//! forever.

#[path = "../examples/support/mod.rs"]
mod support;

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use marspot_term::shell_proto::{MsgType, encode_inject_input};

/// Several of the reader's chunks: the flood is well under way.
const FLOODED: u64 = 1 << 20;

/// Bytes this pane's L3 has taken off its PTY: it appends each chunk
/// to the bytelog as it hands it to the parser.
fn logged_bytes(state: &std::path::Path) -> u64 {
    let Ok(sessions) = std::fs::read_dir(state.join("sessions")) else {
        return 0;
    };
    sessions
        .flatten()
        .filter_map(|d| std::fs::metadata(d.path().join("bytelog")).ok())
        .map(|m| m.len())
        .sum()
}

#[test]
fn ctrl_c_stops_a_program_that_floods_the_pane() {
    let mut sandbox = marspot_term::test_env::sandbox_state_dir();
    // wide, attributed and CJK, so the parser is the slow end of the pipe
    let script = sandbox.dir.join("flood.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nexec yes \"$(printf 'ab \\346\\274\\242\\345\\255\\227 \\033[1mcd\\033[0m %.0s' 1 2 3 4 5 6 7 8)\"\n",
    )
    .expect("write script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    sandbox.env.set("SHELL", &script);

    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_marspot-session"));
    // a failed assert must not leave an L3 flooding the machine: the
    // session is killed when it is dropped
    let mut s = support::Session::spawn(bin, support::COLS, support::ROWS);
    // The queue is ahead of the parser once the parser has been kept
    // busy for a while: the reader fills its bounded queue far faster
    // than a parser at these attributed CJK lines empties it.  A fixed
    // sleep stood in for that, and could not tell a flood from a pane
    // whose program never started.
    let deadline = Instant::now() + Duration::from_secs(10);
    while logged_bytes(sandbox.path()) < FLOODED {
        assert!(
            Instant::now() < deadline,
            "the program never got {FLOODED} bytes into its pane ({} so far)",
            logged_bytes(sandbox.path())
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(s.child.try_wait().expect("try_wait").is_none(), "the flood ended by itself");

    s.send_frame(MsgType::InjectInput, encode_inject_input(0, b"\x03"));
    let sent = Instant::now();
    while s.child.try_wait().expect("try_wait").is_none() {
        assert!(
            sent.elapsed() < Duration::from_secs(10),
            "the session was still flooding 10 s after Ctrl-C"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

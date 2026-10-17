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

#[test]
fn ctrl_c_stops_a_program_that_floods_the_pane() {
    let dir = std::env::temp_dir().join(format!("marspot-flood-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("state dir");
    // wide, attributed and CJK, so the parser is the slow end of the pipe
    let script = dir.join("flood.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nexec yes \"$(printf 'ab \\346\\274\\242\\345\\255\\227 \\033[1mcd\\033[0m %.0s' 1 2 3 4 5 6 7 8)\"\n",
    )
    .expect("write script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    // SAFETY: set before the only child this test spawns, which is what
    // reads them; nothing else in this process looks at either.
    unsafe {
        std::env::set_var("MARSPOT_STATE_DIR", &dir);
        std::env::set_var("SHELL", &script);
    }

    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_marspot-session"));
    // a failed assert must not leave an L3 flooding the machine
    struct Killed(support::Session);
    impl Drop for Killed {
        fn drop(&mut self) {
            if self.0.child.try_wait().ok().flatten().is_none() {
                let _ = self.0.child.kill();
                let _ = self.0.child.wait();
            }
        }
    }
    let mut s = Killed(support::Session::spawn(bin, support::COLS, support::ROWS));
    let deadline = Instant::now() + Duration::from_secs(10);
    while s.0.reader.seq() == 0 {
        assert!(Instant::now() < deadline, "no frame while the program floods the pane");
        std::thread::sleep(Duration::from_millis(10));
    }
    // long enough that the queue is well ahead of the parser
    std::thread::sleep(Duration::from_millis(500));
    assert!(s.0.child.try_wait().expect("try_wait").is_none(), "the flood ended by itself");

    s.0.send_frame(MsgType::InjectInput, encode_inject_input(0, b"\x03"));
    let sent = Instant::now();
    while s.0.child.try_wait().expect("try_wait").is_none() {
        assert!(
            sent.elapsed() < Duration::from_secs(10),
            "the session was still flooding 10 s after Ctrl-C"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

//! Text pasted into a pane in bracketed paste mode cannot end the paste
//! early: markers inside it are dropped before it reaches the PTY.

use crate::support;

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use marspot_term::shell_proto::{MsgType, encode_paste};

#[test]
fn paste_drops_end_marker_before_the_pty() {
    const WANT: &[u8] = b"\x1b[200~abc\x1b[201~";
    let mut sandbox = marspot_term::test_env::sandbox_state_dir();
    let got = sandbox.dir.join("got");
    let script = sandbox.dir.join("paste.sh");
    // READY follows the mode switch in the same write, and L3 parses a
    // pane's output in order: once READY is on the published screen,
    // the mode is set.  dd asks for more than WANT so that anything the
    // stripping missed lands in the file too
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nstty raw -echo\nprintf '\\033[?2004hREADY'\n\
             dd bs=1 count={n} of={got} 2>/dev/null\nsleep 5\n",
            n = WANT.len() + 8,
            got = got.display()
        ),
    )
    .expect("write script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    sandbox.env.set("SHELL", &script);

    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_marspot-session"));
    let s = support::Session::spawn(bin, support::COLS, support::ROWS);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !s
        .try_screen(support::COLS, support::ROWS)
        .is_some_and(|rows| rows.iter().any(|r| r.contains("READY")))
    {
        assert!(Instant::now() < deadline, "the program never turned bracketed paste on");
        std::thread::sleep(Duration::from_millis(10));
    }

    s.send_frame(MsgType::Paste, encode_paste("a\x1b[201~b\x1b[20\x1b[201~1~c"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::read(&got).unwrap_or_default().len() < WANT.len() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    // the paste goes out in one write; this only lets dd finish copying it
    std::thread::sleep(Duration::from_millis(300));
    let bytes = std::fs::read(&got).unwrap_or_default();
    drop(s);
    assert_eq!(bytes, WANT, "what the program read");
}

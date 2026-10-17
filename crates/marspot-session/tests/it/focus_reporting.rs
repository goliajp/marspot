//! A program that turns on focus reporting hears when its pane gains
//! and loses the keyboard.
//!
//! The mode used to be parsed and kept, and the report never sent:
//! nothing told L3 which pane had the keyboard.

use crate::support;

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use marspot_term::shell_proto::{MsgType, encode_pane_terminal_focus};

#[test]
fn focus_in_and_out_reach_a_program_that_asked() {
    let mut sandbox = marspot_term::test_env::sandbox_state_dir();
    let got = sandbox.dir.join("got");
    let script = sandbox.dir.join("focus.sh");
    // READY follows the mode switch in the same write, and L3 parses a
    // pane's output in order: once READY is on the published screen,
    // the mode is set.
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nstty raw -echo\nprintf '\\033[?1004hREADY'\n\
             dd bs=1 count=6 of={got} 2>/dev/null\nsleep 5\n",
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
        assert!(Instant::now() < deadline, "the program never turned focus reporting on");
        std::thread::sleep(Duration::from_millis(10));
    }

    s.send_frame(MsgType::PaneTerminalFocus, encode_pane_terminal_focus(0, true));
    s.send_frame(MsgType::PaneTerminalFocus, encode_pane_terminal_focus(0, false));
    let deadline = Instant::now() + Duration::from_secs(10);
    let bytes = loop {
        let b = std::fs::read(&got).unwrap_or_default();
        if b.len() >= 6 || Instant::now() >= deadline {
            break b;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(s);
    assert_eq!(bytes, b"\x1b[I\x1b[O", "what the program read");
}

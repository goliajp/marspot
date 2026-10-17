//! A program that turns on focus reporting hears when its pane gains
//! and loses the keyboard.
//!
//! The mode used to be parsed and kept, and the report never sent:
//! nothing told L3 which pane had the keyboard.

#[path = "../examples/support/mod.rs"]
mod support;

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use marspot_term::shell_proto::{MsgType, encode_pane_terminal_focus};

#[test]
fn focus_in_and_out_reach_a_program_that_asked() {
    let dir = std::env::temp_dir().join(format!("marspot-focus-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("state dir");
    let got = dir.join("got");
    let ready = dir.join("ready");
    let script = dir.join("focus.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nstty raw -echo\nprintf '\\033[?1004h'\n: > {ready}\n\
             dd bs=1 count=6 of={got} 2>/dev/null\nsleep 5\n",
            ready = ready.display(),
            got = got.display()
        ),
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
    struct Killed(support::Session);
    impl Drop for Killed {
        fn drop(&mut self) {
            if self.0.child.try_wait().ok().flatten().is_none() {
                let _ = self.0.child.kill();
                let _ = self.0.child.wait();
            }
        }
    }
    let s = Killed(support::Session::spawn(bin, support::COLS, support::ROWS));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "the program never turned focus reporting on");
        std::thread::sleep(Duration::from_millis(10));
    }
    // the mode is set by the bytes printed before `ready`; let L3 parse
    // them.  Not a frame to wait for: the screen does not change.
    std::thread::sleep(Duration::from_millis(300));

    s.0.send_frame(MsgType::PaneTerminalFocus, encode_pane_terminal_focus(0, true));
    s.0.send_frame(MsgType::PaneTerminalFocus, encode_pane_terminal_focus(0, false));
    let deadline = Instant::now() + Duration::from_secs(10);
    let bytes = loop {
        let b = std::fs::read(&got).unwrap_or_default();
        if b.len() >= 6 || Instant::now() >= deadline {
            break b;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(bytes, b"\x1b[I\x1b[O", "what the program read");
}

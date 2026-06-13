//! Headless end-to-end probe for the L3 input → echo → shm pipeline
//! (target #4 step 3c).
//!
//! Stands in for L2 without the GUI: it creates the shm region L2 would,
//! spawns a real `marspot-session` with the region + a control socket
//! inherited, types a character over the control socket, and asserts the
//! character lands in the published grid.  That exercises the full
//! step-3 input path — control-socket decode → `key_event_to_bytes` with
//! L3's own modes → PTY write + local-echo predict → republish → poke —
//! deterministically and without Metal/AppKit.
//!
//! Requires a reachable shelld (the dev sandbox's, via `MARSPOT_STATE_DIR`
//! inherited from the environment).  Run via `bin/soak-l3.sh`, which sets
//! that up; standalone:
//!
//!     MARSPOT_STATE_DIR=/tmp/marspot-dev \
//!       cargo run -p marspot-session --example l3_echo_probe -- \
//!       target/release/marspot-session
//!
//! Exit 0 = PASS, non-zero = FAIL (with a diagnostic on stderr).

use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use marspot_term::grid_shm::{create_region, GridShmReader};
use marspot_term::input_core::{KeyState, LogicalKey, MarspotKeyEvent, Modifiers};
use marspot_term::shell_proto::{encode_key_event, event_to_wire, Frame, MsgType};

const COLS: u16 = 80;
const ROWS: u16 = 24;
const TYPED: char = 'z';

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("FAIL: {msg}");
    std::process::exit(1);
}

/// Clear FD_CLOEXEC so an inherited fd survives the child's exec.  L2
/// proper dup2's onto fixed fds in pre_exec; the probe instead passes the
/// real fd numbers (marspot-session parses arbitrary fd numbers from its
/// env), which only needs CLOEXEC cleared.
fn clear_cloexec(fd: RawFd) {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        die(format!("fcntl CLOEXEC on fd {fd}: {}", std::io::Error::last_os_error()));
    }
}

/// Block until the publish seq moves past `from` (or time out).
fn wait_seq_past(reader: &GridShmReader, from: u64, what: &str) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let s = reader.seq();
        if s != from && s != 0 {
            return s;
        }
        if Instant::now() >= deadline {
            die(format!("timed out waiting for {what} (seq stuck at {from})"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn main() {
    let session_bin = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // Sibling of this example binary: target/<profile>/marspot-session.
            std::env::current_exe()
                .ok()
                .and_then(|e| e.parent()?.parent().map(|d| d.join("marspot-session")))
                .unwrap_or_else(|| die("can't locate marspot-session; pass it as argv[1]"))
        });
    if !session_bin.exists() {
        die(format!("marspot-session not found at {}", session_bin.display()));
    }

    // L2 owns the region; both ends map the same fd.
    let region = create_region(COLS, ROWS).unwrap_or_else(|e| die(format!("create_region: {e}")));
    clear_cloexec(region.as_raw_fd());

    // Bidirectional control socket; the child inherits the far end.
    let (parent, child) = UnixStream::pair().unwrap_or_else(|e| die(format!("socket pair: {e}")));
    clear_cloexec(child.as_raw_fd());

    let mut session = Command::new(&session_bin)
        .env("MARSPOT_SHELL_CONTROL_FD", child.as_raw_fd().to_string())
        .env("MARSPOT_SHM_FD", region.as_raw_fd().to_string())
        .spawn()
        .unwrap_or_else(|e| die(format!("spawn {}: {e}", session_bin.display())));
    eprintln!("[probe] spawned marspot-session pid={}", session.id());
    // The child holds its own copy now; drop ours so only it owns it.
    drop(child);

    let reader = GridShmReader::from_fd(region.as_raw_fd())
        .unwrap_or_else(|e| die(format!("GridShmReader::from_fd: {e}")));

    // Wait for the first publish, then let the shell prompt settle.
    let _ = wait_seq_past(&reader, 0, "first publish");
    std::thread::sleep(Duration::from_millis(400));
    let mut buf = Vec::new();
    let before = reader.read(&mut buf).unwrap_or_else(|| die("no frame after first publish"));
    let (c0, r0) = (before.cursor_col, before.cursor_row);
    let seq_before = reader.seq();
    eprintln!("[probe] prompt settled; cursor=({c0},{r0}) seq={seq_before}");

    // Type one character over the control socket.
    let ev = MarspotKeyEvent {
        state: KeyState::Pressed,
        logical: LogicalKey::Char(TYPED),
        text: Some(TYPED.to_string()),
    };
    let frame = Frame::new(MsgType::KeyEvent, encode_key_event(&event_to_wire(&ev, Modifiers::default())));
    {
        let mut w = &parent;
        frame.write_to(&mut w).unwrap_or_else(|e| die(format!("write key frame: {e}")));
        w.flush().ok();
    }
    eprintln!("[probe] sent KeyEvent('{TYPED}')");

    // The char must land at the pre-key cursor (via local-echo predict,
    // or the shell's own echo) and the cursor must advance.
    wait_seq_past(&reader, seq_before, "echo publish");
    std::thread::sleep(Duration::from_millis(100));
    let after = reader.read(&mut buf).unwrap_or_else(|| die("no frame after key"));
    let idx = r0 as usize * COLS as usize + c0 as usize;
    let landed = buf[idx].ch;
    let cursor_advanced = after.cursor_col == c0 + 1 || (after.cursor_row > r0);

    let _ = session.kill();
    let _ = session.wait();

    if landed != TYPED {
        die(format!(
            "typed '{TYPED}' but cell ({c0},{r0}) shows {landed:?} (cursor now ({},{}))",
            after.cursor_col, after.cursor_row
        ));
    }
    if !cursor_advanced {
        die(format!(
            "cell ok but cursor did not advance: was ({c0},{r0}) now ({},{})",
            after.cursor_col, after.cursor_row
        ));
    }
    println!("PASS: '{TYPED}' echoed at ({c0},{r0}); cursor advanced to ({},{})", after.cursor_col, after.cursor_row);
}

//! Headless probe for the L3 scrollback path (target #4 step 4b).
//!
//! Stands in for L2: spawns a real `marspot-session`, types a command
//! that emits enough lines to build scrollback, then sends `GridScroll`
//! frames and asserts the published window tracks the requested offset —
//! L3 owns the scrollback and publishes the historical window, since L2's
//! mirror holds only the visible rows. Verifies the echo (`view_offset`
//! in the snapshot) AND that the on-screen content actually changed when
//! scrolled (a no-op scroll would echo the offset but show the same rows).
//!
//! Requires a reachable shelld (the dev sandbox's, via `MARSPOT_STATE_DIR`).
//! Run via `bin/soak-l3.sh`; standalone:
//!
//!     MARSPOT_STATE_DIR=/tmp/marspot-dev \
//!       cargo run -p marspot-session --example l3_scroll_probe -- \
//!       target/release/marspot-session
//!
//! Exit 0 = PASS, non-zero = FAIL (with a diagnostic on stderr).

use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use marspot_term::grid::Cell;
use marspot_term::grid_shm::{create_region, GridShmReader, GridSnapshot};
use marspot_term::input_core::{KeyState, LogicalKey, MarspotKeyEvent, Modifiers, NamedKey};
use marspot_term::paths::shelld_socket;
use marspot_term::shell_proto::{
    encode_grid_scroll, encode_key_event, event_to_wire, Frame, MsgType,
};
use marspot_term::shelld_client::ShelldClient;

const COLS: u16 = 80;
const ROWS: u16 = 24;
/// Lines the test command emits — comfortably more than ROWS so the
/// screen scrolls and scrollback fills.
const LINES: u32 = 60;

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("FAIL: {msg}");
    std::process::exit(1);
}

fn clear_cloexec(fd: RawFd) {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        die(format!("fcntl CLOEXEC on fd {fd}: {}", std::io::Error::last_os_error()));
    }
}

fn send(sock: &UnixStream, frame: Frame) {
    let mut w = sock;
    frame.write_to(&mut w).unwrap_or_else(|e| die(format!("write frame: {e}")));
    w.flush().ok();
}

fn type_char(sock: &UnixStream, c: char) {
    let ev = MarspotKeyEvent {
        state: KeyState::Pressed,
        logical: LogicalKey::Char(c),
        text: Some(c.to_string()),
    };
    send(sock, Frame::new(MsgType::KeyEvent, encode_key_event(&event_to_wire(&ev, Modifiers::default()))));
}

fn press_enter(sock: &UnixStream) {
    let ev = MarspotKeyEvent {
        state: KeyState::Pressed,
        logical: LogicalKey::Named(NamedKey::Enter),
        text: None,
    };
    send(sock, Frame::new(MsgType::KeyEvent, encode_key_event(&event_to_wire(&ev, Modifiers::default()))));
}

/// Block until a published frame reports `view_offset == off`, returning
/// its snapshot + cells. Times out (a scroll that never republished would
/// hang).
fn wait_offset(reader: &GridShmReader, off: u16, buf: &mut Vec<Cell>, what: &str) -> GridSnapshot {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(snap) = reader.read(buf) {
            if snap.view_offset == off {
                return snap;
            }
        }
        if Instant::now() >= deadline {
            let last = reader.read(buf).map(|s| s.view_offset);
            die(format!("timed out waiting for {what} (offset {off}); last view_offset {last:?}"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn top_row(buf: &[Cell]) -> String {
    buf[0..COLS as usize].iter().map(|c| c.ch).collect()
}

fn main() {
    let session_bin = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|e| e.parent()?.parent().map(|d| d.join("marspot-session")))
                .unwrap_or_else(|| die("can't locate marspot-session; pass it as argv[1]"))
        });
    if !session_bin.exists() {
        die(format!("marspot-session not found at {}", session_bin.display()));
    }

    // Allocate a FRESH session (the L2-allocates path) rather than the
    // standalone attach-first-live: leftover sessions from sibling probes
    // can carry a dirty command line that would corrupt `seq 1 N`.
    let sock = shelld_socket();
    let client = ShelldClient::connect(&sock, || {})
        .unwrap_or_else(|e| die(format!("shelld connect at {}: {e}", sock.display())));
    let id = client
        .create_session(COLS, ROWS, "")
        .unwrap_or_else(|e| die(format!("create_session: {e}")));

    let region = create_region(COLS, ROWS).unwrap_or_else(|e| die(format!("create_region: {e}")));
    clear_cloexec(region.as_raw_fd());
    let (parent, child) = UnixStream::pair().unwrap_or_else(|e| die(format!("socket pair: {e}")));
    clear_cloexec(child.as_raw_fd());

    let mut session = Command::new(&session_bin)
        .env("MARSPOT_SHELL_CONTROL_FD", child.as_raw_fd().to_string())
        .env("MARSPOT_SHM_FD", region.as_raw_fd().to_string())
        .env("MARSPOT_SESSION_ID", id.to_string())
        .spawn()
        .unwrap_or_else(|e| die(format!("spawn {}: {e}", session_bin.display())));
    eprintln!("[scroll] spawned marspot-session pid={}", session.id());
    drop(child);

    let reader = GridShmReader::from_fd(region.as_raw_fd())
        .unwrap_or_else(|e| die(format!("GridShmReader::from_fd: {e}")));
    let mut buf = Vec::new();

    // Let the prompt settle, then run `seq 1 N` to fill the scrollback.
    let deadline = Instant::now() + Duration::from_secs(10);
    while reader.read(&mut buf).is_none() {
        if Instant::now() >= deadline {
            die("no first publish");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(400));
    for c in format!("seq 1 {LINES}").chars() {
        type_char(&parent, c);
    }
    press_enter(&parent);

    // Wait for the scrollback to build (live view, offset 0).
    let deadline = Instant::now() + Duration::from_secs(10);
    let sb_len: u32 = loop {
        if let Some(snap) = reader.read(&mut buf) {
            if snap.view_offset == 0 && snap.scrollback_len >= 20 {
                break snap.scrollback_len;
            }
        }
        if Instant::now() >= deadline {
            let s = reader.read(&mut buf).map(|s| s.scrollback_len);
            die(format!("scrollback never reached 20 rows (last {s:?}) — `seq` did not run / echo path broken"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let live_top = top_row(&buf);
    eprintln!("[scroll] scrollback={sb_len} rows; live top row = {live_top:?}");

    // Scroll back into history; the published window must move there.
    let off = 8u16;
    send(&parent, Frame::new(MsgType::GridScroll, encode_grid_scroll(off)));
    let snap = wait_offset(&reader, off, &mut buf, "scroll-back publish");
    let scrolled_top = top_row(&buf);
    eprintln!("[scroll] at offset {off}: top row = {scrolled_top:?} (sb={})", snap.scrollback_len);
    if scrolled_top == live_top {
        die(format!("scrolled to offset {off} but the top row is unchanged ({live_top:?}) — window did not move"));
    }

    // Back to live.
    send(&parent, Frame::new(MsgType::GridScroll, encode_grid_scroll(0)));
    let _ = wait_offset(&reader, 0, &mut buf, "snap-to-live publish");
    let back_top = top_row(&buf);
    if back_top != live_top {
        die(format!("snap-to-live top row {back_top:?} != original live {live_top:?}"));
    }

    let _ = session.kill();
    let _ = session.wait();
    println!("PASS: L3 scrolled to offset {off} (top row changed) and back to live; scrollback={sb_len}");
}

//! Headless crash-isolation probe for the L3 model (target #4 step 5c).
//!
//! Per-session L3 means a session's parser/terminal lives in its own
//! process, so a panic / crash there ends *one* session and cannot take
//! down L2 or a sibling session. This stands in for L2: it spawns two L3
//! processes (distinct sessions), gets both echoing, then SIGKILLs one
//! (simulating a panic) and asserts:
//!   - the surviving L3 still echoes input (sibling unaffected),
//!   - the dead L3's shm framebuffer is still readable — no garbage, no
//!     panic — so an L2 reading it keeps running (frozen at last frame),
//!   - the killed process is actually reaped (no zombie).
//!
//! Requires a reachable shelld (the dev sandbox's, via `MARSPOT_STATE_DIR`).
//! Run via `bin/soak-l3.sh`; standalone:
//!
//!     MARSPOT_STATE_DIR=/tmp/marspot-dev \
//!       cargo run -p marspot-session --example l3_crash_probe -- \
//!       target/release/marspot-session
//!
//! Exit 0 = PASS, non-zero = FAIL (with a diagnostic on stderr).

use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use marspot_term::grid_shm::{create_region, GridShmReader};
use marspot_term::input_core::{KeyState, LogicalKey, MarspotKeyEvent, Modifiers};
use marspot_term::paths::shelld_socket;
use marspot_term::shell_proto::{encode_key_event, event_to_wire, Frame, MsgType};
use marspot_term::shelld_client::ShelldClient;

const COLS: u16 = 80;
const ROWS: u16 = 24;

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

struct L3 {
    child: Child,
    parent: UnixStream,
    reader: GridShmReader,
    _region: OwnedFd,
}

fn spawn(session_bin: &PathBuf, client: &ShelldClient) -> L3 {
    let id = client
        .create_session(COLS, ROWS, "")
        .unwrap_or_else(|e| die(format!("create_session: {e}")));
    let region = create_region(COLS, ROWS).unwrap_or_else(|e| die(format!("create_region: {e}")));
    clear_cloexec(region.as_raw_fd());
    let (parent, child_sock) = UnixStream::pair().unwrap_or_else(|e| die(format!("socket pair: {e}")));
    clear_cloexec(child_sock.as_raw_fd());
    let child = Command::new(session_bin)
        .env("MARSPOT_SHELL_CONTROL_FD", child_sock.as_raw_fd().to_string())
        .env("MARSPOT_SHM_FD", region.as_raw_fd().to_string())
        .env("MARSPOT_SESSION_ID", id.to_string())
        .spawn()
        .unwrap_or_else(|e| die(format!("spawn: {e}")));
    drop(child_sock);
    let reader = GridShmReader::from_fd(region.as_raw_fd())
        .unwrap_or_else(|e| die(format!("reader: {e}")));
    L3 { child, parent, reader, _region: region }
}

fn type_char(sock: &UnixStream, c: char) {
    let ev = MarspotKeyEvent {
        state: KeyState::Pressed,
        logical: LogicalKey::Char(c),
        text: Some(c.to_string()),
    };
    let frame = Frame::new(MsgType::KeyEvent, encode_key_event(&event_to_wire(&ev, Modifiers::default())));
    let mut w = sock;
    frame.write_to(&mut w).unwrap_or_else(|e| die(format!("write key: {e}")));
    w.flush().ok();
}

fn wait_first_publish(l3: &L3) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut buf = Vec::new();
    while l3.reader.read(&mut buf).is_none() {
        if Instant::now() >= deadline {
            die("no first publish");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(300));
}

/// Type `c` and assert it echoes at the current cursor within 3 s.
fn assert_echoes(l3: &L3, c: char, who: &str) {
    let mut buf = Vec::new();
    let before = l3.reader.read(&mut buf).unwrap_or_else(|| die(format!("{who}: no pre-key frame")));
    let (c0, r0) = (before.cursor_col, before.cursor_row);
    type_char(&l3.parent, c);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(_s) = l3.reader.read(&mut buf) {
            let idx = r0 as usize * COLS as usize + c0 as usize;
            if buf.get(idx).map(|x| x.ch) == Some(c) {
                return;
            }
        }
        if Instant::now() >= deadline {
            die(format!("{who}: typed '{c}' but it never echoed at ({c0},{r0})"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
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

    let sock = shelld_socket();
    let client = ShelldClient::connect(&sock, || {})
        .unwrap_or_else(|e| die(format!("shelld connect at {}: {e}", sock.display())));

    let mut survivor = spawn(&session_bin, &client);
    let mut victim = spawn(&session_bin, &client);
    eprintln!("[crash] survivor pid={} victim pid={}", survivor.child.id(), victim.child.id());
    wait_first_publish(&survivor);
    wait_first_publish(&victim);

    // Both work before the crash.
    assert_echoes(&survivor, 'a', "survivor (pre-crash)");
    assert_echoes(&victim, 'b', "victim (pre-crash)");

    // Read the victim's last frame so we can prove it's still readable
    // after the crash (the shm region outlives the writer).
    let mut vbuf = Vec::new();
    let victim_frame_before = victim.reader.read(&mut vbuf).map(|s| (s.cols, s.rows));

    // Simulate a panic: SIGKILL the victim's process.
    let victim_pid = victim.child.id();
    victim.child.kill().unwrap_or_else(|e| die(format!("kill victim: {e}")));
    let status = victim.child.wait().unwrap_or_else(|e| die(format!("reap victim: {e}")));
    eprintln!("[crash] killed victim pid={victim_pid} ({status})");

    // 1) The survivor still echoes — sibling isolation.
    assert_echoes(&survivor, 'c', "survivor (post-crash)");

    // 2) The dead L3's shm is still readable, unchanged — an L2 reading it
    //    keeps running (frozen at last frame), never panics/garbles.
    let victim_frame_after = victim.reader.read(&mut vbuf).map(|s| (s.cols, s.rows));
    if victim_frame_after.is_none() {
        die("victim shm unreadable after crash (would break L2's read loop)");
    }
    if victim_frame_after != victim_frame_before {
        die(format!(
            "victim shm dims changed after crash: {victim_frame_before:?} -> {victim_frame_after:?}"
        ));
    }

    // 3) No zombie: a second waitpid on the reaped pid must fail (ECHILD).
    let r = unsafe { libc::waitpid(victim_pid as i32, std::ptr::null_mut(), libc::WNOHANG) };
    if r > 0 {
        die(format!("victim pid={victim_pid} not reaped (zombie)"));
    }

    let _ = survivor.child.kill();
    let _ = survivor.child.wait();
    println!("PASS: one L3 killed; sibling kept echoing, dead L3's shm stayed readable, victim reaped");
}

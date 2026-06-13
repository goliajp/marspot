//! Headless probe for the per-session silent-swap mechanism (target #4 5a).
//!
//! Idle per-session update works by bringing up a *replacement* L3 on the
//! same shelld session (new binary), letting it replay the bytelog into a
//! second shm region, then atomically pointing L2 at it and killing the
//! old L3 — invisible because the replayed screen matches. This stands in
//! for L2 and proves the load-bearing behavior the swap relies on:
//!
//!   - two L3 can attach the *same* live session concurrently (shelld is a
//!     multi-subscriber broadcaster),
//!   - the replacement replays the bytelog into its own region and
//!     reconstructs the *same* screen (continuity — no visible blip),
//!   - after the old L3 is killed (the "promote") the replacement is fully
//!     live: it still accepts input and echoes on the same session,
//!   - the old L3 is reaped (no orphan / no zombie).
//!
//! Requires a reachable shelld (the dev sandbox's, via `MARSPOT_STATE_DIR`).
//! Run via `bin/soak-l3.sh`; standalone:
//!
//!     MARSPOT_STATE_DIR=/tmp/marspot-dev \
//!       cargo run -p marspot-session --example l3_swap_probe -- \
//!       target/release/marspot-session
//!
//! Exit 0 = PASS, non-zero = FAIL (with a diagnostic on stderr).

use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use marspot_term::grid::Cell;
use marspot_term::grid_shm::{create_region, GridShmReader};
use marspot_term::input_core::{KeyState, LogicalKey, MarspotKeyEvent, Modifiers, NamedKey};
use marspot_term::paths::shelld_socket;
use marspot_term::shell_proto::{encode_key_event, event_to_wire, Frame, MsgType};
use marspot_term::shelld_client::ShelldClient;

const COLS: u16 = 80;
const ROWS: u16 = 24;
const MARK1: &str = "SWAPxBEFORExMARK";
const MARK2: &str = "SWAPxAFTERxMARK";

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

/// Spawn an L3 bound to `session_id` (the L2-allocates path) with its own
/// fresh shm region + control socket.
fn spawn_on(session_bin: &PathBuf, session_id: u64) -> L3 {
    let region = create_region(COLS, ROWS).unwrap_or_else(|e| die(format!("create_region: {e}")));
    clear_cloexec(region.as_raw_fd());
    let (parent, child_sock) = UnixStream::pair().unwrap_or_else(|e| die(format!("socket pair: {e}")));
    clear_cloexec(child_sock.as_raw_fd());
    let child = Command::new(session_bin)
        .env("MARSPOT_SHELL_CONTROL_FD", child_sock.as_raw_fd().to_string())
        .env("MARSPOT_SHM_FD", region.as_raw_fd().to_string())
        .env("MARSPOT_SESSION_ID", session_id.to_string())
        .spawn()
        .unwrap_or_else(|e| die(format!("spawn: {e}")));
    drop(child_sock);
    let reader = GridShmReader::from_fd(region.as_raw_fd())
        .unwrap_or_else(|e| die(format!("reader: {e}")));
    L3 { child, parent, reader, _region: region }
}

fn type_str(sock: &UnixStream, s: &str) {
    for c in s.chars() {
        let ev = MarspotKeyEvent {
            state: KeyState::Pressed,
            logical: LogicalKey::Char(c),
            text: Some(c.to_string()),
        };
        let f = Frame::new(MsgType::KeyEvent, encode_key_event(&event_to_wire(&ev, Modifiers::default())));
        let mut w = sock;
        f.write_to(&mut w).unwrap_or_else(|e| die(format!("write char: {e}")));
        w.flush().ok();
    }
}

fn press_enter(sock: &UnixStream) {
    let ev = MarspotKeyEvent {
        state: KeyState::Pressed,
        logical: LogicalKey::Named(NamedKey::Enter),
        text: None,
    };
    let f = Frame::new(MsgType::KeyEvent, encode_key_event(&event_to_wire(&ev, Modifiers::default())));
    let mut w = sock;
    f.write_to(&mut w).unwrap_or_else(|e| die(format!("write enter: {e}")));
    w.flush().ok();
}

fn grid_text(reader: &GridShmReader, buf: &mut Vec<Cell>) -> String {
    if reader.read(buf).is_none() {
        return String::new();
    }
    buf.iter().map(|c| c.ch).collect()
}

/// Wait until the published grid contains `needle` (or time out).
fn wait_contains(reader: &GridShmReader, needle: &str, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut buf = Vec::new();
    loop {
        if grid_text(reader, &mut buf).contains(needle) {
            return;
        }
        if Instant::now() >= deadline {
            die(format!("timed out waiting for {what:?} to show {needle:?}"));
        }
        std::thread::sleep(Duration::from_millis(20));
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
    let id = client
        .create_session(COLS, ROWS, "")
        .unwrap_or_else(|e| die(format!("create_session: {e}")));

    // Old L3 on the session; put a marker on screen.
    let mut old = spawn_on(&session_bin, id);
    eprintln!("[swap] old L3 pid={} on session {id}", old.child.id());
    wait_contains(&old.reader, "", "old boot"); // wait for first publish
    std::thread::sleep(Duration::from_millis(300));
    type_str(&old.parent, &format!("echo {MARK1}"));
    press_enter(&old.parent);
    wait_contains(&old.reader, MARK1, "old after echo");
    eprintln!("[swap] old shows {MARK1}");

    // Bring up the replacement on the SAME session — it replays the
    // bytelog into its own region and must reconstruct the same screen.
    let new = spawn_on(&session_bin, id);
    eprintln!("[swap] new L3 pid={} on session {id} (replaying)", new.child.id());
    wait_contains(&new.reader, MARK1, "new replay");
    eprintln!("[swap] new replayed {MARK1} — continuity OK");

    // Promote: kill the old L3 (L2 would atomically point at `new` first).
    let old_pid = old.child.id();
    old.child.kill().unwrap_or_else(|e| die(format!("kill old: {e}")));
    let st = old.child.wait().unwrap_or_else(|e| die(format!("reap old: {e}")));
    eprintln!("[swap] promoted: killed old pid={old_pid} ({st})");

    // The replacement must still be live: type into it on the same session.
    type_str(&new.parent, &format!("echo {MARK2}"));
    press_enter(&new.parent);
    wait_contains(&new.reader, MARK2, "new after swap");
    eprintln!("[swap] new accepted input post-swap, shows {MARK2}");

    // No zombie for the old pid.
    let r = unsafe { libc::waitpid(old_pid as i32, std::ptr::null_mut(), libc::WNOHANG) };
    if r > 0 {
        die(format!("old pid={old_pid} not reaped (zombie)"));
    }

    let mut new = new;
    let _ = new.child.kill();
    let _ = new.child.wait();
    println!("PASS: replacement replayed the screen on the same session, took over after the old L3 was killed, no zombie");
}

//! Headless probe for the L3 selection-text path (target #4 step 4c).
//!
//! Stands in for L2's Cmd-C: spawns a real `marspot-session` on a fresh
//! session, types `echo <MARKER>` so the marker lands on screen, then
//! sends a `GetSelectionText` frame over the control socket and reads the
//! `SelectionText` reply — asserting it carries the marker. That proves
//! the round-trip works AND that the text comes from L3's own grid (L2's
//! mirror is window-only and `terminal()` panics on an L3 pane, so the
//! text *must* come over the wire from the session process).
//!
//! Requires a reachable shelld (the dev sandbox's, via `MARSPOT_STATE_DIR`).
//! Run via `bin/soak-l3.sh`; standalone:
//!
//!     MARSPOT_STATE_DIR=/tmp/marspot-dev \
//!       cargo run -p marspot-session --example l3_selection_probe -- \
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
use marspot_term::input_core::{KeyState, LogicalKey, MarspotKeyEvent, Modifiers, NamedKey};
use marspot_term::paths::shelld_socket;
use marspot_term::shell_proto::{
    decode_selection_text, encode_get_selection_text, encode_key_event, event_to_wire, Frame,
    MsgType,
};
use marspot_term::shelld_client::ShelldClient;

const COLS: u16 = 80;
const ROWS: u16 = 24;
const MARKER: &str = "MARSPOTxSELECTxMARKER";

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

    // Fresh session (L2-allocates path) for a clean prompt.
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
    eprintln!("[selection] spawned marspot-session pid={}", session.id());
    drop(child);

    let reader = GridShmReader::from_fd(region.as_raw_fd())
        .unwrap_or_else(|e| die(format!("GridShmReader::from_fd: {e}")));
    let mut buf = Vec::new();

    // Prompt settle, then `echo MARKER`.
    let deadline = Instant::now() + Duration::from_secs(10);
    while reader.read(&mut buf).is_none() {
        if Instant::now() >= deadline {
            die("no first publish");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(300));
    for c in format!("echo {MARKER}").chars() {
        type_char(&parent, c);
    }
    press_enter(&parent);

    // Wait until the marker is on the published grid (echo output landed).
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(snap) = reader.read(&mut buf) {
            let text: String = buf.iter().map(|c| c.ch).collect();
            // The command line also contains "echo MARKER"; wait until the
            // marker appears at least twice (command echo + output) so we
            // know the command actually ran.
            if text.matches(MARKER).count() >= 2 {
                eprintln!("[selection] marker on grid (sb={})", snap.scrollback_len);
                break;
            }
        }
        if Instant::now() >= deadline {
            die("marker never appeared on the grid — echo path broken");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // Cmd-C: select the entire visible grid. anchor = (col, abs) where abs
    // is rows up from the live *grid bottom* (abs 0 = grid row ROWS-1, abs
    // ROWS-1 = grid row 0). Output fills from the top, so selecting abs
    // ROWS-1 down to 0 covers every on-screen row — the marker included.
    send(
        &parent,
        Frame::new(
            MsgType::GetSelectionText,
            encode_get_selection_text((0, (ROWS - 1) as u32), (COLS - 1, 0), false),
        ),
    );

    // Read the SelectionText reply on a thread (the socket also carries
    // GridReady pokes; a bare blocking read would never re-check a
    // deadline). Bound the wait so a lost reply fails fast instead of
    // hanging the soak.
    let reply_sock = parent.try_clone().unwrap_or_else(|e| die(format!("clone sock: {e}")));
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut rdr = reply_sock;
        loop {
            match Frame::read_from(&mut rdr) {
                Ok(Some(f)) if f.msg_type == MsgType::SelectionText => {
                    if let Ok(t) = decode_selection_text(&f.payload) {
                        let _ = tx.send(t);
                    }
                    return;
                }
                Ok(Some(_)) => continue, // GridReady poke etc.
                Ok(None) | Err(_) => return,
            }
        }
    });
    let reply = rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|_| die("no SelectionText reply within 5s"));

    let _ = session.kill();
    let _ = session.wait();

    if !reply.contains(MARKER) {
        die(format!("SelectionText reply did not contain the marker.\n  reply = {reply:?}"));
    }
    println!("PASS: GetSelectionText returned the on-screen marker from L3's grid ({} bytes)", reply.len());
}

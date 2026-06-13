//! Headless probe for the L3 resize path (target #4 step 4b).
//!
//! Stands in for L2: creates the shm region, spawns a real
//! `marspot-session`, then sends `GridResize` frames over the control
//! socket and asserts the published snapshot's dims track each resize —
//! grow then shrink — proving the whole path: frame → `session.resize`
//! (shelld ioctl's the PTY + local reflow) → republish at the new dims →
//! the reader reads the new dims from the header with **no remap** (the
//! region is capacity-mapped once at create).
//!
//! Requires a reachable shelld (the dev sandbox's, via `MARSPOT_STATE_DIR`).
//! Run via `bin/soak-l3.sh`; standalone:
//!
//!     MARSPOT_STATE_DIR=/tmp/marspot-dev \
//!       cargo run -p marspot-session --example l3_resize_probe -- \
//!       target/release/marspot-session
//!
//! Exit 0 = PASS, non-zero = FAIL (with a diagnostic on stderr).

use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use marspot_term::grid_shm::{create_region, GridShmReader};
use marspot_term::shell_proto::{encode_grid_resize, Frame, MsgType};

const BOOT_COLS: u16 = 80;
const BOOT_ROWS: u16 = 24;

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

/// Block until a published frame reports exactly `(cols, rows)`, or time
/// out (a resize that never reflowed would hang here).
fn wait_dims(reader: &GridShmReader, cols: u16, rows: u16, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut buf = Vec::new();
    loop {
        if let Some(snap) = reader.read(&mut buf) {
            if (snap.cols, snap.rows) == (cols, rows) {
                // Sanity: the cell buffer length must match the dims.
                if buf.len() != cols as usize * rows as usize {
                    die(format!(
                        "{what}: dims {cols}x{rows} but {} cells",
                        buf.len()
                    ));
                }
                return;
            }
        }
        if Instant::now() >= deadline {
            let last = reader.read(&mut buf).map(|s| (s.cols, s.rows));
            die(format!("timed out waiting for {what} ({cols}x{rows}); last published {last:?}"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn send_resize(sock: &UnixStream, cols: u16, rows: u16) {
    let frame = Frame::new(MsgType::GridResize, encode_grid_resize(cols, rows));
    let mut w = sock;
    frame.write_to(&mut w).unwrap_or_else(|e| die(format!("write GridResize: {e}")));
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

    let region =
        create_region(BOOT_COLS, BOOT_ROWS).unwrap_or_else(|e| die(format!("create_region: {e}")));
    clear_cloexec(region.as_raw_fd());
    let (parent, child) = UnixStream::pair().unwrap_or_else(|e| die(format!("socket pair: {e}")));
    clear_cloexec(child.as_raw_fd());

    let mut session = Command::new(&session_bin)
        .env("MARSPOT_SHELL_CONTROL_FD", child.as_raw_fd().to_string())
        .env("MARSPOT_SHM_FD", region.as_raw_fd().to_string())
        .spawn()
        .unwrap_or_else(|e| die(format!("spawn {}: {e}", session_bin.display())));
    eprintln!("[resize] spawned marspot-session pid={}", session.id());
    drop(child);

    let reader = GridShmReader::from_fd(region.as_raw_fd())
        .unwrap_or_else(|e| die(format!("GridShmReader::from_fd: {e}")));

    // Boot dims first.
    wait_dims(&reader, BOOT_COLS, BOOT_ROWS, "boot publish");
    eprintln!("[resize] boot at {BOOT_COLS}x{BOOT_ROWS}");

    // Grow, shrink, and back — each must land in the published snapshot
    // with a matching cell count (no remap; the region was capacity-mapped
    // once at create).
    for &(cols, rows) in &[(100u16, 40u16), (40, 12), (80, 24)] {
        send_resize(&parent, cols, rows);
        wait_dims(&reader, cols, rows, "resize publish");
        eprintln!("[resize] now {cols}x{rows}");
    }

    let _ = session.kill();
    let _ = session.wait();
    println!("PASS: L3 resize tracked 80x24 → 100x40 → 40x12 → 80x24 in place (no remap)");
}

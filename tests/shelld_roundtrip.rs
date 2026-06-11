//! Integration test for the shelld client/server round-trip.
//!
//! Spawns marspot-shelld as a subprocess on a per-test socket path,
//! connects via `ShelldClient`, exercises NEW_SESSION + DATA flow,
//! verifies the embedded Terminal sees the shell's prompt bytes.
//!
//! Not part of `cargo nextest --lib`; run with `cargo nextest run`
//! to include it.  Self-cleaning: kills the daemon on Drop and
//! removes the temp socket path.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use marspot::shelld_client::ShelldClient;

/// One-off scaffolding: spawn a fresh shelld on its own socket path.
struct DaemonGuard {
    child: Child,
    socket: PathBuf,
}

impl DaemonGuard {
    fn spawn(tag: &str) -> Self {
        let socket = std::env::temp_dir().join(format!("marspot-shelld-{}-{}.sock", tag, std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let bin = if std::env::var("CARGO_TARGET_DIR").is_ok() {
            std::env::var("CARGO_TARGET_DIR").unwrap() + "/debug/marspot-shelld"
        } else {
            "target/debug/marspot-shelld".into()
        };
        // Override the daemon's socket path via env var.  shelld
        // honours MARSPOT_SHELLD_SOCKET if set; falls back to the
        // default Caches path otherwise (production).
        let child = Command::new(bin)
            .env("MARSPOT_SHELLD_SOCKET", &socket)
            .env("HOME", std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn shelld for test");

        // Wait for the socket to appear (bind happens early in main).
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if socket.exists() {
                return Self { child, socket };
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("shelld socket didn't appear at {}", socket.display());
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

#[test]
fn client_new_session_reads_prompt() {
    let daemon = DaemonGuard::spawn("client-new");
    let woke = Arc::new(AtomicBool::new(false));
    let woke_c = woke.clone();
    let client = ShelldClient::connect(&daemon.socket, move || {
        woke_c.store(true, Ordering::Release);
    })
    .expect("connect");

    let mut session = client.new_session(80, 24, "").expect("new session");

    // Spin until enough bytes arrive that the prompt should have
    // been written, or 3s elapse.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut total = 0;
    while Instant::now() < deadline && total < 80 {
        total += session.pump();
        if total > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // Give the prompt a moment to fully arrive (Last login… line
    // can come in two chunks).
    std::thread::sleep(Duration::from_millis(200));
    total += session.pump();
    assert!(total > 0, "expected prompt bytes, got 0");
    assert!(woke.load(Ordering::Acquire), "wake callback never fired");

    // Render the grid via the public Terminal API: any non-default
    // cells at all means the parser actually consumed the chunk.
    let grid = session.terminal.grid();
    let cell_00 = grid.cell(0, 0);
    let cell_count_nonblank = (0..grid.cols())
        .filter(|c| {
            let cell = grid.cell(*c, 0);
            cell.ch != ' ' && cell.ch != '\0'
        })
        .count();
    assert!(
        cell_count_nonblank > 0,
        "row 0 should have at least one non-blank cell; first cell = {:?}",
        cell_00
    );
}

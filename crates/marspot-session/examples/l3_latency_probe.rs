//! Headless keystroke→echo latency probe for the L3 path (target #4 4d).
//!
//! The per-session L3 model adds one IPC hop to local echo vs the
//! in-process path (L2 forwards the key → L3 predicts + publishes → pokes
//! L2). The design (docs/per-session-l3.md) calls for measuring that tail.
//! This times, per keystroke, `frame written → predicted echo visible in
//! the shm framebuffer` — i.e. the L3 half of keystroke→paint, which is
//! exactly the new cost (L2's Metal paint is unchanged and not headless-
//! measurable). Reports p50/p99/max and fails only on a *gross* regression
//! (a broken event-driven path that polls or stalls), not on jitter.
//!
//! Requires a reachable shelld (the dev sandbox's, via `MARSPOT_STATE_DIR`).
//! Run via `bin/soak-l3.sh`; standalone:
//!
//!     MARSPOT_STATE_DIR=/tmp/marspot-dev \
//!       cargo run -p marspot-session --example l3_latency_probe -- \
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
use marspot_term::paths::shelld_socket;
use marspot_term::shell_proto::{encode_key_event, event_to_wire, Frame, MsgType};
use marspot_term::shelld_client::ShelldClient;

const COLS: u16 = 80;
const ROWS: u16 = 24;
/// Keystrokes timed. Fits on the prompt line (well under COLS).
const SAMPLES: usize = 50;
/// Gross-regression ceiling. A working localhost socketpair round-trip is
/// well under a millisecond; a broken event-driven path (polling / stall)
/// blows past this. Generous so dev-box / CI jitter can't flake it.
const MAX_MS_CEILING: f64 = 50.0;

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

fn type_x(sock: &UnixStream) {
    let ev = MarspotKeyEvent {
        state: KeyState::Pressed,
        logical: LogicalKey::Char('x'),
        text: Some("x".to_string()),
    };
    let frame = Frame::new(MsgType::KeyEvent, encode_key_event(&event_to_wire(&ev, Modifiers::default())));
    let mut w = sock;
    frame.write_to(&mut w).unwrap_or_else(|e| die(format!("write key: {e}")));
    w.flush().ok();
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

    // Fresh session for a clean prompt.
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
    eprintln!("[latency] spawned marspot-session pid={}", session.id());
    drop(child);

    let reader = GridShmReader::from_fd(region.as_raw_fd())
        .unwrap_or_else(|e| die(format!("GridShmReader::from_fd: {e}")));
    let mut buf = Vec::new();

    // Settle the prompt.
    let deadline = Instant::now() + Duration::from_secs(10);
    while reader.read(&mut buf).is_none() {
        if Instant::now() >= deadline {
            die("no first publish");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(300));

    let mut samples_ms: Vec<f64> = Vec::with_capacity(SAMPLES);
    for i in 0..SAMPLES {
        // Pre-key cursor: the echoed 'x' must land at (c0, r0).
        let before = reader.read(&mut buf).unwrap_or_else(|| die("no frame before key"));
        let (c0, r0) = (before.cursor_col, before.cursor_row);

        let t0 = Instant::now();
        type_x(&parent);

        // Spin (no sleep — we're measuring latency) until the echo lands.
        let kdeadline = t0 + Duration::from_secs(2);
        loop {
            if let Some(_s) = reader.read(&mut buf) {
                let idx = r0 as usize * COLS as usize + c0 as usize;
                if buf.get(idx).map(|c| c.ch) == Some('x') {
                    break;
                }
            }
            if Instant::now() >= kdeadline {
                die(format!("keystroke {i}: echo never landed at ({c0},{r0}) within 2s"));
            }
            std::hint::spin_loop();
        }
        samples_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        // Small gap so the next prompt column is stable before we sample.
        std::thread::sleep(Duration::from_millis(5));
    }

    let _ = session.kill();
    let _ = session.wait();

    samples_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| samples_ms[((samples_ms.len() as f64 * p) as usize).min(samples_ms.len() - 1)];
    let (p50, p99, max) = (pct(0.50), pct(0.99), *samples_ms.last().unwrap());
    let mean: f64 = samples_ms.iter().sum::<f64>() / samples_ms.len() as f64;
    eprintln!(
        "[latency] keystroke→echo over {SAMPLES} samples: mean={mean:.3}ms p50={p50:.3}ms p99={p99:.3}ms max={max:.3}ms"
    );

    if max > MAX_MS_CEILING {
        die(format!(
            "max keystroke→echo {max:.3}ms exceeds {MAX_MS_CEILING}ms ceiling — event-driven path may be stalling/polling"
        ));
    }
    println!("PASS: L3 keystroke→echo p50={p50:.3}ms p99={p99:.3}ms max={max:.3}ms (under {MAX_MS_CEILING}ms)");
}

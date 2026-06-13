//! Production-path single-session cat throughput (perf-attack E-bench-infra/L3).
//!
//! The cross-terminal `bin/measure.sh` drives the standalone `mcli` binary —
//! a single in-process session with no IPC/shm hop. But the *product* now
//! ships shell→core→L3 (per-session engine, default since 2026-06-13): every
//! pane's bytes flow shelld → marspot-session (parser → grid → shm publish),
//! ~0.90× the in-process bulk-cat rate (the per-pump shm-window memcpy + the
//! process hop). So the gate's "marspot live" number — measured on mcli or
//! hand-captured via Screen Sharing on the pre-L3 app — overstates what the
//! product actually drains. This probe measures the real L3 data path
//! headlessly so the gate can track it reproducibly.
//!
//! It spawns a real `marspot-session` attached to a fresh shelld session
//! (exactly as `marspot-core::spawn_l3_pane` does: inherited shm region + a
//! control socket), types `cat <scenario> <scenario> …` over the control
//! socket, and times the drain window from the first scroll_push_count advance
//! to its plateau (cat finished → parse caught up → count stops). The caller
//! credits `repeats × file_bytes` against that window for MiB/s.
//!
//!     MARSPOT_STATE_DIR=… l3_throughput <marspot-session> <scenario.bin> <repeats>
//!
//! Prints the drain window in nanoseconds to stdout (diagnostics to stderr).
//! `bin/measure-l3.sh` loops scenarios × trials and aggregates the median.

use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use marspot_term::grid::Cell;
use marspot_term::grid_shm::{create_region, GridShmReader};
use marspot_term::input_core::{KeyState, LogicalKey, MarspotKeyEvent, Modifiers, NamedKey};
use marspot_term::paths::shelld_socket;
use marspot_term::shell_proto::{encode_key_event, event_to_wire, Frame, MsgType};
use marspot_term::shelld_client::ShelldClient;

const COLS: u16 = 200;
const ROWS: u16 = 50;

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

/// Drain duration of a `cat` issued at `t_issue`: from the command being
/// typed to the moment the parse stops advancing (cat done, last pump
/// published). `count` drives any reading itself and returns the current
/// scroll_push_count.
///
/// The window is measured from `t_issue` to the **timestamp of the final
/// count change** — NOT from "first advance". The L3 session reads the
/// shelld firehose and parses in coarse pumps (a cached `cat` can land
/// tens of MiB in a single pump), publishing the shm only after each pump.
/// So progress is visible only at pump boundaries: if we started the clock
/// at "first advance" we could miss the entire parse inside one giant pump
/// and read a near-zero window. Anchoring the start at `t_issue` and using
/// `last_change` (when count last moved = when the final pump finished)
/// captures the true parse time regardless of pump granularity.
///
/// The plateau threshold is generous (1000 ms): it only delays *detection*,
/// it does not enter the recorded value (which is `last_change - t_issue`),
/// so it can safely exceed any inter-pump gap during active draining.
fn drain_window<F: FnMut() -> u64>(t_issue: Instant, mut count: F) -> f64 {
    // Wait for the cat to actually stream (count climbs well past the
    // prompt echo), so the long command line being parsed by the shell
    // doesn't trip the plateau detector before any bytes flow.
    let base = count();
    let start_dl = Instant::now() + Duration::from_secs(30);
    loop {
        if count() > base + 2_000 {
            break;
        }
        if Instant::now() >= start_dl {
            die("cat never started (prompt not ready?)");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut last = count();
    let mut last_change = Instant::now();
    loop {
        let c = count();
        let now = Instant::now();
        if c != last {
            last = c;
            last_change = now;
        } else if now.duration_since(last_change) >= Duration::from_millis(1000) {
            return last_change.duration_since(t_issue).as_secs_f64();
        }
        // Hard ceiling so a stuck session can't hang the gate.
        if now.duration_since(t_issue) >= Duration::from_secs(120) {
            return now.duration_since(t_issue).as_secs_f64();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn type_command(sock: &UnixStream, cmd: &str) {
    for c in cmd.chars().map(Some).chain(std::iter::once(None)) {
        let ev = match c {
            Some(ch) => MarspotKeyEvent {
                state: KeyState::Pressed,
                logical: LogicalKey::Char(ch),
                text: Some(ch.to_string()),
            },
            None => MarspotKeyEvent {
                state: KeyState::Pressed,
                logical: LogicalKey::Named(NamedKey::Enter),
                text: None,
            },
        };
        let mut w = sock;
        Frame::new(MsgType::KeyEvent, encode_key_event(&event_to_wire(&ev, Modifiers::default())))
            .write_to(&mut w)
            .unwrap_or_else(|e| die(format!("type: {e}")));
        w.flush().ok();
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let session_bin = args.next().map(PathBuf::from).unwrap_or_else(|| {
        die("usage: l3_throughput <marspot-session> <scenario.bin> <repeats>")
    });
    let scenario = args
        .next()
        .unwrap_or_else(|| die("usage: l3_throughput <marspot-session> <scenario.bin> <repeats>"));
    let repeats: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1).max(1);
    if !session_bin.exists() {
        die(format!("session bin not found: {}", session_bin.display()));
    }
    let scenario_path = std::fs::canonicalize(&scenario)
        .unwrap_or_else(|e| die(format!("scenario {scenario}: {e}")));

    let sock_path = shelld_socket();
    let client = ShelldClient::connect(&sock_path, || {})
        .unwrap_or_else(|e| die(format!("shelld connect ({}): {e}", sock_path.display())));

    let id = client
        .create_session(COLS, ROWS, "")
        .unwrap_or_else(|e| die(format!("create_session: {e}")));
    let region = create_region(COLS, ROWS).unwrap_or_else(|e| die(format!("create_region: {e}")));
    clear_cloexec(region.as_raw_fd());
    let (parent, child) = UnixStream::pair().unwrap_or_else(|e| die(format!("pair: {e}")));
    clear_cloexec(child.as_raw_fd());
    let mut session = Command::new(&session_bin)
        .env("MARSPOT_SHELL_CONTROL_FD", child.as_raw_fd().to_string())
        .env("MARSPOT_SHM_FD", region.as_raw_fd().to_string())
        .env("MARSPOT_SESSION_ID", id.to_string())
        // Silence the session's per-pump debug log — only the probe's final
        // ns must reach stdout so the wrapper can capture it cleanly.
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| die(format!("spawn session: {e}")));
    drop(child);
    let reader =
        GridShmReader::from_fd(region.as_raw_fd()).unwrap_or_else(|e| die(format!("reader: {e}")));

    // Drain the control socket so the session's GridReady pokes never block.
    let stop = Arc::new(AtomicBool::new(false));
    let stop_r = stop.clone();
    let mut rdr = parent.try_clone().unwrap_or_else(|e| die(format!("clone: {e}")));
    let drain = std::thread::spawn(move || {
        while !stop_r.load(Ordering::Relaxed) {
            if Frame::read_from(&mut rdr).map(|f| f.is_none()).unwrap_or(true) {
                break;
            }
        }
    });

    // Wait for first publish, then let the prompt settle.
    let mut buf: Vec<Cell> = Vec::new();
    let dl = Instant::now() + Duration::from_secs(10);
    while reader.read(&mut buf).is_none() {
        if Instant::now() >= dl {
            die("no first publish");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(1500));

    // `cat path path … path` — one continuous stream, repeated to grow the
    // drain window past the 10 ms poll / 300 ms plateau resolution. Repeating
    // the path keeps the content mix (ascii/cjk/emoji/mixed) identical to the
    // cross-terminal scenario.
    let p = scenario_path.to_string_lossy();
    let mut cmd = String::from("cat");
    for _ in 0..repeats {
        cmd.push(' ');
        cmd.push_str(&p);
    }
    // Clock starts the instant the command+Enter is fully sent, so the
    // (potentially long) keystroke stream itself isn't charged as drain
    // time — only the shell parsing/exec'ing cat and the parse that follows.
    type_command(&parent, &cmd);
    let t_issue = Instant::now();

    let secs = drain_window(t_issue, || {
        reader.read(&mut buf).map(|s| s.scroll_push_count).unwrap_or(0)
    });
    eprintln!("[l3] {} repeats of {} drained in {secs:.3}s", repeats, p);

    stop.store(true, Ordering::Relaxed);
    let _ = session.kill();
    let _ = session.wait();
    let _ = drain.join();
    let _ = client.kill_session(id);

    if secs <= 0.0 {
        die("no progress (cat didn't run)");
    }
    println!("{}", (secs * 1e9) as u64);
}

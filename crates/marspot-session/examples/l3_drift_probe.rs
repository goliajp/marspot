//! Sustained-load RSS-drift probe for one L3 session (perf-attack A1,
//! re-scoped to the per-session L3 architecture that is now the default).
//!
//! A1 ("active-9x-soak RSS drift") was filed against the *standalone*
//! `marspot` (in-process 9 grids in one address space). The product is now
//! shell→core→L3: the parser/terminal/grid/scrollback/PTY half lives in N
//! separate `marspot-session` processes. So the per-session leak
//! candidates A1 still has open — PTY chunk heap fragmentation, Terminal/
//! Session accumulation, scrollback ring growth — now live *here*. This
//! probe floods one L3 with continuous output and samples its RSS over a
//! sustained window, asserting the "cannot get slower the longer it runs"
//! commitment holds for the per-session engine (the factor that 9×
//! multiplies in the real product).
//!
//! A background thread drains the L3's GridReady pokes (as L2 would) so the
//! session keeps flowing; the main thread types a never-ending output loop
//! and samples the child's RSS every interval, then reports q4/q1 drift +
//! absolute growth and fails on a real leak.
//!
//!     MARSPOT_STATE_DIR=/tmp/marspot-dev \
//!       cargo run -p marspot-session --example l3_drift_probe -- \
//!       target/release/marspot-session [duration_s] [interval_s]
//!
//! Exit 0 = PASS, non-zero = FAIL (with a diagnostic on stderr).

use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use marspot_term::grid_shm::{create_region, GridShmReader};
use marspot_term::input_core::{KeyState, LogicalKey, MarspotKeyEvent, Modifiers, NamedKey};
use marspot_term::paths::shelld_socket;
use marspot_term::shell_proto::{encode_key_event, event_to_wire, Frame, MsgType};
use marspot_term::shelld_client::ShelldClient;

const COLS: u16 = 80;
const ROWS: u16 = 24;
/// Architectural commitment: post-warmup RSS must be flat. Allow a small
/// drift for the scrollback ring's lazy page-commit ramp; a real leak
/// (A1 was ~10 MiB/min → ~2× over the window) blows past this.
const DRIFT_MAX: f64 = 1.10;
/// Absolute growth ceiling over the window (KiB). The scrollback ring +
/// glyph state is bounded; anything beyond this is unbounded.
const ABS_GROWTH_KIB_MAX: i64 = 8 * 1024; // 8 MiB

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
    }
    let ev = MarspotKeyEvent { state: KeyState::Pressed, logical: LogicalKey::Named(NamedKey::Enter), text: None };
    let f = Frame::new(MsgType::KeyEvent, encode_key_event(&event_to_wire(&ev, Modifiers::default())));
    let mut w = sock;
    f.write_to(&mut w).unwrap_or_else(|e| die(format!("write enter: {e}")));
    w.flush().ok();
}

fn rss_kib(pid: u32) -> Option<i64> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse::<i64>().ok()
}

fn main() {
    let session_bin = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|e| e.parent()?.parent().map(|d| d.join("marspot-session")))
            .unwrap_or_else(|| die("can't locate marspot-session; pass it as argv[1]"))
    });
    if !session_bin.exists() {
        die(format!("marspot-session not found at {}", session_bin.display()));
    }
    let duration_s: u64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(60);
    let interval_s: u64 = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(5);

    let sock = shelld_socket();
    let client = ShelldClient::connect(&sock, || {})
        .unwrap_or_else(|e| die(format!("shelld connect at {}: {e}", sock.display())));
    let id = client.create_session(COLS, ROWS, "").unwrap_or_else(|e| die(format!("create_session: {e}")));

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
    let pid = session.id();
    eprintln!("[drift] L3 pid={pid}, flooding for {duration_s}s (sample every {interval_s}s)");
    drop(child);

    let reader = GridShmReader::from_fd(region.as_raw_fd()).unwrap_or_else(|e| die(format!("reader: {e}")));

    // Drain GridReady pokes in the background (as L2 would) so the session
    // never blocks on its poke write and keeps pumping at full rate.
    let stop = Arc::new(AtomicBool::new(false));
    let stop_r = stop.clone();
    let drain = {
        let mut rdr = parent.try_clone().unwrap_or_else(|e| die(format!("clone: {e}")));
        std::thread::spawn(move || {
            while !stop_r.load(Ordering::Relaxed) {
                if Frame::read_from(&mut rdr).map(|f| f.is_none()).unwrap_or(true) {
                    break;
                }
            }
        })
    };

    // Settle, then start a never-ending high-rate output loop.
    let mut buf = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while reader.read(&mut buf).is_none() {
        if Instant::now() >= deadline {
            die("no first publish");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    type_str(&parent, "while :; do seq 1 200; done");

    // Sample RSS across the window.
    let mut samples: Vec<i64> = Vec::new();
    let start = Instant::now();
    while start.elapsed().as_secs() < duration_s {
        std::thread::sleep(Duration::from_secs(interval_s));
        match rss_kib(pid) {
            Some(r) => {
                samples.push(r);
                eprintln!("[drift] t={:>4}s  rss={r} KiB", start.elapsed().as_secs());
            }
            None => break, // session gone
        }
    }

    stop.store(true, Ordering::Relaxed);
    let _ = session.kill();
    let _ = session.wait();
    let _ = drain.join();

    if samples.len() < 4 {
        die(format!("only {} samples — window too short / session died", samples.len()));
    }
    // q1/q4 means (quarter windows), like active-9x-soak.
    let q = samples.len() / 4;
    let mean = |sl: &[i64]| sl.iter().sum::<i64>() as f64 / sl.len() as f64;
    let q1 = mean(&samples[..q]);
    let q4 = mean(&samples[samples.len() - q..]);
    let drift = if q1 > 0.0 { q4 / q1 } else { 0.0 };
    let first = samples[0];
    let max = *samples.iter().max().unwrap();
    let abs_growth = max - first;
    eprintln!(
        "[drift] q1={q1:.0} q4={q4:.0} drift={drift:.3} first={first} max={max} abs_growth={abs_growth} KiB"
    );

    if drift > DRIFT_MAX {
        die(format!("RSS drift q4/q1 = {drift:.3} > {DRIFT_MAX} — per-session L3 leaks under sustained load"));
    }
    if abs_growth > ABS_GROWTH_KIB_MAX {
        die(format!("RSS abs growth {abs_growth} KiB > {ABS_GROWTH_KIB_MAX} KiB cap — unbounded source in L3"));
    }
    println!("PASS: L3 RSS bounded under {duration_s}s sustained output — drift {drift:.3} (≤{DRIFT_MAX}), abs growth {abs_growth} KiB (≤{ABS_GROWTH_KIB_MAX})");
}

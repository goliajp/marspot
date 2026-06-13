//! Apples-to-apples cat throughput: L3 pipeline vs the old in-process one.
//!
//! "Did making L3 the default cost bulk throughput?" can only be answered
//! by measuring BOTH paths the same way, same machine, same shelld, same
//! parser, same payload — so the ratio isolates exactly L3's added cost
//! (the per-pump ~40 KiB shm-window memcpy + the GridReady poke + the
//! extra process hop). Absolute MiB/s still swings with machine load;
//! the **ratio** is the honest signal.
//!
//! Both modes `cat` the same fixed-width-line file through a shelld
//! session and time from the first scroll_push_count advance (parse
//! started) to the last (parse done — count plateaus when cat finishes):
//!
//! - `inproc`: attach a `ShelldSession` and pump it in-process (exactly the
//!   pre-L3 `Pane::new_shelld` core path — shelld → client → parser → grid).
//! - `l3`: spawn a real `marspot-session` (parser → grid → shm publish);
//!   read scroll_push_count from the shm snapshot.
//!
//!     MARSPOT_STATE_DIR=... pipeline_throughput <marspot-session> [MiB]
//!
//! Runs both modes and prints each MiB/s + the L3/inproc ratio.

use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use marspot_term::grid::Cell;
use marspot_term::grid_shm::{create_region, GridShmReader};
use marspot_term::paths::shelld_socket;
use marspot_term::shell_proto::{Frame, MsgType};
use marspot_term::shelld_client::ShelldClient;

const COLS: u16 = 200;
const ROWS: u16 = 50;
const LINE_BYTES: u64 = 101; // 100 '0' + '\n'

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

fn make_payload(mib: u64) -> (String, u64) {
    let path = format!("/tmp/marspot-pipe-{}.txt", std::process::id());
    let mut line = vec![b'0'; (LINE_BYTES - 1) as usize];
    line.push(b'\n');
    let block: Vec<u8> = line.repeat(10_000); // ~1 MiB
    let blocks = (mib * 1024 * 1024) / block.len() as u64;
    let mut f = std::fs::File::create(&path).unwrap_or_else(|e| die(format!("payload: {e}")));
    for _ in 0..blocks {
        f.write_all(&block).unwrap_or_else(|e| die(format!("payload write: {e}")));
    }
    f.flush().ok();
    let lines = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) / LINE_BYTES;
    (path, lines)
}

/// Time a `cat` from first scroll_push_count advance to plateau, given a
/// closure that returns the current count (driving any pumping itself).
fn time_to_plateau<F: FnMut() -> u64>(mut count: F) -> (u64, f64) {
    let base = count();
    let start_dl = Instant::now() + Duration::from_secs(30);
    loop {
        if count() > base + 2_000 { break; }
        if Instant::now() >= start_dl { die("cat never started (prompt not ready?)"); }
        std::thread::sleep(Duration::from_millis(10));
    }
    let t0 = Instant::now();
    let c0 = count();
    let mut last = c0;
    let mut last_change = Instant::now();
    loop {
        let c = count();
        let now = Instant::now();
        if c != last {
            last = c;
            last_change = now;
        } else if now.duration_since(last_change) >= Duration::from_millis(300) {
            return (last - c0, last_change.duration_since(t0).as_secs_f64());
        }
        if now.duration_since(t0) >= Duration::from_secs(120) {
            return (last - c0, now.duration_since(t0).as_secs_f64());
        }
    }
}

fn mibps(lines: u64, secs: f64) -> f64 {
    if lines == 0 || secs <= 0.0 { die("no progress (cat didn't run)"); }
    (lines as f64 * LINE_BYTES as f64) / (1024.0 * 1024.0) / secs
}

/// Old core path: attach a ShelldSession, write `cat`, pump in-process.
fn measure_inproc(client: &ShelldClient, payload: &str) -> f64 {
    let id = client.create_session(COLS, ROWS, "").unwrap_or_else(|e| die(format!("create_session: {e}")));
    let mut s = client.attach(id, COLS, ROWS).unwrap_or_else(|e| die(format!("attach: {e}")));
    // Let the prompt arrive, draining it.
    let warm = Instant::now() + Duration::from_millis(800);
    while Instant::now() < warm { s.pump(); std::thread::sleep(Duration::from_millis(5)); }
    s.write(format!("cat {payload}\n").as_bytes()).unwrap_or_else(|e| die(format!("write: {e}")));
    let (lines, secs) = time_to_plateau(|| { s.pump(); s.terminal().grid().scroll_push_count() });
    eprintln!("[inproc] {lines} lines in {secs:.3}s");
    let _ = client.kill_session(id);
    mibps(lines, secs)
}

/// L3 path: spawn marspot-session, cat over the control socket, read shm.
fn measure_l3(client: &ShelldClient, session_bin: &PathBuf, payload: &str) -> f64 {
    use marspot_term::input_core::{KeyState, LogicalKey, MarspotKeyEvent, Modifiers, NamedKey};
    use marspot_term::shell_proto::{encode_key_event, event_to_wire};

    let id = client.create_session(COLS, ROWS, "").unwrap_or_else(|e| die(format!("create_session: {e}")));
    let region = create_region(COLS, ROWS).unwrap_or_else(|e| die(format!("create_region: {e}")));
    clear_cloexec(region.as_raw_fd());
    let (parent, child) = UnixStream::pair().unwrap_or_else(|e| die(format!("pair: {e}")));
    clear_cloexec(child.as_raw_fd());
    let mut session = Command::new(session_bin)
        .env("MARSPOT_SHELL_CONTROL_FD", child.as_raw_fd().to_string())
        .env("MARSPOT_SHM_FD", region.as_raw_fd().to_string())
        .env("MARSPOT_SESSION_ID", id.to_string())
        .spawn().unwrap_or_else(|e| die(format!("spawn: {e}")));
    drop(child);
    let reader = GridShmReader::from_fd(region.as_raw_fd()).unwrap_or_else(|e| die(format!("reader: {e}")));

    let stop = Arc::new(AtomicBool::new(false));
    let stop_r = stop.clone();
    let mut rdr = parent.try_clone().unwrap_or_else(|e| die(format!("clone: {e}")));
    let drain = std::thread::spawn(move || {
        while !stop_r.load(Ordering::Relaxed) {
            if Frame::read_from(&mut rdr).map(|f| f.is_none()).unwrap_or(true) { break; }
        }
    });

    let mut buf: Vec<Cell> = Vec::new();
    let dl = Instant::now() + Duration::from_secs(10);
    while reader.read(&mut buf).is_none() {
        if Instant::now() >= dl { die("no first publish"); }
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(1500));
    // Type `cat payload` over the control socket.
    let cmd = format!("cat {payload}");
    for c in cmd.chars().map(Some).chain(std::iter::once(None)) {
        let ev = match c {
            Some(ch) => MarspotKeyEvent { state: KeyState::Pressed, logical: LogicalKey::Char(ch), text: Some(ch.to_string()) },
            None => MarspotKeyEvent { state: KeyState::Pressed, logical: LogicalKey::Named(NamedKey::Enter), text: None },
        };
        let mut w = &parent;
        Frame::new(MsgType::KeyEvent, encode_key_event(&event_to_wire(&ev, Modifiers::default())))
            .write_to(&mut w).unwrap_or_else(|e| die(format!("type: {e}")));
        w.flush().ok();
    }
    let (lines, secs) = time_to_plateau(|| reader.read(&mut buf).map(|s| s.scroll_push_count).unwrap_or(0));
    eprintln!("[l3] {lines} lines in {secs:.3}s");
    stop.store(true, Ordering::Relaxed);
    let _ = session.kill(); let _ = session.wait(); let _ = drain.join();
    let _ = client.kill_session(id);
    mibps(lines, secs)
}

fn main() {
    let session_bin = std::env::args().nth(1).map(PathBuf::from)
        .unwrap_or_else(|| die("usage: pipeline_throughput <marspot-session> [MiB]"));
    if !session_bin.exists() { die(format!("not found: {}", session_bin.display())); }
    let mib: u64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(64);

    let (payload, file_lines) = make_payload(mib);
    eprintln!("[payload] {file_lines} lines in {mib} MiB");
    let sock = shelld_socket();
    let client = ShelldClient::connect(&sock, || {}).unwrap_or_else(|e| die(format!("shelld connect: {e}")));

    // inproc first (no spawned process to interfere), then L3.
    let inproc = measure_inproc(&client, &payload);
    let l3 = measure_l3(&client, &session_bin, &payload);
    let _ = std::fs::remove_file(&payload);

    let ratio = if inproc > 0.0 { l3 / inproc } else { 0.0 };
    println!(
        "PASS: cat {mib} MiB — inproc {inproc:.1} MiB/s, L3 {l3:.1} MiB/s, ratio L3/inproc = {ratio:.2}"
    );
}

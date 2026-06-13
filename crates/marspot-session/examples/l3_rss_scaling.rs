//! Per-session idle RSS scaling under L3 (perf-attack C re-scope).
//!
//! C ("per-session RSS bloat vs Terminal.app") was filed against the
//! *standalone* marspot — one process holding 9 in-process grids + Metal +
//! a per-cell scrollback ring + the 16 MiB glyph atlas — which idled at
//! +90 MiB for 9 sessions (the same lazy-fault-into-mmap-ring trajectory as
//! A1). A1 was resolved under L3 (the growth candidates moved into separate
//! bounded processes). C must be re-measured the same way: what does each
//! added session actually cost under shell→core→L3?
//!
//! This spawns N idle `marspot-session` processes (inherited shm region +
//! control socket, exactly as marspot-core::spawn_l3_pane), lets each reach
//! its prompt, and sums their RSS — isolating the L3-side per-session
//! footprint (the part that replaced the standalone per-cell ring + grid).
//! The core's per-session cost is a synthetic grid mirror (~cols×rows cells,
//! a few KiB) and one shared atlas; the shell owns the single window. So the
//! L3-process total here IS the marginal per-session cost the user pays.
//!
//!     MARSPOT_STATE_DIR=… l3_rss_scaling <marspot-session> <N>
//!
//! Prints `N <total_kib> <per_session_kib>` to stdout (diagnostics to
//! stderr). bin/soak-l3-rss-scaling.sh runs it across N and checks the slope.

use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use marspot_term::grid::Cell;
use marspot_term::grid_shm::{create_region, GridShmReader};
use marspot_term::paths::shelld_socket;
use marspot_term::shell_proto::Frame;
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

fn rss_kib(pid: u32) -> Option<i64> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

struct L3 {
    id: u64,
    child: Child,
    pid: u32,
    // Kept alive so the control socket / reader thread stay up.
    _parent: UnixStream,
    stop: Arc<AtomicBool>,
    drain: Option<JoinHandle<()>>,
    reader: GridShmReader,
}

fn spawn_idle_l3(client: &ShelldClient, session_bin: &PathBuf) -> L3 {
    let id = client
        .create_session(COLS, ROWS, "")
        .unwrap_or_else(|e| die(format!("create_session: {e}")));
    let region = create_region(COLS, ROWS).unwrap_or_else(|e| die(format!("create_region: {e}")));
    clear_cloexec(region.as_raw_fd());
    let (parent, child_sock) = UnixStream::pair().unwrap_or_else(|e| die(format!("pair: {e}")));
    clear_cloexec(child_sock.as_raw_fd());
    let child = Command::new(session_bin)
        .env("MARSPOT_SHELL_CONTROL_FD", child_sock.as_raw_fd().to_string())
        .env("MARSPOT_SHM_FD", region.as_raw_fd().to_string())
        .env("MARSPOT_SESSION_ID", id.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| die(format!("spawn session: {e}")));
    drop(child_sock);
    let pid = child.id();
    let reader =
        GridShmReader::from_fd(region.as_raw_fd()).unwrap_or_else(|e| die(format!("reader: {e}")));

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

    // Wait for first publish so the session is fully up (prompt drawn).
    let mut buf: Vec<Cell> = Vec::new();
    let dl = Instant::now() + Duration::from_secs(10);
    while reader.read(&mut buf).is_none() {
        if Instant::now() >= dl {
            die("no first publish");
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    L3 { id, child, pid, _parent: parent, stop, drain: Some(drain), reader }
}

fn main() {
    let session_bin = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| die("usage: l3_rss_scaling <marspot-session> <N>"));
    let n: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(9).max(1);
    if !session_bin.exists() {
        die(format!("not found: {}", session_bin.display()));
    }

    let sock = shelld_socket();
    let client = ShelldClient::connect(&sock, || {})
        .unwrap_or_else(|e| die(format!("shelld connect ({}): {e}", sock.display())));

    let mut sessions: Vec<L3> = Vec::with_capacity(n);
    for i in 0..n {
        sessions.push(spawn_idle_l3(&client, &session_bin));
        eprintln!("[rss-scaling] spawned idle L3 {}/{n} pid={}", i + 1, sessions[i].pid);
    }

    // Let every session settle past startup allocations.
    std::thread::sleep(Duration::from_millis(2000));

    let mut total: i64 = 0;
    for s in &sessions {
        match rss_kib(s.pid) {
            Some(r) => {
                eprintln!("[rss-scaling] pid={} rss={r} KiB", s.pid);
                total += r;
            }
            None => die(format!("rss read failed for pid {}", s.pid)),
        }
    }
    let per = total / n as i64;
    // Touch the readers once so the optimiser can't drop the regions early.
    let mut buf: Vec<Cell> = Vec::new();
    for s in &sessions {
        let _ = s.reader.read(&mut buf);
    }

    // Teardown: stop drain threads, kill + reap children.
    for s in &mut sessions {
        s.stop.store(true, Ordering::Relaxed);
    }
    for s in &mut sessions {
        let _ = s.child.kill();
        let _ = s.child.wait();
        if let Some(h) = s.drain.take() {
            let _ = h.join();
        }
        let _ = client.kill_session(s.id);
    }

    println!("{n} {total} {per}");
}

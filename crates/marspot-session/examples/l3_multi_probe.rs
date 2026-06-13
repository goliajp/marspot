//! Headless probe for the N-session L3 path (target #4 step 4a).
//!
//! Stands in for L2's boot: it connects to shelld as L2 does, allocates
//! N **distinct** sessions via `ShelldClient::create_session` (the
//! L2-allocates / L3-attaches split), spawns one real `marspot-session`
//! per session — each with its own shm region, control socket, and
//! `MARSPOT_SESSION_ID` — types a *distinct* character into each, and
//! asserts every region echoes **only its own** character. That proves
//! the invariant 4a turns on: N L3 children never race for one session
//! (a collision would make two regions show the same char, or one show
//! the other's).
//!
//! Requires a reachable shelld (the dev sandbox's, via `MARSPOT_STATE_DIR`).
//! Run via `bin/soak-l3.sh`; standalone:
//!
//!     MARSPOT_STATE_DIR=/tmp/marspot-dev \
//!       cargo run -p marspot-session --example l3_multi_probe -- \
//!       target/release/marspot-session 4
//!
//! Exit 0 = PASS, non-zero = FAIL (with a diagnostic on stderr).

use std::collections::HashSet;
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

fn wait_seq_past(reader: &GridShmReader, from: u64, what: &str) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let s = reader.seq();
        if s != from && s != 0 {
            return s;
        }
        if Instant::now() >= deadline {
            die(format!("timed out waiting for {what} (seq stuck at {from})"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Block until the publish seq has been stable for `quiet`, i.e. the
/// prompt has finished printing. Sampling a cursor before quiescence is
/// the classic flake: under load the prompt is still drawing, so the
/// recorded cursor is stale and the typed char echoes elsewhere.
fn wait_quiescent(reader: &GridShmReader, quiet: Duration) {
    let cap = Instant::now() + Duration::from_secs(8);
    let mut last = reader.seq();
    let mut stable_since = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(20));
        let s = reader.seq();
        if s != last {
            last = s;
            stable_since = Instant::now();
        } else if stable_since.elapsed() >= quiet {
            return;
        }
        if Instant::now() >= cap {
            return; // give up waiting for perfect quiet; sample anyway
        }
    }
}

/// One L3 child plus the L2-side handles needed to drive + observe it.
struct L3 {
    id: u64,
    typed: char,
    child: Child,
    parent: UnixStream,
    reader: GridShmReader,
    // Keeps the region fd alive for the child's lifetime (the child maps
    // the same fd it inherited).
    _region: OwnedFd,
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
    let n: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(4);
    assert!((1..=20).contains(&n), "N out of range: {n}");

    // Connect to shelld exactly as L2 does — same socket path resolution
    // (keys off MARSPOT_STATE_DIR), no-op wake (we poll the shm).
    let sock = shelld_socket();
    let client = ShelldClient::connect(&sock, || {})
        .unwrap_or_else(|e| die(format!("shelld connect at {}: {e}", sock.display())));

    // L2 allocates N distinct sessions, then spawns one L3 per session.
    let mut l3s: Vec<L3> = Vec::with_capacity(n);
    for i in 0..n {
        let id = client
            .create_session(COLS, ROWS, "")
            .unwrap_or_else(|e| die(format!("create_session #{i}: {e}")));

        let region = create_region(COLS, ROWS).unwrap_or_else(|e| die(format!("create_region: {e}")));
        clear_cloexec(region.as_raw_fd());
        let (parent, child_sock) =
            UnixStream::pair().unwrap_or_else(|e| die(format!("socket pair: {e}")));
        clear_cloexec(child_sock.as_raw_fd());

        let child = Command::new(&session_bin)
            .env("MARSPOT_SHELL_CONTROL_FD", child_sock.as_raw_fd().to_string())
            .env("MARSPOT_SHM_FD", region.as_raw_fd().to_string())
            .env("MARSPOT_SESSION_ID", id.to_string())
            .spawn()
            .unwrap_or_else(|e| die(format!("spawn #{i}: {e}")));
        drop(child_sock);

        let reader = GridShmReader::from_fd(region.as_raw_fd())
            .unwrap_or_else(|e| die(format!("reader #{i}: {e}")));

        let typed = (b'a' + i as u8) as char;
        eprintln!("[multi] L3 #{i}: session_id={id} pid={} typed='{typed}'", child.id());
        l3s.push(L3 { id, typed, child, parent, reader, _region: region });
    }

    // Distinct ids by construction — assert it loudly (a regression where
    // create_session reused an id would collide here).
    let unique: HashSet<u64> = l3s.iter().map(|l| l.id).collect();
    if unique.len() != n {
        die(format!("create_session handed out duplicate ids: {:?}", l3s.iter().map(|l| l.id).collect::<Vec<_>>()));
    }

    // Let every prompt settle (wait for the publish stream to go quiet,
    // not a fixed sleep — under load the prompt may still be drawing), then
    // record each one's pre-key cursor.
    let mut cursors = Vec::with_capacity(n);
    for l in &l3s {
        let _ = wait_seq_past(&l.reader, 0, "first publish");
    }
    for l in &l3s {
        wait_quiescent(&l.reader, Duration::from_millis(250));
    }
    let mut buf = Vec::new();
    for l in &l3s {
        let snap = l.reader.read(&mut buf).unwrap_or_else(|| die("no frame after settle"));
        cursors.push((snap.cursor_col, snap.cursor_row, l.reader.seq()));
    }

    // Type each L3's distinct char into it.
    for l in &l3s {
        let ev = MarspotKeyEvent {
            state: KeyState::Pressed,
            logical: LogicalKey::Char(l.typed),
            text: Some(l.typed.to_string()),
        };
        let frame = Frame::new(
            MsgType::KeyEvent,
            encode_key_event(&event_to_wire(&ev, Modifiers::default())),
        );
        let mut w = &l.parent;
        frame.write_to(&mut w).unwrap_or_else(|e| die(format!("write key #{}: {e}", l.id)));
        w.flush().ok();
    }

    // Each region must show ITS OWN char at its own pre-key cursor.  This
    // alone is a robust collision check: if two L3 shared one session,
    // both recorded the same prompt cursor `c0`, and the first char
    // written there (the lower-index L3's) is what both regions would
    // show — so the higher-index L3's "is my char at c0?" check fails.
    // (A whole-grid scan for sibling chars would false-positive on prompt
    // text — usernames/paths contain a-z — so we don't do that.)
    for (i, l) in l3s.iter().enumerate() {
        let (c0, r0, seq_before) = cursors[i];
        wait_seq_past(&l.reader, seq_before, "echo publish");
        std::thread::sleep(Duration::from_millis(100));
        let _ = l.reader.read(&mut buf).unwrap_or_else(|| die("no frame after key"));
        let idx = r0 as usize * COLS as usize + c0 as usize;
        let landed = buf[idx].ch;
        if landed != l.typed {
            die(format!(
                "L3 #{i} (id={}): typed '{}' but cell ({c0},{r0}) shows {landed:?} \
                 (session collision, or echo lost)",
                l.id, l.typed
            ));
        }
    }

    // Teardown: kill + reap every child (no orphans).
    for l in &mut l3s {
        let _ = l.child.kill();
        let _ = l.child.wait();
    }

    println!("PASS: {n} L3 sessions, distinct ids, each echoed only its own char (no collision)");
}

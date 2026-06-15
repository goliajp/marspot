//! Headless client driver for marspot-shelld. Used by the execv-swap
//! soak (`bin/soak-shelld-execv-swap.sh`) to create sessions, list
//! them, and verify continuity across a SIGUSR1-driven in-place
//! self-update of the daemon.
//!
//! Subcommands (all driven via env: MARSPOT_STATE_DIR + the shelld
//! socket implicitly resolved from it):
//!
//!   create <N>   open N sessions, print "<id>\t<child_pid>" per
//!                line to stdout, exit (closes connection).
//!   list         print "<id>\t<child_pid>\t<alive>" per line for
//!                every live session shelld knows about, exit.
//!   write <id>   send the rest of stdin to session <id> as INPUT
//!                frames, then exit.
//!
//! The script side compares the post-swap `list` to the pre-swap
//! `create` output: same ids, same child PIDs, all alive = sessions
//! survived the execv image swap.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use marspot_term::paths::shelld_socket;
use marspot_term::shelld_client::ShelldClient;

const COLS: u16 = 80;
const ROWS: u16 = 24;

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("FAIL: {msg}");
    std::process::exit(1);
}

fn connect() -> ShelldClient {
    let path = shelld_socket();
    let wake = Arc::new(AtomicBool::new(false));
    let wk = wake.clone();
    ShelldClient::connect(&path, move || wk.store(true, Ordering::Release))
        .unwrap_or_else(|e| die(format!("connect {}: {e}", path.display())))
}

fn cmd_create(n: usize) {
    let client = connect();
    let cwd = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    for _ in 0..n {
        let id = client
            .create_session(COLS, ROWS, &cwd)
            .unwrap_or_else(|e| die(format!("create_session: {e}")));
        // Pull child_pid back via list_sessions — we can't ask
        // create_session for it directly.
        let sessions = client
            .list_sessions()
            .unwrap_or_else(|e| die(format!("list_sessions: {e}")));
        let info = sessions
            .iter()
            .find(|s| s.session_id == id)
            .unwrap_or_else(|| die(format!("created session {id} missing from list")));
        println!("{}\t{}", info.session_id, info.child_pid);
    }
}

fn cmd_list() {
    let client = connect();
    let sessions = client
        .list_sessions()
        .unwrap_or_else(|e| die(format!("list_sessions: {e}")));
    for s in sessions {
        println!("{}\t{}\t{}", s.session_id, s.child_pid, s.alive);
    }
}

fn cmd_write(id: u64) {
    let client = connect();
    // attach so we own the session under the same connection's write
    // mutex — write() is on ShelldSession.
    let mut session = client
        .attach(id, COLS, ROWS)
        .unwrap_or_else(|e| die(format!("attach {id}: {e}")));
    let mut buf = Vec::new();
    std::io::stdin()
        .read_to_end(&mut buf)
        .unwrap_or_else(|e| die(format!("read stdin: {e}")));
    session
        .write(&buf)
        .unwrap_or_else(|e| die(format!("write to session {id}: {e}")));
}

/// Attach, pump until the StateSnapshot is applied (≤ deadline), and
/// print "<id>\t<sha256-of-grid-cells-hex>\t<generation>" so the soak
/// script can diff fingerprints across an execv swap.
///
/// RFC-002 step 10: invariant being tested = the L4 Terminal serializes
/// + persists + rehydrates verbatim across execv, so a snapshot
/// captured from a re-attached client after a swap is bit-for-bit
/// what a same-instant pre-swap attach would have returned.
fn cmd_fingerprint(id: u64) {
    let client = connect();
    let mut session = client
        .attach(id, COLS, ROWS)
        .unwrap_or_else(|e| die(format!("attach {id}: {e}")));
    // Drain StateSnapshot.  pump's first non-zero return after attach
    // = snapshot applied.  We poll briefly to ride past any latency
    // in the reader thread queueing the frame.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut applied = false;
    while std::time::Instant::now() < deadline {
        if session.pump() > 0 {
            applied = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if !applied {
        die(format!("session {id} produced no snapshot within 2s"));
    }
    let term = session.terminal();
    let grid = term.grid();
    let (cols, rows) = (grid.cols(), grid.rows());
    // FNV-1a 64-bit over (ch, attrs.fg, attrs.bg, attrs.flags-bits) per
    // cell, row-major.  Tiny, dependency-free, plenty discriminative
    // for "did the grid round-trip identically?".
    let mut h: u64 = 0xcbf29ce484222325;
    for r in 0..rows {
        for c in 0..cols {
            let cell = grid.cell(c, r);
            for &b in (cell.ch as u32).to_le_bytes().iter() {
                h ^= b as u64;
                h = h.wrapping_mul(0x00000100000001B3);
            }
            for &b in serialize_attrs_flat(cell.attrs).iter() {
                h ^= b as u64;
                h = h.wrapping_mul(0x00000100000001B3);
            }
        }
    }
    println!("{}\t{:016x}\t{}", id, h, term.generation());
}

fn serialize_attrs_flat(a: marspot_term::grid::CellAttrs) -> [u8; 4] {
    let mut flags: u8 = 0;
    if a.bold { flags |= 1; }
    if a.italic { flags |= 2; }
    if a.underline { flags |= 4; }
    if a.reverse { flags |= 8; }
    if a.dim { flags |= 16; }
    let (fg_k, fg_p) = color_kind_payload(a.fg);
    let (bg_k, bg_p) = color_kind_payload(a.bg);
    [flags, fg_k ^ fg_p, bg_k ^ bg_p, 0]
}

fn color_kind_payload(c: marspot_term::grid::Color) -> (u8, u8) {
    match c {
        marspot_term::grid::Color::Default => (0, 0),
        marspot_term::grid::Color::Indexed(i) => (1, i),
        marspot_term::grid::Color::Rgb(r, g, b) => (2, r ^ g ^ b),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("create") => {
            let n: usize = args
                .get(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| die("usage: shelld_session_probe create <N>"));
            cmd_create(n);
        }
        Some("list") => cmd_list(),
        Some("write") => {
            let id: u64 = args
                .get(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| die("usage: shelld_session_probe write <id>"));
            cmd_write(id);
        }
        Some("fingerprint") => {
            let id: u64 = args
                .get(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| die("usage: shelld_session_probe fingerprint <id>"));
            cmd_fingerprint(id);
        }
        _ => die(
            "usage: shelld_session_probe (create N | list | write id | fingerprint id)",
        ),
    }
}

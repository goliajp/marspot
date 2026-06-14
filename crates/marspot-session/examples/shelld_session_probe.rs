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
        _ => die("usage: shelld_session_probe (create N | list | write id)"),
    }
}

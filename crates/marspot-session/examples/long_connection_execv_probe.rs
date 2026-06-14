//! Long-lived ShelldClient connection across a shelld execv self-update.
//!
//! Before this PR, the client's reader thread treated socket EOF (which
//! is what an execv-driven shelld restart looks like from the wire side,
//! because Rust's std accept sets CLOEXEC on the accepted fd) as
//! "every session has exited" — flipping every pane's state to Exited
//! and prompting marspot-core to terminate. Now a `supervisor_loop`
//! reconnects with exponential backoff, re-handshakes, and re-attaches
//! every session id, so the bytelog replay restores terminal state and
//! the GUI sees a momentary read pause rather than the world ending.
//!
//! This probe exercises that path:
//!   1. Open ShelldClient, attach 2 sessions, write a marker to each.
//!   2. Print PROBE_READY <session_ids...> and wait until the
//!      orchestrator (bin/test-long-connection-execv.sh) sends
//!      SIGCONT — by then the orchestrator has SIGUSR1'd shelld.
//!   3. Assert `is_exited()` is still false on every session.
//!   4. Write a second marker through the same client handle; the
//!      send must succeed (proving the writer survived the reconnect).
//!
//! Exit 0 = sessions survived a real shelld execv via supervisor
//! reconnect; non-zero = regression.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use marspot_term::paths::shelld_socket;
use marspot_term::shelld_client::ShelldClient;

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("FAIL: {msg}");
    std::process::exit(1);
}

fn main() {
    let path = shelld_socket();
    // Sanity: socket must exist before we connect.
    UnixStream::connect(&path)
        .unwrap_or_else(|e| die(format!("connect {}: {e}", path.display())));

    let wake = Arc::new(AtomicBool::new(false));
    let wk = wake.clone();
    let client = ShelldClient::connect(&path, move || wk.store(true, Ordering::Release))
        .unwrap_or_else(|e| die(format!("ShelldClient::connect: {e}")));

    let cwd = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    // Two sessions so we exercise the supervisor's re-attach-each-id
    // loop, not just the lucky one-session case.
    let mut sessions: Vec<_> = (0..2)
        .map(|i| {
            client
                .new_session(80, 24, &cwd)
                .unwrap_or_else(|e| die(format!("new_session #{i}: {e}")))
        })
        .collect();

    // Phase 1: emit a marker, drain briefly, prove we have a live link.
    for s in sessions.iter_mut() {
        s.write(b"echo PRE_MARK\n")
            .unwrap_or_else(|e| die(format!("write PRE_MARK on {}: {e}", s.id())));
    }
    std::thread::sleep(Duration::from_millis(500));
    for s in sessions.iter_mut() {
        s.pump();
        if s.is_exited() {
            die(format!("session {} reported exited BEFORE execv", s.id()));
        }
    }

    // Hand control to the orchestrator: print ids + flush, then suspend
    // ourselves so it can SIGUSR1 the daemon at a known-quiescent moment.
    let ids: Vec<String> = sessions.iter().map(|s| s.id().to_string()).collect();
    println!("PROBE_READY {}", ids.join(","));
    std::io::stdout().flush().ok();
    // Wait for SIGCONT from the orchestrator (sent after kill -USR1 shelld).
    let pid = unsafe { libc::getpid() };
    unsafe {
        libc::kill(pid, libc::SIGSTOP);
    }

    // Phase 2: assert sessions are NOT exited despite the daemon swap.
    // Reconnect happens in the supervisor thread; give it generous head
    // room — 4 s covers the worst of the 9-step backoff.
    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        for s in sessions.iter_mut() {
            s.pump();
            if s.is_exited() {
                die(format!(
                    "session {} reported exited DURING reconnect window",
                    s.id()
                ));
            }
        }
    }

    // Phase 3: writer must work post-reconnect. We don't need to
    // observe the echo through the local terminal — we only need the
    // send_frame call (driven by ShelldSession::write) to succeed,
    // which proves the writer Mutex<UnixStream> got swapped to a new
    // live stream.
    for s in sessions.iter_mut() {
        s.write(b"echo POST_MARK\n").unwrap_or_else(|e| {
            die(format!(
                "write POST_MARK on session {} after reconnect: {e}",
                s.id()
            ))
        });
    }

    println!("PROBE_PASS {} sessions survived execv", sessions.len());
}

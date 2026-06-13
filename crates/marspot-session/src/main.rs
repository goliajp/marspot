//! marspot-session — the per-session L3 process (target #4).
//!
//! Runs ONE terminal session's parser/terminal half and nothing else:
//! it connects to shelld, attaches (or creates) a single session, and
//! pumps PTY bytes through the VT parser into a `Grid`.  It links only
//! `marspot-term` (the zero-GUI engine) — no Metal, AppKit, or
//! CoreText — so it floors at ~3–5 MB resident.  The renderer (L2,
//! `marspot-core`) will read this process's grid over shared memory and
//! composite it; that, plus the L2↔L3 control channel and per-session
//! silent update, land in later steps.  See `docs/per-session-l3.md`.
//!
//! This skeleton (step 1) just proves the floor: boot, attach shelld,
//! pump bytes → grid, and log grid geometry. Event-driven — it blocks
//! on the shelld wake until there's work, so idle CPU is ~0.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use marspot_term::paths::shelld_socket;
use marspot_term::shelld_client::{SessionState, ShelldClient};
use marspot_term::shelld_proto::SessionInfo;

/// Placeholder geometry until L2 drives a real resize (later step).
const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;

fn state_str(s: SessionState) -> &'static str {
    match s {
        SessionState::Active => "active",
        SessionState::Idle => "idle",
        SessionState::Exited => "exited",
    }
}

fn main() {
    eprintln!(
        "marspot-session {} (git {}) pid={}",
        env!("CARGO_PKG_VERSION"),
        option_env!("MARSPOT_GIT_SHA").unwrap_or("unknown"),
        std::process::id()
    );

    // Event-driven wake: shelld's reader thread pokes this channel
    // whenever the session has new bytes (or hits EOF), so the main
    // loop sleeps until there's real work.
    let (wake_tx, wake_rx): (Sender<()>, Receiver<()>) = mpsc::channel();
    let wake = move || {
        let _ = wake_tx.send(());
    };

    let sock = shelld_socket();
    eprintln!("[session] connecting to shelld at {}", sock.display());
    let client = match ShelldClient::connect(&sock, wake) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[session] shelld connect failed: {e}");
            std::process::exit(1);
        }
    };

    // Pick the session to drive: an explicit MARSPOT_SESSION_ID if it
    // names a live one, else the first live session (reattach + bytelog
    // replay), else a fresh session.
    let want: Option<u64> = std::env::var("MARSPOT_SESSION_ID")
        .ok()
        .and_then(|s| s.parse().ok());
    let existing: Vec<SessionInfo> = client
        .list_sessions()
        .unwrap_or_else(|e| {
            eprintln!("[session] list_sessions failed: {e} — starting fresh");
            Vec::new()
        })
        .into_iter()
        .filter(|s| s.alive)
        .collect();
    let target = want
        .filter(|id| existing.iter().any(|s| s.session_id == *id))
        .or_else(|| existing.first().map(|s| s.session_id));

    let mut session = match target {
        Some(id) => {
            eprintln!("[session] attaching existing session id={id}");
            match client.attach(id, INITIAL_COLS, INITIAL_ROWS) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[session] attach {id} failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        None => {
            eprintln!("[session] no live session; creating a fresh one");
            match client.new_session(INITIAL_COLS, INITIAL_ROWS, "") {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[session] new_session failed: {e}");
                    std::process::exit(1);
                }
            }
        }
    };
    eprintln!(
        "[session] driving session id={} pid={} ({INITIAL_COLS}x{INITIAL_ROWS})",
        session.id(),
        session.child_pid()
    );

    let start = Instant::now();
    let mut frame: u64 = 0;
    loop {
        match wake_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("[session] shelld wake channel closed; exiting");
                break;
            }
        }
        // Coalesce any queued wakes — one pump drains everything ready.
        while wake_rx.try_recv().is_ok() {}

        let n = session.pump();
        if session.is_exited() {
            session.pump();
            eprintln!("[session] session exited; exiting cleanly");
            break;
        }

        frame += 1;
        // Log on any byte activity, plus a heartbeat every ~minute of
        // idle 5 s ticks so a wedged session is visible in the log.
        if n > 0 || frame.is_multiple_of(12) {
            let g = session.terminal().grid();
            let (cc, cr) = g.cursor();
            eprintln!(
                "[session] t={:.1}s pumped={n} grid={}x{} cursor=({cc},{cr}) state={}",
                start.elapsed().as_secs_f64(),
                g.cols(),
                g.rows(),
                state_str(session.state()),
            );
        }
    }
}

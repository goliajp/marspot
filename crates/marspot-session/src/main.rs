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
//! Step 1 proved the floor (boot, attach shelld, pump bytes → grid).
//! Step 3a makes L3 a real session backend: it takes the grid-shm
//! region L2 created (inherited as `MARSPOT_SHM_FD`) as the writer, and
//! reads keystrokes L2 forwards over the control socket (fd 3,
//! `MARSPOT_SHELL_CONTROL_FD`) — encoding them itself with its own
//! terminal mode flags, writing to the PTY, and local-echoing ahead of
//! the round trip. Still event-driven: it blocks on a unified event
//! channel fed by both the shelld wake and the control reader, so idle
//! CPU is ~0. With neither env var set it stays fully standalone
//! (self-creates the region, no input source) for the dev/test path.

use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use marspot_term::grid_shm::{
    GridShmWriter, ENV_SHM_FD, FLAG_APP_CURSOR_KEYS, FLAG_BRACKETED_PASTE, FLAG_CURSOR_VISIBLE,
};
use marspot_term::input_core::{MarspotKeyEvent, Modifiers};
use marspot_term::paths::shelld_socket;
use marspot_term::shell_proto::{
    decode_key_event, wire_to_event, Frame, MsgType, DEFAULT_CONTROL_FD, ENV_CONTROL_FD,
};
use marspot_term::shelld_client::{SessionState, ShelldClient, ShelldSession};
use marspot_term::shelld_proto::SessionInfo;

/// What wakes the L3 main loop. Both arms arrive on one channel so the
/// loop blocks in a single place (idle CPU ~0): the shelld reader thread
/// sends `Wake` when the PTY has bytes/EOF, the control reader sends
/// `Key` when L2 forwards a keystroke.
enum SessionEvent {
    Wake,
    Key(MarspotKeyEvent, Modifiers),
}

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

/// Publish the session's current grid (live view) + cursor/mode flags
/// into the shared framebuffer for L2 to render.
fn publish(shm: &mut GridShmWriter, session: &marspot_term::shelld_client::ShelldSession) {
    let term = session.terminal();
    let mut flags = 0u32;
    if term.cursor_visible() {
        flags |= FLAG_CURSOR_VISIBLE;
    }
    if term.cursor_key_application_mode() {
        flags |= FLAG_APP_CURSOR_KEYS;
    }
    if term.bracketed_paste_mode() {
        flags |= FLAG_BRACKETED_PASTE;
    }
    shm.publish(term.grid(), 0, flags);
}

/// Encode one forwarded keystroke with L3's own terminal mode flags,
/// write it to the PTY, and local-echo printable bytes ahead of the
/// round trip. Returns whether the echo painted the grid (so the caller
/// republishes even when no PTY bytes pumped this tick).
///
/// Clipboard reads resolve to `None` here: Cmd-V paste is forwarded by
/// L2 as already-resolved text in a later step, not pulled from the
/// pasteboard by the GUI-free L3.
fn handle_key(session: &mut ShelldSession, event: MarspotKeyEvent, mods: Modifiers) -> bool {
    let (app_mode, bracketed) = {
        let t = session.terminal();
        (t.cursor_key_application_mode(), t.bracketed_paste_mode())
    };
    let Some(bytes) =
        marspot_term::input_core::key_event_to_bytes(&event, mods, app_mode, bracketed, || None)
    else {
        return false;
    };
    let _ = session.write(&bytes);
    let mut predicted = false;
    for &b in bytes.as_ref() {
        if session.terminal_mut().predict_byte(b) {
            predicted = true;
        }
    }
    predicted
}

/// If L2 handed us a control socket (fd `MARSPOT_SHELL_CONTROL_FD`,
/// default 3), spawn a reader thread that decodes forwarded `KeyEvent`
/// frames and feeds them to the main loop, and return a write half so
/// the loop can poke L2 with `GridReady` after each publish. EOF / error
/// on the read side means L2 is gone, so the thread just exits (the
/// loop's heartbeat + shelld wake keep the session alive regardless).
/// No env var → standalone, no input source, no writer.
fn setup_control_socket(tx: Sender<SessionEvent>) -> Option<UnixStream> {
    let fd: RawFd = match std::env::var(ENV_CONTROL_FD) {
        Ok(s) => match s.parse() {
            Ok(fd) => fd,
            Err(_) => {
                eprintln!("[session] bad {ENV_CONTROL_FD}={s:?}; ignoring control socket");
                return None;
            }
        },
        Err(_) => {
            let _ = DEFAULT_CONTROL_FD; // standalone: no control socket
            return None;
        }
    };
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let writer = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[session] control socket try_clone failed: {e}");
            return None;
        }
    };
    eprintln!("[session] control socket on fd {fd}");
    let mut reader = stream;
    std::thread::spawn(move || loop {
        match Frame::read_from(&mut reader) {
            Ok(Some(f)) => {
                // KeyEvent is all L3 acts on for now; resize/paste/etc.
                // arrive in later steps.
                if f.msg_type == MsgType::KeyEvent {
                    if let Ok(w) = decode_key_event(&f.payload) {
                        let (e, m) = wire_to_event(w);
                        if tx.send(SessionEvent::Key(e, m)).is_err() {
                            break; // main loop gone
                        }
                    }
                }
            }
            Ok(None) | Err(_) => break, // L2 closed the socket
        }
    });
    Some(writer)
}

/// Publish the grid and, if connected to L2, poke it so it re-reads the
/// shm — keeps L2 event-driven instead of polling per frame.
fn publish_and_poke(
    shm: &mut GridShmWriter,
    session: &marspot_term::shelld_client::ShelldSession,
    poke: Option<&mut UnixStream>,
) {
    publish(shm, session);
    if let Some(w) = poke {
        // Best-effort: a dead socket just means L2 went away; the next
        // read EOF tears the reader down and the session keeps running.
        let _ = Frame::new(MsgType::GridReady, Vec::new()).write_to(w);
    }
}

/// Bring up the grid framebuffer and report its geometry. When L2 owns
/// the region (`MARSPOT_SHM_FD` set) we take the writer role on the
/// inherited fd and adopt the region's dims; standalone we self-create
/// at the placeholder geometry. The returned `(cols, rows)` is what the
/// session must be sized to so its published grid fits the region.
fn setup_shm() -> (GridShmWriter, u16, u16) {
    match std::env::var(ENV_SHM_FD) {
        Ok(s) => {
            let fd: RawFd = s.parse().unwrap_or_else(|_| {
                eprintln!("[session] bad {ENV_SHM_FD}={s:?}");
                std::process::exit(1);
            });
            let owned = unsafe { OwnedFd::from_raw_fd(fd) };
            let w = GridShmWriter::from_fd(owned).unwrap_or_else(|e| {
                eprintln!("[session] grid_shm from_fd({fd}) failed: {e}");
                std::process::exit(1);
            });
            let (c, r) = (w.cols(), w.rows());
            eprintln!("[session] grid framebuffer from inherited fd {fd} ({c}x{r})");
            (w, c, r)
        }
        Err(_) => {
            let w = GridShmWriter::create(INITIAL_COLS, INITIAL_ROWS).unwrap_or_else(|e| {
                eprintln!("[session] grid_shm create failed: {e}");
                std::process::exit(1);
            });
            eprintln!(
                "[session] grid framebuffer self-created (shm fd {}) ({INITIAL_COLS}x{INITIAL_ROWS})",
                w.fd()
            );
            (w, INITIAL_COLS, INITIAL_ROWS)
        }
    }
}

fn main() {
    eprintln!(
        "marspot-session {} (git {}) pid={}",
        env!("CARGO_PKG_VERSION"),
        option_env!("MARSPOT_GIT_SHA").unwrap_or("unknown"),
        std::process::id()
    );

    // Event-driven wake: one channel carries both shelld byte/EOF wakes
    // and L2-forwarded keystrokes, so the main loop sleeps in a single
    // place until there's real work.
    let (ev_tx, ev_rx): (Sender<SessionEvent>, Receiver<SessionEvent>) = mpsc::channel();
    let wake_tx = ev_tx.clone();
    let wake = move || {
        let _ = wake_tx.send(SessionEvent::Wake);
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

    // Shared grid framebuffer first — it defines the geometry. When L2
    // owns the region it sized it to the on-screen cell rect; we must
    // drive the session at exactly those dims, since the grid we publish
    // has to fit the region (a mismatch would overflow the mapping).
    let (mut shm, cols, rows) = setup_shm();

    // Pick the session to drive: an explicit MARSPOT_SESSION_ID if it
    // names a live one, else the first live session (reattach + bytelog
    // replay), else a fresh session. Sized to the framebuffer.
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

    let session = match target {
        Some(id) => {
            eprintln!("[session] attaching existing session id={id}");
            client.attach(id, cols, rows)
        }
        None => {
            eprintln!("[session] no live session; creating a fresh one");
            client.new_session(cols, rows, "")
        }
    };
    let mut session = session.unwrap_or_else(|e| {
        eprintln!("[session] session setup failed: {e}");
        std::process::exit(1);
    });
    eprintln!(
        "[session] driving session id={} pid={} ({cols}x{rows})",
        session.id(),
        session.child_pid()
    );

    // Input source + L2 wake channel: L2 forwards keystrokes over the
    // control socket; we poke it back with GridReady after each publish.
    let mut poke = setup_control_socket(ev_tx.clone());
    publish_and_poke(&mut shm, &session, poke.as_mut());

    let start = Instant::now();
    let mut frame: u64 = 0;
    loop {
        let first = match ev_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(ev) => Some(ev),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("[session] event channel closed; exiting");
                break;
            }
        };
        // Drain the burst: handle every queued key now, coalesce wakes
        // into the single pump below.
        let mut predicted = false;
        for ev in first.into_iter().chain(std::iter::from_fn(|| ev_rx.try_recv().ok())) {
            if let SessionEvent::Key(e, m) = ev {
                predicted |= handle_key(&mut session, e, m);
            }
        }

        let n = session.pump();
        if session.is_exited() {
            session.pump();
            publish_and_poke(&mut shm, &session, poke.as_mut());
            eprintln!("[session] session exited; exiting cleanly");
            break;
        }
        // Republish on PTY output or on a local echo that painted ahead
        // of it.
        if n > 0 || predicted {
            publish_and_poke(&mut shm, &session, poke.as_mut());
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

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

mod local_session;
mod uds_server;

use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use marspot_term::{lx_debug, lx_error, lx_event, lx_info, lx_warn};
use marspot_term::grid_shm::{
    GridShmWriter, ENV_SHM_FD, FLAG_APP_CURSOR_KEYS, FLAG_BRACKETED_PASTE, FLAG_CURSOR_VISIBLE,
};
use marspot_term::input_core::{MarspotKeyEvent, Modifiers};
use marspot_term::render::grid_selection_text;
use marspot_term::shell_proto::{
    decode_get_selection_text, decode_grid_resize, decode_grid_scroll, decode_key_event,
    decode_paste,
    encode_selection_text, wire_to_event, Frame, MsgType,
    DEFAULT_CONTROL_FD, ENV_CONTROL_FD,
};
use marspot_term::session_state::{PendingPage, SessionState};

use local_session::LocalSession;

/// RFC-003 step 1c. Two session backends coexist:
///
/// * `Shelld` — the L4-mediated path. PTY + bytelog live in the
///   `marspot-shelld` daemon; this L3 talks to it over a unix socket
///   and consumes a snapshot/byte stream.
/// * `Local`  — the L3-owns-PTY path. PTY + bytelog live right here.
///   No L4 dependency.
///
/// Both expose the same surface to the main loop so the only
/// difference user-facing is the one env-var-gated dispatch in
/// `main()`. Phase 6 of RFC-003 deletes the `Shelld` variant once
/// L2 has fully switched over.
/// RFC-003 Phase 6: only the L3-owns-PTY path remains; SessionImpl
/// is a single-variant alias kept for code-shape continuity.
enum SessionImpl {
    Local(LocalSession),
}

impl SessionImpl {
    fn id(&self) -> u64 {
        match self {
            Self::Local(s) => s.id(),
        }
    }
    fn child_pid(&self) -> i32 {
        match self {
            Self::Local(s) => s.child_pid(),
        }
    }
    fn is_exited(&self) -> bool {
        match self {
            Self::Local(s) => s.is_exited(),
        }
    }
    fn state(&self) -> SessionState {
        match self {
            Self::Local(s) => s.state(),
        }
    }
    fn terminal(&self) -> &marspot_term::terminal::Terminal {
        match self {
            Self::Local(s) => s.terminal(),
        }
    }
    fn terminal_mut(&mut self) -> &mut marspot_term::terminal::Terminal {
        match self {
            Self::Local(s) => s.terminal_mut(),
        }
    }
    fn pump(&mut self) -> usize {
        match self {
            Self::Local(s) => s.pump(),
        }
    }
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Local(s) => s.write(bytes),
        }
    }
    fn resize(&mut self, cols: u16, rows: u16) -> std::io::Result<()> {
        match self {
            Self::Local(s) => s.resize(cols, rows),
        }
    }
    fn request_scrollback_page(
        &mut self,
        line_start: u32,
        line_count: u32,
    ) -> std::io::Result<()> {
        match self {
            Self::Local(s) => s.request_scrollback_page(line_start, line_count),
        }
    }
    fn take_pending_scrollback_pages(&mut self) -> Vec<PendingPage> {
        match self {
            Self::Local(s) => s.take_pending_scrollback_pages(),
        }
    }
}

/// What wakes the L3 main loop. Both arms arrive on one channel so the
/// loop blocks in a single place (idle CPU ~0): the shelld reader thread
/// sends `Wake` when the PTY has bytes/EOF, the control reader sends
/// `Key` when L2 forwards a keystroke.
enum SessionEvent {
    Wake,
    Key(MarspotKeyEvent, Modifiers),
    /// L2 forwarded a cell-grid resize (window/layout changed). L3 resizes
    /// its Terminal + ioctl's the PTY (via shelld) + reflows, then
    /// republishes at the new dims into the same (capacity-mapped) region.
    Resize(u16, u16),
    /// L2 forwarded a scrollback view offset (rows up from live). L3 owns
    /// the scrollback, so it publishes that window; L2 renders the mirror
    /// as-is (it can't scroll its window-only mirror itself).
    Scroll(u16),
    /// L2 wants the clipboard text under a selection (Cmd-C). L3 owns the
    /// grid + scrollback, so it serialises the text and replies with a
    /// `SelectionText` frame. `(seq, anchor, focus, blockwise)`; `seq` is
    /// echoed back so a late reply can't alias a newer request.
    GetSelection(u32, (u16, u32), (u16, u32), bool),
    /// L2 resolved the macOS pasteboard (Cmd-V) and forwarded the text —
    /// the GUI-free L3 can't read the pasteboard itself. We bracketed-wrap
    /// it (per our terminal's mode) and write it to the PTY.
    Paste(String),
    /// The control socket to L2/core hit EOF — our core is gone (crash,
    /// hang-kill, or clean exit). A freshly-booted core re-attaches the
    /// shelld session (PTY + bytelog survive *in shelld*, kept alive with
    /// zero subscribers) and spawns a brand-new L3 for it, so this
    /// orphaned engine has no reader left: it must exit, not linger.
    /// Lingering leaks one process + shm region per core restart, which
    /// violates the "cannot get slower the longer it runs" invariant.
    /// Standalone sessions have no control socket, so this never fires
    /// there.  RFC-003 §6 Amendment 11 debug-3: carries the client
    /// generation that owned the dying reader.  Main ignores a
    /// CoreGone whose generation is older than the current `poke` —
    /// otherwise an OLD L2's socket EOF, arriving AFTER a NEW L2 has
    /// already reattached, would null out the poke we just adopted.
    CoreGone(u64),
    /// RFC-003 step 2e: a UDS client finished its Hello handshake and
    /// is now the L3's control client.  Carries the validated stream
    /// so main can spawn a reader on it and adopt the write half as
    /// `poke`.  In owns-pty mode this replaces the inherited-fd
    /// control socket — owns-pty L3s have no fd 3 to read from.
    NewClient(UnixStream),
    /// RFC-003 §6 Amendment 15 — SIGTERM received.  L1 / L2 told us
    /// to retire (silent upgrade or user-driven marspot quit).  Main
    /// loop persists Terminal state to `state.bin`, then `exit(0)` —
    /// `process::exit` does NOT unwind, so `Pty::Drop` never runs and
    /// the shell is NOT SIGHUP'd.  L1's fd-vault still holds a duped
    /// PTY master fd (for the silent-upgrade replacement to withdraw),
    /// or the user-quit path will close that vault entry too, letting
    /// the shell exit on its own.
    ShutdownRequested,
}

/// RFC-003 §6 Amendment 15 — SIGTERM self-pipe write end.  -1 until
/// the handler is installed.
static SIGTERM_PIPE_W: AtomicI32 = AtomicI32::new(-1);

extern "C" fn l3_sigterm_handler(_: libc::c_int) {
    let fd = SIGTERM_PIPE_W.load(AtomicOrdering::Relaxed);
    if fd >= 0 {
        let buf: [u8; 1] = [1];
        unsafe { libc::write(fd, buf.as_ptr() as *const _, 1); }
    }
}

/// Install a SIGTERM handler that posts `SessionEvent::ShutdownRequested`
/// onto the main loop via a self-pipe.  Async-signal-safe write is the
/// only thing the handler does; the watcher thread translates each
/// wake byte into a regular SessionEvent so the loop processes it in
/// its normal drain.
fn install_sigterm_handler(ev_tx: Sender<SessionEvent>) -> std::io::Result<()> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let (r_fd, w_fd) = (fds[0], fds[1]);
    for fd in [r_fd, w_fd] {
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
    SIGTERM_PIPE_W.store(w_fd, AtomicOrdering::Relaxed);

    std::thread::Builder::new()
        .name("l3-sigterm-watcher".to_string())
        .spawn(move || {
            let mut buf = [0u8; 1];
            loop {
                let n = unsafe { libc::read(r_fd, buf.as_mut_ptr() as *mut _, 1) };
                if n <= 0 { break; }
                if ev_tx.send(SessionEvent::ShutdownRequested).is_err() {
                    break;
                }
            }
        })?;

    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = l3_sigterm_handler as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESTART;
        if libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// RFC-003 §6 Amendment 15 — disk path for the Terminal snapshot we
/// persist on graceful exit (used for the resurrect-on-boot path when
/// the L3 process is gone but the pane still has state to show).
fn session_state_bin_path(id: u64) -> std::path::PathBuf {
    marspot_term::session_registry::session_dir(id).join("state.bin")
}

/// RFC-003 §6 Amendment 15 — encode the metadata that travels alongside
/// the PTY master fd in L1's vault.  Format: line-based, key=value.
///   child_pid=<i32>
///   cols=<u16>
///   rows=<u16>
/// L1 doesn't know what these mean (it's opaque bytes from its POV);
/// only L3's encode/decode pair below interprets the payload.
fn build_resume_metadata(child_pid: i32, cols: u16, rows: u16) -> Vec<u8> {
    format!("child_pid={child_pid}\ncols={cols}\nrows={rows}\n").into_bytes()
}

fn parse_resume_metadata(buf: &[u8]) -> std::io::Result<(i32, u16, u16)> {
    let s = std::str::from_utf8(buf).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("metadata not utf8: {e}"))
    })?;
    let mut child_pid: Option<i32> = None;
    let mut cols: Option<u16> = None;
    let mut rows: Option<u16> = None;
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "child_pid" => child_pid = v.trim().parse().ok(),
                "cols" => cols = v.trim().parse().ok(),
                "rows" => rows = v.trim().parse().ok(),
                _ => {}
            }
        }
    }
    let invalid = |k: &str| -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("metadata missing/bad {k}"),
        )
    };
    Ok((
        child_pid.ok_or_else(|| invalid("child_pid"))?,
        cols.ok_or_else(|| invalid("cols"))?,
        rows.ok_or_else(|| invalid("rows"))?,
    ))
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

/// Publish the session's grid window at `view_offset` (rows up from the
/// live tail, clamped to the available scrollback) + cursor/mode flags
/// into the shared framebuffer for L2 to render.
fn publish(shm: &mut GridShmWriter, session: &SessionImpl, view_offset: u16) {
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
    // Clamp here too: L2 clamps against the scrollback_len it last saw, but
    // scrollback can shrink (alt-screen enter / reset) between L2's request
    // and this publish, so never hand the writer an out-of-range offset.
    let off = view_offset.min(term.grid().scrollback_len() as u16);
    shm.publish(term.grid(), off, flags);
}

/// Encode one forwarded keystroke with L3's own terminal mode flags,
/// write it to the PTY, and local-echo printable bytes ahead of the
/// round trip. Returns whether the echo painted the grid (so the caller
/// republishes even when no PTY bytes pumped this tick).
///
/// Clipboard reads resolve to `None` here: Cmd-V paste is forwarded by
/// L2 as already-resolved text in a later step, not pulled from the
/// pasteboard by the GUI-free L3.
fn handle_key(session: &mut SessionImpl, event: MarspotKeyEvent, mods: Modifiers) -> bool {
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
    // Local-echo predict — but ONLY for plain printable runs.  An
    // escape sequence (ESC-leading) is a control code; the first
    // byte 0x1b isn't printable so predict_byte rejects it, but the
    // sequence's tail (`[`, `C`, …) is ASCII printable and would be
    // painted literally into the grid, producing the `[C[C[C` smear
    // the user reported every time they pressed an arrow key.  Just
    // skip the whole run when it starts with ESC.
    let mut predicted = false;
    if !bytes.starts_with(&[0x1b]) {
        for &b in bytes.as_ref() {
            if session.terminal_mut().predict_byte(b) {
                predicted = true;
            }
        }
    }
    predicted
}

/// Write pasted text (already resolved from the macOS pasteboard by L2) to
/// the PTY.  Wrap in bracketed-paste markers iff this terminal turned the
/// mode on (DECSET 2004) so the receiving app treats it as a single paste
/// rather than typed input.  No local predict — the PTY echo round-trips
/// back through the normal pump.
fn handle_paste(session: &mut SessionImpl, text: &str) {
    let bracketed = session.terminal().bracketed_paste_mode();
    let mut bytes: Vec<u8> = Vec::with_capacity(text.len() + 12);
    if bracketed {
        bytes.extend_from_slice(b"\x1b[200~");
    }
    bytes.extend_from_slice(text.as_bytes());
    if bracketed {
        bytes.extend_from_slice(b"\x1b[201~");
    }
    let _ = session.write(&bytes);
}

/// If L2 handed us a control socket (fd `MARSPOT_SHELL_CONTROL_FD`,
/// default 3), spawn a reader thread that decodes forwarded `KeyEvent`
/// frames and feeds them to the main loop, and return a write half so
/// the loop can poke L2 with `GridReady` after each publish. EOF / error
/// on the read side means L2/core is gone, so we signal the main loop to
/// exit (`CoreGone`): the surviving shelld session is re-attached by the
/// next core, so an orphaned engine would only leak (see `CoreGone`).
/// No env var → standalone, no input source, no writer.
fn setup_control_socket(tx: Sender<SessionEvent>) -> Option<UnixStream> {
    let fd: RawFd = match std::env::var(ENV_CONTROL_FD) {
        Ok(s) => match s.parse() {
            Ok(fd) => fd,
            Err(_) => {
                lx_warn!(
                    "session.control_fd.parse_failed",
                    "bad control fd env; ignoring",
                    env_var = ENV_CONTROL_FD,
                    value = s
                );
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
            lx_error!(
                "session.control_socket.try_clone_failed",
                &format!("{e}"),
                fd = fd
            );
            return None;
        }
    };
    lx_info!("session.control_socket.attached", "control socket fd inherited", fd = fd);
    // The inherited-fd path runs at generation 0; if it dies, that's a
    // genuine "no client left" (not a stale-reader race).
    spawn_control_reader(stream, tx, 0);
    Some(writer)
}

/// RFC-003 step 2e — frame-dispatch loop for the L2↔L3 control socket.
/// Shared by both the inherited-fd path (`setup_control_socket`) and
/// the UDS accept path (`uds_server::handshake`) so both ends speak
/// the same protocol and unknown-frame handling lives in exactly one
/// place.  `generation` tags the CoreGone emitted on EOF so a stale
/// reader (from a swapped-out L2 socket) can't null the current poke.
fn spawn_control_reader(mut reader: UnixStream, tx: Sender<SessionEvent>, generation: u64) {
    std::thread::spawn(move || loop {
        match Frame::read_from(&mut reader) {
            Ok(Some(f)) => match f.msg_type {
                MsgType::KeyEvent => {
                    if let Ok(w) = decode_key_event(&f.payload) {
                        let (e, m) = wire_to_event(w);
                        if tx.send(SessionEvent::Key(e, m)).is_err() {
                            break;
                        }
                    }
                }
                MsgType::GridResize => {
                    if let Ok((cols, rows)) = decode_grid_resize(&f.payload) {
                        if tx.send(SessionEvent::Resize(cols, rows)).is_err() {
                            break;
                        }
                    }
                }
                MsgType::GridScroll => {
                    if let Ok(off) = decode_grid_scroll(&f.payload) {
                        if tx.send(SessionEvent::Scroll(off)).is_err() {
                            break;
                        }
                    }
                }
                MsgType::GetSelectionText => {
                    if let Ok((seq, a, fo, bw)) = decode_get_selection_text(&f.payload) {
                        if tx.send(SessionEvent::GetSelection(seq, a, fo, bw)).is_err() {
                            break;
                        }
                    }
                }
                MsgType::Paste => {
                    if let Ok(text) = decode_paste(&f.payload) {
                        if tx.send(SessionEvent::Paste(text)).is_err() {
                            break;
                        }
                    }
                }
                _ => {}
            },
            Ok(None) | Err(_) => {
                let _ = tx.send(SessionEvent::CoreGone(generation));
                break;
            }
        }
    });
}

/// Publish the grid and, if connected to L2, poke it so it re-reads the
/// shm — keeps L2 event-driven instead of polling per frame.
fn publish_and_poke(
    shm: &mut GridShmWriter,
    session: &SessionImpl,
    view_offset: u16,
    poke: Option<&mut UnixStream>,
) {
    publish(shm, session, view_offset);
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
                lx_error!(
                    "session.shm_fd.parse_failed",
                    "bad shm fd env",
                    env_var = ENV_SHM_FD,
                    value = s
                );
                std::process::exit(1);
            });
            let owned = unsafe { OwnedFd::from_raw_fd(fd) };
            let w = GridShmWriter::from_fd(owned).unwrap_or_else(|e| {
                lx_error!(
                    "session.grid_shm.from_fd_failed",
                    &format!("{e}"),
                    fd = fd
                );
                std::process::exit(1);
            });
            let (c, r) = (w.cols(), w.rows());
            lx_info!(
                "session.grid_shm.attached",
                "grid framebuffer from inherited fd",
                fd = fd,
                cols = c,
                rows = r
            );
            (w, c, r)
        }
        Err(_) => {
            let w = GridShmWriter::create(INITIAL_COLS, INITIAL_ROWS).unwrap_or_else(|e| {
                lx_error!("session.grid_shm.create_failed", &format!("{e}"));
                std::process::exit(1);
            });
            lx_info!(
                "session.grid_shm.self_created",
                "grid framebuffer self-created",
                fd = w.fd(),
                cols = INITIAL_COLS,
                rows = INITIAL_ROWS
            );
            (w, INITIAL_COLS, INITIAL_ROWS)
        }
    }
}

fn main() {
    marspot_term::logx::init("session");
    lx_event!(
        "SESSION_BOOT",
        "marspot-session starting",
        version = env!("MARSPOT_VERSION_SESSION"),
        marspot_version = env!("MARSPOT_VERSION_CORE"),
        git = option_env!("MARSPOT_GIT_SHA").unwrap_or("unknown"),
        pid = std::process::id()
    );

    // Event-driven wake: one channel carries both PTY byte/EOF wakes
    // (from either ShelldClient or LocalSession's reader thread) and
    // L2-forwarded keystrokes, so the main loop sleeps in a single
    // place until there's real work.
    let (ev_tx, ev_rx): (Sender<SessionEvent>, Receiver<SessionEvent>) = mpsc::channel();

    // RFC-003 step 1c: MARSPOT_L3_OWNS_PTY=1 picks the LocalSession
    // path. Default off; L4 shelld still owns PTY until L2 (Phase 3)
    // and the shelld delete (Phase 6) land.
    let owns_pty = std::env::var("MARSPOT_L3_OWNS_PTY").as_deref() == Ok("1");

    // Shared grid framebuffer first — it defines the geometry. When L2
    // owns the region it sized it to the on-screen cell rect; we must
    // drive the session at exactly those dims, since the grid we publish
    // has to fit the region (a mismatch would overflow the mapping).
    let (mut shm, cols, rows) = setup_shm();

    // Phase 2c: when we own the PTY, also own the UDS control socket
    // + registry entry so L2 (Phase 3) can discover us after a swap.
    // Bound for the whole owns-pty branch; Drop on exit unbinds +
    // prunes the on-disk entry.
    let mut _listener: Option<uds_server::SessionListener> = None;

    let mut session: SessionImpl = if owns_pty {
        let wake_tx = ev_tx.clone();
        let wake = move || {
            let _ = wake_tx.send(SessionEvent::Wake);
        };
        let id = match std::env::var("MARSPOT_SESSION_ID")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            Some(id) => id,
            None => marspot_term::session_registry::allocate_next_session_id()
                .unwrap_or_else(|e| {
                    lx_error!("session.local.allocate_id_failed", &format!("{e}"));
                    std::process::exit(1);
                }),
        };
        // RFC-003 §6 Amendment 15 — three boot modes:
        //   * Resume:   env `MARSPOT_L3_RESUME_KEY=<id>` set →
        //               withdraw PTY master fd + child_pid from L1
        //               vault, restore Terminal from state.bin, no
        //               forkpty.
        //   * Restore:  env `MARSPOT_L3_RESTORE_STATE_BIN=<path>` →
        //               cold spawn shell, but apply_snapshot the
        //               state.bin first so the L2 mirror paints the
        //               last-known frame from a previous L3 instance
        //               (resurrect after a marspot quit).
        //   * Cold:     plain fresh forkpty + shell.  After spawn,
        //               deposit the master fd into L1's vault.
        let resume_key: Option<u64> = std::env::var("MARSPOT_L3_RESUME_KEY")
            .ok()
            .and_then(|s| s.parse().ok());
        let restore_state_bin: Option<std::path::PathBuf> = std::env::var(
            "MARSPOT_L3_RESTORE_STATE_BIN",
        )
        .ok()
        .map(std::path::PathBuf::from);
        let vault_sock: Option<std::path::PathBuf> =
            std::env::var(marspot_term::fd_vault::ENV_VAULT_SOCK)
                .ok()
                .map(std::path::PathBuf::from);

        let local = if let Some(key) = resume_key {
            let sock = vault_sock.clone().unwrap_or_else(|| {
                lx_error!(
                    "session.resume.no_vault_sock",
                    "MARSPOT_L3_RESUME_KEY set but ENV_VAULT_SOCK missing"
                );
                std::process::exit(1);
            });
            lx_event!(
                "L3_RESUME_WITHDRAW",
                "withdrawing PTY master fd from L1 vault",
                session_id = id,
                key = key,
                sock = sock.display()
            );
            let (owned_fd, metadata) = marspot_term::fd_vault::withdraw_fd(&sock, key)
                .unwrap_or_else(|e| {
                    lx_error!("session.resume.withdraw_failed", &format!("{e}"), key = key);
                    std::process::exit(1);
                });
            let (child_pid, prev_cols, prev_rows) = parse_resume_metadata(&metadata)
                .unwrap_or_else(|e| {
                    lx_error!("session.resume.metadata_bad", &format!("{e}"));
                    std::process::exit(1);
                });
            // Build Terminal from state.bin (best-effort).  Geometry
            // comes from the metadata so a mid-resize swap doesn't
            // mis-shape the grid; PTY will be ioctl'd to match.
            let mut terminal = marspot_term::terminal::Terminal::new(prev_cols, prev_rows);
            let state_path = session_state_bin_path(id);
            match std::fs::read(&state_path) {
                Ok(body) => {
                    if let Err(e) = terminal.apply_snapshot(&body) {
                        lx_warn!(
                            "l3.resume.snapshot_apply_failed",
                            &format!("{e}"),
                            body_bytes = body.len()
                        );
                    } else {
                        lx_event!(
                            "L3_RESUME_SNAPSHOT_APPLIED",
                            "restored Terminal from state.bin",
                            body_bytes = body.len()
                        );
                    }
                    let _ = std::fs::remove_file(&state_path);
                }
                Err(_) => {
                    lx_warn!(
                        "l3.resume.snapshot_missing",
                        "no state.bin; resumed Terminal stays blank until next PTY burst"
                    );
                }
            }
            let master_fd: RawFd =
                <OwnedFd as std::os::fd::IntoRawFd>::into_raw_fd(owned_fd);
            LocalSession::from_handoff(
                id, master_fd, child_pid, prev_cols, prev_rows, terminal, wake,
            )
            .unwrap_or_else(|e| {
                lx_error!("l3.resume.from_handoff_failed", &format!("{e}"));
                std::process::exit(1);
            })
        } else {
            // Cold start (with optional restore-snapshot).
            lx_event!(
                "L3_OWNS_PTY",
                "spawning local PTY (no shelld)",
                session_id = id,
                cols = cols,
                rows = rows
            );
            let mut local = LocalSession::spawn(id, cols, rows, "", wake)
                .unwrap_or_else(|e| {
                    lx_error!("session.local.spawn_failed", &format!("{e}"));
                    std::process::exit(1);
                });
            // Resurrect path: paint the saved snapshot before the
            // fresh shell's prompt arrives (user sees prior content +
            // new prompt instead of a blank pane).  Best-effort.
            if let Some(path) = restore_state_bin {
                match std::fs::read(&path) {
                    Ok(body) => {
                        if let Err(e) = local.terminal_mut().apply_snapshot(&body) {
                            lx_warn!(
                                "l3.restore.snapshot_apply_failed",
                                &format!("{e}"),
                                body_bytes = body.len(),
                                state_path = path.display()
                            );
                        } else {
                            lx_event!(
                                "L3_RESURRECT_RESTORED",
                                "applied state.bin to fresh shell",
                                session_id = id,
                                body_bytes = body.len()
                            );
                        }
                        // One-shot: don't repaint stale snapshot on
                        // a future cold start.
                        let _ = std::fs::remove_file(&path);
                    }
                    Err(e) => {
                        lx_warn!(
                            "l3.restore.read_failed",
                            &format!("{e}"),
                            state_path = path.display()
                        );
                    }
                }
            }
            // Deposit master_fd into L1's vault so future upgrades /
            // resumes can withdraw it.  Best-effort: if the vault is
            // down, silent updates degrade but the session still works.
            if let Some(sock) = vault_sock.as_ref() {
                let master_fd = local.master_raw_fd();
                let child_pid = local.child_pid();
                let metadata = build_resume_metadata(child_pid, cols, rows);
                match marspot_term::fd_vault::deposit_fd(sock, id, &metadata, master_fd) {
                    Ok(()) => lx_event!(
                        "L3_VAULT_DEPOSIT",
                        "PTY master fd deposited to L1 vault",
                        session_id = id,
                        key = id,
                        sock = sock.display()
                    ),
                    Err(e) => lx_warn!(
                        "l3.vault.deposit_failed",
                        &format!("{e}; silent updates disabled for this session"),
                        sock = sock.display()
                    ),
                }
            } else {
                lx_warn!(
                    "l3.vault.no_sock",
                    "ENV_VAULT_SOCK not set; running in degraded (no-vault) mode"
                );
            }
            local
        };
        let cwd = std::env::var("HOME").unwrap_or_default();
        let shm_name = std::env::var("MARSPOT_SHM_NAME").unwrap_or_default();
        match uds_server::SessionListener::bind(
            id, cols, rows, &cwd, &shm_name, ev_tx.clone(),
        ) {
            Ok(l) => _listener = Some(l),
            Err(e) => {
                lx_error!("session.local.uds_bind_failed", &format!("{e}"));
                std::process::exit(1);
            }
        }
        SessionImpl::Local(local)
    } else {
        // RFC-003 Phase 6: L4 shelld retired.  MARSPOT_L3_OWNS_PTY=1
        // is now mandatory; the old shelld-driven fallback is gone.
        lx_error!(
            "session.shelld_path_removed",
            "MARSPOT_L3_OWNS_PTY=0 used to route through shelld; RFC-003 removed L4 — set MARSPOT_L3_OWNS_PTY=1 (or leave it unset and rely on the default)"
        );
        std::process::exit(1);
    };
    lx_event!(
        "SESSION_DRIVING",
        "driving session",
        session_id = session.id(),
        child_pid = session.child_pid(),
        cols = cols,
        rows = rows
    );

    // RFC-003 §6 Amendment 15 — install SIGTERM handler so L2 can
    // ask us to retire (silent-update path) or L1 can wipe us on
    // marspot quit.  The handler is just a self-pipe write; the main
    // loop turns the wake into a `ShutdownRequested` event and drives
    // the persist-state-then-exit path.  Best-effort: if install
    // fails the L3 still runs but silent updates degrade to "just
    // gets SIGKILL'd and loses state.bin".
    if let Err(e) = install_sigterm_handler(ev_tx.clone()) {
        lx_warn!(
            "l3.sigterm.install_failed",
            &format!("{e}; silent shutdown disabled for this L3")
        );
    } else {
        lx_event!(
            "L3_SIGTERM_INSTALLED",
            "SIGTERM handler armed for silent shutdown",
            pid = std::process::id()
        );
    }

    // Input source + L2 wake channel: L2 forwards keystrokes over the
    // control socket; we poke it back with GridReady after each publish.
    let mut poke = setup_control_socket(ev_tx.clone());
    // Scrollback view offset L2 last asked us to publish (0 = live tail).
    let mut view_offset: u16 = 0;
    publish_and_poke(&mut shm, &session, view_offset, poke.as_mut());

    // RFC-002 §4 (architectural correction over earlier step 6):
    // shelld (L4) owns the Terminal SoT now.  L3 no longer pushes
    // snapshots — the daemon parses every PTY byte itself, so on
    // every ATTACH it ships a freshly serialized state direct from
    // the master copy.  This eliminates the push throttle, the
    // generation-vector chase, and the "stale snapshot at the
    // server" failure mode entirely.

    // RFC-002 §8 (step 8b — fetch side): on attach, prefetch up to
    // HISTORY_PREFETCH_LINES rows of historic scrollback from L4 so
    // a user-initiated scroll-up has data to surface.  The reply is
    // drained from `take_pending_scrollback_pages` each publish tick
    // into `historic_pages`; integration into the publish window
    // (rendering above local scrollback) lands in step 8c.
    const HISTORY_PREFETCH_LINES: u32 = 1024;
    if let Err(e) = session.request_scrollback_page(0, HISTORY_PREFETCH_LINES) {
        lx_warn!(
            "session.scrollback.prefetch_send_failed",
            &format!("{e}"),
            session_id = session.id()
        );
    }
    let mut historic_pages: Vec<(Vec<marspot_term::grid::Cell>, bool)> = Vec::new();

    let start = Instant::now();
    let mut frame: u64 = 0;
    // RFC-003 §6 Amendment 11 (debug-3): generation tag for control
    // readers.  Increments on each NewClient adoption; CoreGone events
    // carry the generation of the reader that died, so a stale EOF
    // from a swapped-out L2 socket can't null the poke a newer L2
    // just adopted.
    let mut client_generation: u64 = 0;
    loop {
        let first = match ev_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(ev) => Some(ev),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                lx_event!("SESSION_EXIT", "event channel closed; exiting");
                break;
            }
        };
        // Drain the burst: handle every queued key now, coalesce wakes
        // into the single pump below, and collapse a flurry of resizes to
        // the final dims (intermediate sizes never need a reflow).
        let mut predicted = false;
        let mut pending_resize: Option<(u16, u16)> = None;
        let mut pending_scroll: Option<u16> = None;
        let mut selection_reqs: Vec<(u32, (u16, u32), (u16, u32), bool)> = Vec::new();
        let mut core_gone = false;
        for ev in first.into_iter().chain(std::iter::from_fn(|| ev_rx.try_recv().ok())) {
            match ev {
                SessionEvent::Key(e, m) => predicted |= handle_key(&mut session, e, m),
                SessionEvent::Resize(cols, rows) => pending_resize = Some((cols, rows)),
                SessionEvent::Scroll(off) => pending_scroll = Some(off),
                // Each request gets its own reply (don't coalesce — L2 is
                // blocking on a reply per request).
                SessionEvent::GetSelection(seq, a, f, bw) => selection_reqs.push((seq, a, f, bw)),
                // Paste writes straight to the PTY; the echo round-trips
                // back through the normal pump → republish (no local
                // predict — bulk text isn't latency-sensitive like typing).
                SessionEvent::Paste(text) => handle_paste(&mut session, &text),
                SessionEvent::Wake => {}
                SessionEvent::CoreGone(gen) => {
                    // RFC-003 §6 Amendment 11 (debug-2 + debug-3 root
                    // cause): owns-pty L3 MUST NOT exit on control
                    // EOF, AND the EOF that ends an OLD client must
                    // not null out the poke a NEW client already
                    // adopted.
                    //
                    // Sequence during a dual-core swap:
                    //   T0: OLD L2 connected, control_reader@gen=K
                    //   T1: NEW L2 reattaches; NewClient fires →
                    //       client_generation=K+1, poke=NEW
                    //   T2: OLD L2 dies → OLD reader EOFs → fires
                    //       CoreGone(K)
                    //   T3: main sees CoreGone(K) — STALE; ignore.
                    //
                    // Without the gen tag, T3 would null poke and
                    // leave L3 disconnected from the live NEW L2.
                    if gen < client_generation {
                        lx_event!(
                            "L3_UDS_STALE_EOF",
                            "ignored stale control EOF; current client newer",
                            stale_gen = gen,
                            current_gen = client_generation
                        );
                    } else if owns_pty {
                        poke = None;
                        lx_event!(
                            "L3_UDS_CLIENT_GONE",
                            "control client closed; awaiting reattach (owns-pty)",
                            gen = gen
                        );
                    } else {
                        core_gone = true;
                    }
                }
                SessionEvent::ShutdownRequested => {
                    // RFC-003 §6 Amendment 15 — persist a final
                    // Terminal snapshot to state.bin (for the
                    // resurrect-on-boot path) and exit WITHOUT
                    // running Drops.  Skipping unwind means
                    // `Pty::Drop` never runs → no SIGHUP for the
                    // shell.  L1's fd-vault still holds a duped
                    // master fd, so the kernel object stays alive
                    // until the replacement L3 withdraws it (silent
                    // upgrade) or L1's close path drains the vault
                    // (user-driven quit).
                    let id = session.id();
                    let _ = session.pump();
                    let body = session.terminal().serialize_snapshot();
                    let path = session_state_bin_path(id);
                    match std::fs::write(&path, &body) {
                        Ok(()) => lx_event!(
                            "L3_SHUTDOWN_SNAPSHOT_PERSISTED",
                            "state.bin written for handoff",
                            session_id = id,
                            body_bytes = body.len(),
                            path = path.display()
                        ),
                        Err(e) => lx_warn!(
                            "l3.shutdown.snapshot_persist_failed",
                            &format!("{e}"),
                            session_id = id,
                            path = path.display()
                        ),
                    }
                    // One last publish so a viewer attached to shm
                    // sees the final frame even before withdrawing
                    // the fd.
                    publish_and_poke(&mut shm, &session, view_offset, poke.as_mut());
                    lx_event!(
                        "L3_SHUTDOWN_EXIT",
                        "exiting on SIGTERM (no Drop, no SIGHUP)",
                        session_id = id,
                        pid = std::process::id()
                    );
                    // process::exit does NOT unwind, so Drops do not
                    // fire.  In particular Pty::Drop's SIGHUP path is
                    // skipped, which is the whole point.
                    std::process::exit(0);
                }
                SessionEvent::NewClient(stream) => {
                    // Adopt the validated stream as the control
                    // socket. spawn_control_reader fires inbound
                    // frames as SessionEvent::Key/Resize/etc just
                    // like the inherited-fd path.  Bump the
                    // generation BEFORE spawning so any in-flight
                    // CoreGone from the prior reader is recognised
                    // as stale by the time it reaches the loop.
                    client_generation += 1;
                    let this_gen = client_generation;
                    match stream.try_clone() {
                        Ok(writer) => {
                            poke = Some(writer);
                            spawn_control_reader(stream, ev_tx.clone(), this_gen);
                            // RFC-003 §6 Amendment 7: republish the
                            // current grid + fire GridReady so L2 has
                            // a poke to read the shm we already
                            // wrote.  Without this the first frame
                            // L2 sees is whatever the next PTY burst
                            // triggers — until then the pane renders
                            // empty (looks black).
                            publish_and_poke(
                                &mut shm,
                                &session,
                                view_offset,
                                poke.as_mut(),
                            );
                            lx_event!(
                                "L3_UDS_CLIENT_ADOPTED",
                                "control reader spawned on UDS stream"
                            );
                        }
                        Err(e) => {
                            lx_warn!(
                                "session.uds.try_clone_failed",
                                &format!("{e}")
                            );
                        }
                    }
                }
            }
        }
        // Our core vanished: the shelld session lives on (re-attached by
        // the next core), so exit rather than linger as an orphan.
        if core_gone {
            lx_event!("CORE_GONE", "L2/core gone (control EOF); exiting cleanly");
            break;
        }
        let resized = pending_resize.is_some();
        if let Some((cols, rows)) = pending_resize {
            // shelld ioctl's the PTY (SIGWINCH to the child) and the local
            // Terminal reflows; the next publish carries the new dims, and
            // L2's reader picks them up from the header with no remap.
            if let Err(e) = session.resize(cols, rows) {
                lx_warn!(
                    "session.resize_failed",
                    &format!("{e}"),
                    cols = cols,
                    rows = rows
                );
            }
        }
        // A scroll changes which window we publish even with no new output.
        let scrolled = match pending_scroll {
            Some(off) if off != view_offset => {
                view_offset = off;
                true
            }
            _ => false,
        };

        let n = session.pump();
        if session.is_exited() {
            session.pump();
            publish_and_poke(&mut shm, &session, view_offset, poke.as_mut());
            lx_event!("SESSION_EXITED", "shelld session exited; exiting cleanly");
            break;
        }
        // Republish on PTY output, a local echo that painted ahead of it,
        // a resize (grid shape changed), or a scroll (window changed) —
        // even when no bytes pumped this tick.
        if n > 0 || predicted || resized || scrolled {
            publish_and_poke(&mut shm, &session, view_offset, poke.as_mut());
        }

        // RFC-002 §8 (step 8b — fetch side): drain any ScrollbackPage
        // replies L4 sent.  Empty `line_count` means "no more history
        // past this point" — record the wall and stop asking.  Cells
        // accumulate in `historic_pages` (oldest-first per page, in
        // arrival order across pages) for the publish-side read-through
        // landing in step 8c.
        for page in session.take_pending_scrollback_pages() {
            match marspot_term::terminal::Terminal::decode_scrollback_page_body(
                page.line_count,
                &page.body,
            ) {
                Ok(mut lines) => {
                    let received = lines.len();
                    historic_pages.append(&mut lines);
                    lx_debug!(
                        "session.scrollback.page_received",
                        "RFC-002 GetScrollbackPage reply ingested",
                        line_start = page.line_start,
                        line_count = page.line_count,
                        received = received,
                        cached_total = historic_pages.len()
                    );
                }
                Err(e) => {
                    lx_warn!(
                        "session.scrollback.page_decode_failed",
                        &format!("{e}"),
                        line_start = page.line_start,
                        line_count = page.line_count,
                        body_bytes = page.body.len()
                    );
                }
            }
        }

        // Answer any Cmd-C selection requests against the post-pump grid.
        if !selection_reqs.is_empty() {
            if let Some(w) = poke.as_mut() {
                for (seq, anchor, focus, blockwise) in selection_reqs {
                    let text = grid_selection_text(session.terminal().grid(), anchor, focus, blockwise)
                        .unwrap_or_default();
                    let frame = Frame::new(MsgType::SelectionText, encode_selection_text(seq, &text));
                    if let Err(e) = frame.write_to(w) {
                        lx_warn!("session.selection_reply_failed", &format!("{e}"));
                        break;
                    }
                }
            }
        }

        frame += 1;
        // Heartbeat is for proof-of-life on idle sessions, not for
        // tracking byte activity — emitting on `n > 0` floods the
        // shared marspot.log at PTY-chunk rate (thousands per second
        // when a session is producing output), and the resulting
        // SINK mutex pressure across all live L3 processes was the
        // root cause of new-core HELLO timeouts during dual-core
        // swaps. Keep heartbeat purely time-based + lx_debug so it
        // only surfaces with MARSPOT_LOG_SESSION=debug.
        if frame.is_multiple_of(12) {
            let g = session.terminal().grid();
            let (cc, cr) = g.cursor();
            lx_debug!(
                "session.heartbeat",
                "pump tick",
                t_s = format!("{:.1}", start.elapsed().as_secs_f64()),
                pumped = n,
                cols = g.cols(),
                rows = g.rows(),
                cursor_col = cc,
                cursor_row = cr,
                state = state_str(session.state())
            );
        }
    }
}

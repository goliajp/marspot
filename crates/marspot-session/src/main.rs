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
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use marspot_term::{lx_debug, lx_error, lx_event, lx_info, lx_warn};
use marspot_term::grid_shm::{
    GridShmWriter, ENV_SHM_FD, FLAG_APP_CURSOR_KEYS, FLAG_BRACKETED_PASTE, FLAG_CURSOR_VISIBLE,
};
use marspot_term::input_core::{MarspotKeyEvent, Modifiers};
use marspot_term::paths::shelld_socket;
use marspot_term::render::grid_selection_text;
use marspot_term::shell_proto::{
    decode_get_selection_text, decode_grid_resize, decode_grid_scroll, decode_key_event,
    decode_paste,
    encode_selection_text, wire_to_event, Frame, MsgType, DEFAULT_CONTROL_FD, ENV_CONTROL_FD,
};
use marspot_term::shelld_client::{PendingPage, SessionState, ShelldClient, ShelldSession};
use marspot_term::shelld_proto::SessionInfo;

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
enum SessionImpl {
    Shelld(ShelldSession),
    Local(LocalSession),
}

impl SessionImpl {
    fn id(&self) -> u64 {
        match self {
            Self::Shelld(s) => s.id(),
            Self::Local(s) => s.id(),
        }
    }

    fn child_pid(&self) -> i32 {
        match self {
            Self::Shelld(s) => s.child_pid(),
            Self::Local(s) => s.child_pid(),
        }
    }

    fn is_exited(&self) -> bool {
        match self {
            Self::Shelld(s) => s.is_exited(),
            Self::Local(s) => s.is_exited(),
        }
    }

    fn state(&self) -> SessionState {
        match self {
            Self::Shelld(s) => s.state(),
            Self::Local(s) => s.state(),
        }
    }

    fn terminal(&self) -> &marspot_term::terminal::Terminal {
        match self {
            Self::Shelld(s) => s.terminal(),
            Self::Local(s) => s.terminal(),
        }
    }

    fn terminal_mut(&mut self) -> &mut marspot_term::terminal::Terminal {
        match self {
            Self::Shelld(s) => s.terminal_mut(),
            Self::Local(s) => s.terminal_mut(),
        }
    }

    fn pump(&mut self) -> usize {
        match self {
            Self::Shelld(s) => s.pump(),
            Self::Local(s) => s.pump(),
        }
    }

    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Shelld(s) => s.write(bytes),
            Self::Local(s) => s.write(bytes),
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) -> std::io::Result<()> {
        match self {
            Self::Shelld(s) => s.resize(cols, rows),
            Self::Local(s) => s.resize(cols, rows),
        }
    }

    fn request_scrollback_page(
        &mut self,
        line_start: u32,
        line_count: u32,
    ) -> std::io::Result<()> {
        match self {
            Self::Shelld(s) => s.request_scrollback_page(line_start, line_count),
            Self::Local(s) => s.request_scrollback_page(line_start, line_count),
        }
    }

    fn take_pending_scrollback_pages(&mut self) -> Vec<PendingPage> {
        match self {
            Self::Shelld(s) => s.take_pending_scrollback_pages(),
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
    /// there.
    CoreGone,
    /// RFC-003 step 2e: a UDS client finished its Hello handshake and
    /// is now the L3's control client.  Carries the validated stream
    /// so main can spawn a reader on it and adopt the write half as
    /// `poke`.  In owns-pty mode this replaces the inherited-fd
    /// control socket — owns-pty L3s have no fd 3 to read from.
    NewClient(UnixStream),
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
    spawn_control_reader(stream, tx);
    Some(writer)
}

/// RFC-003 step 2e — frame-dispatch loop for the L2↔L3 control socket.
/// Shared by both the inherited-fd path (`setup_control_socket`) and
/// the UDS accept path (`uds_server::handshake`) so both ends speak
/// the same protocol and unknown-frame handling lives in exactly one
/// place.
fn spawn_control_reader(mut reader: UnixStream, tx: Sender<SessionEvent>) {
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
                let _ = tx.send(SessionEvent::CoreGone);
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
        // L3-owns-PTY: spawn user shell directly. Session id is the
        // L2-assigned id (MARSPOT_SESSION_ID) when present; otherwise
        // fall back to .next_id allocation so a standalone dev run
        // still works without a coordinator.
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
        lx_event!(
            "L3_OWNS_PTY",
            "spawning local PTY (no shelld)",
            session_id = id,
            cols = cols,
            rows = rows
        );
        let local = LocalSession::spawn(id, cols, rows, "", wake).unwrap_or_else(|e| {
            lx_error!("session.local.spawn_failed", &format!("{e}"));
            std::process::exit(1);
        });
        // Bind UDS + write registry entry. Failure here drops the
        // session — without a registry handle L2 can't reach us, so
        // there's no point continuing.
        let cwd = std::env::var("HOME").unwrap_or_default();
        match uds_server::SessionListener::bind(id, cols, rows, &cwd, ev_tx.clone()) {
            Ok(l) => _listener = Some(l),
            Err(e) => {
                lx_error!("session.local.uds_bind_failed", &format!("{e}"));
                std::process::exit(1);
            }
        }
        SessionImpl::Local(local)
    } else {
        let wake_tx = ev_tx.clone();
        let wake = move || {
            let _ = wake_tx.send(SessionEvent::Wake);
        };
        let sock = shelld_socket();
        lx_info!(
            "session.shelld.connecting",
            "connecting to shelld",
            socket = sock.display()
        );
        let client = match ShelldClient::connect(&sock, wake) {
            Ok(c) => c,
            Err(e) => {
                lx_error!("session.shelld.connect_failed", &format!("{e}"));
                std::process::exit(1);
            }
        };

        // Pick the session to drive. Two regimes:
        //
        // * L2-managed (`MARSPOT_SESSION_ID` set): L2 owns assignment.
        // * Standalone (dev/test/soak, no id): reattach the first live
        //   session (bytelog replay) if any, else create a fresh one.
        let want: Option<u64> = std::env::var("MARSPOT_SESSION_ID")
            .ok()
            .and_then(|s| s.parse().ok());
        let attached = match want {
            Some(id) => {
                lx_info!(
                    "session.attach.assigned",
                    "attaching L2-assigned session",
                    session_id = id
                );
                client.attach(id, cols, rows)
            }
            None => {
                let existing: Vec<SessionInfo> = client
                    .list_sessions()
                    .unwrap_or_else(|e| {
                        lx_warn!(
                            "session.list_sessions_failed",
                            &format!("{e} — starting fresh")
                        );
                        Vec::new()
                    })
                    .into_iter()
                    .filter(|s| s.alive)
                    .collect();
                match existing.first() {
                    Some(s) => {
                        lx_info!(
                            "session.attach.standalone_existing",
                            "standalone: attaching first live session",
                            session_id = s.session_id
                        );
                        client.attach(s.session_id, cols, rows)
                    }
                    None => {
                        lx_info!(
                            "session.attach.standalone_fresh",
                            "standalone: no live session; creating fresh"
                        );
                        client.new_session(cols, rows, "")
                    }
                }
            }
        };
        SessionImpl::Shelld(attached.unwrap_or_else(|e| {
            lx_error!("session.setup_failed", &format!("{e}"));
            std::process::exit(1);
        }))
    };
    lx_event!(
        "SESSION_DRIVING",
        "driving session",
        session_id = session.id(),
        child_pid = session.child_pid(),
        cols = cols,
        rows = rows
    );

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
                SessionEvent::CoreGone => {
                    // owns-pty L3 should NOT exit when its client
                    // disconnects: the PTY + shell live here in our
                    // process, so dropping us would kill the shell.
                    // Just drop the writer and wait for a fresh
                    // NewClient (typical case: L2 silent update).
                    if owns_pty {
                        poke = None;
                        lx_event!(
                            "L3_UDS_CLIENT_GONE",
                            "control client closed; awaiting reattach"
                        );
                    } else {
                        core_gone = true;
                    }
                }
                SessionEvent::NewClient(stream) => {
                    // Adopt the validated stream as the control
                    // socket. spawn_control_reader fires inbound
                    // frames as SessionEvent::Key/Resize/etc just
                    // like the inherited-fd path.
                    match stream.try_clone() {
                        Ok(writer) => {
                            poke = Some(writer);
                            spawn_control_reader(stream, ev_tx.clone());
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

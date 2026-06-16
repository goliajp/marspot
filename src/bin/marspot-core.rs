//! marspot-core — the renderer + input-dispatch half of the
//! silent-update split.
//!
//! Spawned by `marspot-shell` as a child; renders the full marspot
//! multi-pane UI into the shared IOSurface the shell created,
//! attaches to shelld sessions, and processes input forwarded over
//! the control socket.
//!
//! Feature parity with the standalone `marspot` binary's non-tmux
//! mode: 9-grid (all `LayoutMode`s via the [layout] picker), sidebar
//! with focus rows / close [×] / add [+], click-to-focus, per-pane
//! scrollback scrolling, drag selection (linewise + Option-blockwise)
//! with Cmd-C copy, cell-title editing, IME preedit rendering, and
//! Cmd-B sidebar toggle.  The shared state shapes and pure logic
//! live in `marspot::ui`; this file owns the socket-event plumbing
//! the way src/main.rs owns the AppKit plumbing.
//!
//! Out of scope (standalone-only): tmux -CC mode, latency / RSS
//! profiling instrumentation, --snapshot / --bench.

use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use objc2::runtime::ProtocolObject;
use objc2_metal::MTLTexture;

use marspot::grid_shm::{self, ENV_SHM_FD, GridShmReader};
use marspot::input::{key_event_to_bytes, MarspotKeyEvent, Modifiers};
use marspot::iosurface::IOSurface;
use marspot::layout::Layout;
use marspot::pane::{L3Conn, L3Spawn, Pane};
use marspot::render::{SessionView, SidebarEntry};
use marspot::render_metal::MetalRenderer;
use marspot::session::SessionState;
use marspot::shell_proto::{
    decode_focus, decode_hello, decode_key_event, decode_mouse, decode_ping, decode_preedit,
    decode_resize, decode_scroll, decode_selection_text, decode_surface_attach,
    encode_caret_rect, encode_hello_ack, encode_pong, encode_surface_ready, mods_to_struct,
    wire_to_event, Frame, MsgType, DEFAULT_CONTROL_FD, ENV_CONTROL_FD, ENV_SURFACE_HEIGHT,
    ENV_SURFACE_ID, ENV_SURFACE_ID_BACK, ENV_SURFACE_SCALE, ENV_SURFACE_WIDTH, PROTO_VERSION,
};
use marspot::shelld_client::ShelldClient;
use marspot::{lx_debug, lx_debug_sampled, lx_error, lx_event, lx_info, lx_warn};
use marspot::ui::{
    scroll_lines, selection_text, selection_view_for_pane, truncate_for_sidebar, LayoutMode,
    Selection, SelectionMode, CELL_TITLE_PT, MAX_SIDEBAR_LABEL_CHARS, PICKER_LAYOUTS,
    SESSION_COUNT_HARD_CAP, SIDEBAR_W_LOGICAL,
};
use marspot::HEADER_PT;

/// Initial dimensions for sessions created before the layout has
/// sized them (mirrors src/main.rs; the post-spawn rebuild resizes
/// to the real cell rect immediately).
const INITIAL_COLS: u16 = 40;
const INITIAL_ROWS: u16 = 12;

fn env_required<T: std::str::FromStr>(name: &str) -> T {
    let raw = std::env::var(name)
        .unwrap_or_else(|_| panic!("[core] missing required env var {name}"));
    raw.parse::<T>()
        .ok()
        .unwrap_or_else(|| panic!("[core] env {name} = {raw:?} failed to parse"))
}

/// Input event the reader thread converts each control-socket frame
/// into.  The main loop drains a channel of these once per render
/// frame and dispatches them into `CoreApp`.
#[derive(Debug)]
enum CoreEvent {
    Key(MarspotKeyEvent, Modifiers),
    MouseDown(f64, f64, Modifiers),
    MouseDrag(f64, f64),
    /// Coordinates are on the wire but unused — release only ends
    /// the drag (same as src/main.rs `mouse_up`).
    MouseUp,
    /// `(dy_phys, precise)`; the horizontal delta is dropped at
    /// decode (terminal scrollback is vertical-only).
    Scroll(f64, bool),
    Focus(bool),
    /// PROTO_VERSION=1 single-surface resize.  Kept for tolerance; the
    /// PROTO_VERSION=2 path uses `SurfaceAttach` (dual-buffer).
    Resize(u32, f64, f64, f64),
    /// PROTO_VERSION=2 dual-buffer pair handshake: `(front_id, back_id,
    /// w_phys, h_phys, scale)`.  Either announces a fresh pair (resize
    /// / restart / pending-update spawn) or re-confirms the live pair
    /// at the same dims after a restart.
    SurfaceAttach(u32, u32, f64, f64, f64),
    Preedit(String),
    /// Shelld wake — some pane has new bytes to pump (PTY → bytelog
    /// → broadcast).  Sent by the shelld client's wake callback so
    /// the main loop is event-driven instead of polling.
    PumpShelld,
    /// Shell sent HELLO with its protocol version.  We reply with
    /// HELLO_ACK echoing the version we agree on.
    Hello(u32),
    /// Shell sent a liveness probe.  We echo the nonce back via PONG.
    Ping(u32),
    /// Shell closed the control socket — supervisor will tear us down.
    Closed,
    /// An L3 session process (`MARSPOT_L3`) published a new grid into
    /// shared memory.  A pure wake: it unblocks the loop so `pump_all`
    /// re-reads the shm mirror promptly instead of waiting on the 1 s
    /// heartbeat.  Carries no data (the mirror is read from shm).
    L3Ready,
    /// SIGUSR2 (per-session silent-update trigger): bring up replacement
    /// L3s on every idle pane's session and swap when they're ready.  The
    /// manual hook the updater will drive once a new `marspot-session` is
    /// staged (mirrors the shell's SIGUSR1 manual update trigger).
    SwapIdleL3,
    /// Shell → core: decorate the pane backing the given shelld session
    /// with this right-side badge in its title strip.  Empty text clears
    /// the badge.  Originates from L1 plugins (e.g. claudecode), routed
    /// shell → control socket → here.
    PaneBadge(u64, String),
    /// Shell → core: a plugin took over the pane backing this shelld
    /// session.  Capability bits say what L2 should change while the
    /// session is active (lock keys, freeze grid, accept overlays).
    PaneSessionBegin(u64, u32),
    /// Shell → core: plugin released the pane back to live mode.
    PaneSessionEnd(u64),
}

fn decode_frame(f: &Frame) -> Option<CoreEvent> {
    match f.msg_type {
        MsgType::KeyEvent => decode_key_event(&f.payload).ok().map(|w| {
            let (e, m) = wire_to_event(w);
            CoreEvent::Key(e, m)
        }),
        MsgType::MouseDown => decode_mouse(&f.payload)
            .ok()
            .map(|(x, y, m)| CoreEvent::MouseDown(x, y, mods_to_struct(m))),
        MsgType::MouseDrag => decode_mouse(&f.payload)
            .ok()
            .map(|(x, y, _)| CoreEvent::MouseDrag(x, y)),
        MsgType::MouseUp => decode_mouse(&f.payload).ok().map(|_| CoreEvent::MouseUp),
        MsgType::Scroll => decode_scroll(&f.payload)
            .ok()
            .map(|(_dx, dy, p)| CoreEvent::Scroll(dy, p)),
        MsgType::Focus => decode_focus(&f.payload).ok().map(CoreEvent::Focus),
        MsgType::Resize => decode_resize(&f.payload)
            .ok()
            .map(|(id, w, h, s)| CoreEvent::Resize(id, w, h, s)),
        MsgType::SurfaceAttach => decode_surface_attach(&f.payload)
            .ok()
            .map(|(f_id, b_id, w, h, s)| CoreEvent::SurfaceAttach(f_id, b_id, w, h, s)),
        MsgType::PaneBadge => marspot::shell_proto::decode_pane_badge(&f.payload)
            .ok()
            .map(|(sid, text)| CoreEvent::PaneBadge(sid, text)),
        MsgType::PaneSessionBegin => marspot::shell_proto::decode_pane_session_begin(&f.payload)
            .ok()
            .map(|(sid, caps)| CoreEvent::PaneSessionBegin(sid, caps)),
        MsgType::PaneSessionEnd => marspot::shell_proto::decode_pane_session_end(&f.payload)
            .ok()
            .map(CoreEvent::PaneSessionEnd),
        MsgType::Preedit => decode_preedit(&f.payload).ok().map(CoreEvent::Preedit),
        MsgType::Hello => decode_hello(&f.payload).ok().map(CoreEvent::Hello),
        MsgType::Ping => decode_ping(&f.payload).ok().map(CoreEvent::Ping),
        _ => None,
    }
}

/// Background reader: reads framed messages off the control socket
/// until EOF / error, dispatches each into `tx`.
fn reader_loop(mut stream: UnixStream, tx: Sender<CoreEvent>) {
    loop {
        match Frame::read_from(&mut stream) {
            Ok(None) => {
                let _ = tx.send(CoreEvent::Closed);
                return;
            }
            Ok(Some(frame)) => {
                if let Some(ev) = decode_frame(&frame) {
                    if tx.send(ev).is_err() {
                        return;
                    }
                }
            }
            Err(e) => {
                lx_error!("core.control.read_failed", &format!("{e}"));
                let _ = tx.send(CoreEvent::Closed);
                return;
            }
        }
    }
}

/// Background reader on the L2↔L3 control socket: an L3 session pokes
/// `GridReady` whenever it republishes its grid; we turn each into an
/// `L3Ready` wake so the main loop re-reads the shm mirror promptly.
/// EOF / error just ends the thread — the pane's own `try_wait` detects
/// the child's exit.
fn l3_reader_loop(
    mut stream: UnixStream,
    tx: Sender<CoreEvent>,
    selection_tx: Sender<(u32, String)>,
) {
    loop {
        match Frame::read_from(&mut stream) {
            Ok(Some(f)) => match f.msg_type {
                MsgType::GridReady => {
                    if tx.send(CoreEvent::L3Ready).is_err() {
                        return;
                    }
                }
                // Reply to a Cmd-C `GetSelectionText`: hand it to whoever is
                // blocked in `L3Conn::request_selection_text`.  A dropped
                // receiver (request already timed out) is fine — ignore.
                MsgType::SelectionText => {
                    // `(seq, text)` — the seq lets the waiter discard a late
                    // reply from an earlier, timed-out request.
                    if let Ok(reply) = decode_selection_text(&f.payload) {
                        let _ = selection_tx.send(reply);
                    }
                }
                _ => {}
            },
            Ok(None) | Err(_) => return,
        }
    }
}

/// Write end of the SIGUSR2 self-pipe.  The signal handler writes one byte
/// here (the only async-signal-safe way to hand a signal to the event
/// loop); a reader thread turns each byte into a `SwapIdleL3` event.
static SIGUSR2_PIPE_W: AtomicI32 = AtomicI32::new(-1);

extern "C" fn sigusr2_handler(_: libc::c_int) {
    let fd = SIGUSR2_PIPE_W.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = [1u8];
        // libc::write is async-signal-safe; ignore the result (best-effort).
        unsafe { libc::write(fd, byte.as_ptr() as *const libc::c_void, 1) };
    }
}

/// Install a SIGUSR2 handler that posts `CoreEvent::SwapIdleL3` to the main
/// loop via a self-pipe — the per-session silent-update trigger (the
/// updater will `kill -USR2` core once a new `marspot-session` is staged;
/// manually exercisable meanwhile, like the shell's SIGUSR1).
fn install_swap_trigger(tx: Sender<CoreEvent>) {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        lx_error!(
            "core.sigusr2.self_pipe_failed",
            &format!("{}", std::io::Error::last_os_error())
        );
        return;
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    SIGUSR2_PIPE_W.store(write_fd, Ordering::Relaxed);
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = sigusr2_handler as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGUSR2, &sa, std::ptr::null_mut());
    }
    std::thread::spawn(move || {
        let mut buf = [0u8; 64];
        loop {
            let n = unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 || tx.send(CoreEvent::SwapIdleL3).is_err() {
                return;
            }
        }
    });
}

/// Spawn one per-session L3 process and return its assembled pieces.  L2
/// owns the shm region's lifecycle: it creates + sizes the region, inherits
/// the fd (4) and the control socket (3) into the child, maps the region as
/// a reader, and starts a thread turning L3's `GridReady` pokes into
/// `L3Ready` wakes (and routing `SelectionText` replies).  Behind
/// `MARSPOT_L3=1`.  Used both at boot ([`spawn_l3_pane`]) and to bring up a
/// silent-update replacement on the same session ([`CoreApp::swap_idle_l3`]).
fn spawn_l3(
    cols: u16,
    rows: u16,
    session_id: u64,
    event_tx: &Sender<CoreEvent>,
) -> std::io::Result<L3Spawn> {
    // L2 creates + stamps the region; both ends map the same fd.
    let region = grid_shm::create_region(cols, rows)?;
    let region_raw = region.as_raw_fd();

    // Bidirectional control socket; the child inherits one end as fd 3.
    let mut sp = [0i32; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sp.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let parent_fd: RawFd = sp[0];
    let child_fd: RawFd = sp[1];

    // CLOEXEC every fd we hold here so a *sibling* L3 spawned later (and
    // this L3 itself, except via the dup'd well-known fds) never inherits
    // the core end of a control socket or the shm region.  Without this,
    // each L3 holds the core end of its own (and prior siblings')
    // control sockets, so the socket never sees EOF when core dies — the
    // orphaned engine can't tell its core is gone and lingers forever (a
    // process + shm leak per core restart).  The pre_exec dup2 below
    // re-clears CLOEXEC on the target fds 3/4, so the child still gets
    // its control socket + region.
    for fd in [parent_fd, child_fd, region_raw] {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags >= 0 {
            unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
        }
    }

    // marspot-session lives next to marspot-core; an explicit override
    // (dev / tests) wins.
    let session_bin = match std::env::var_os("MARSPOT_SESSION_BIN") {
        Some(p) => std::path::PathBuf::from(p),
        None => std::env::current_exe()?
            .parent()
            .map(|d| d.join("marspot-session"))
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "no current_exe parent")
            })?,
    };

    const SHM_TARGET_FD: RawFd = 4;
    lx_event!(
        "L3_SPAWN",
        "spawning L3 session",
        bin = session_bin.display(),
        cols = cols,
        rows = rows,
        control_fd = DEFAULT_CONTROL_FD,
        shm_fd = SHM_TARGET_FD
    );
    let mut cmd = Command::new(&session_bin);
    cmd.env(ENV_CONTROL_FD, DEFAULT_CONTROL_FD.to_string())
        .env(ENV_SHM_FD, SHM_TARGET_FD.to_string())
        // L2 owns session assignment: hand this L3 the exact session it
        // must drive so N children never race for the same one.
        .env("MARSPOT_SESSION_ID", session_id.to_string());
    // SAFETY: pre_exec runs between fork and exec; only async-signal-safe
    // libc calls (dup2/close/fcntl) are used, mirroring the shell→core
    // spawn template.
    unsafe {
        cmd.pre_exec(move || {
            // Move the inherited ends onto the well-known fds, then strip
            // CLOEXEC so they survive exec.
            for (src, dst) in [(child_fd, DEFAULT_CONTROL_FD), (region_raw, SHM_TARGET_FD)] {
                if src != dst {
                    if libc::dup2(src, dst) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                let flags = libc::fcntl(dst, libc::F_GETFD);
                if flags >= 0 {
                    libc::fcntl(dst, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
                }
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    lx_event!("L3_SPAWNED", "L3 child running", pid = child.id());

    // Parent no longer needs the child's socket end.
    unsafe { libc::close(child_fd) };

    // Map the region as a reader (mmap survives the fd closing, so the
    // owned `region` can drop after).
    let reader = GridShmReader::from_fd(region_raw)?;
    drop(region);

    // Control: write half → L3Conn (forward keys); read half → poke thread.
    // The poke thread also routes Cmd-C `SelectionText` replies into a
    // channel the L3Conn blocks on.
    let control = unsafe { UnixStream::from_raw_fd(parent_fd) };
    let reader_stream = control.try_clone()?;
    let tx = event_tx.clone();
    let (selection_tx, selection_rx) = std::sync::mpsc::channel::<(u32, String)>();
    std::thread::spawn(move || l3_reader_loop(reader_stream, tx, selection_tx));

    Ok(L3Spawn {
        child,
        control,
        reader,
        selection_rx,
    })
}

/// Boot/`[+]` helper: spawn an L3 and wrap it in a fresh L3-backed `Pane`.
fn spawn_l3_pane(
    cols: u16,
    rows: u16,
    session_id: u64,
    event_tx: &Sender<CoreEvent>,
) -> std::io::Result<Pane> {
    let spawn = spawn_l3(cols, rows, session_id, event_tx)?;
    Ok(Pane::new_l3(L3Conn::new(spawn, session_id)))
}

/// The full multi-pane UI state machine — `Marspot` (src/main.rs)
/// minus the AppKit window plumbing.  Mouse coordinates arrive in
/// view-local physical pixels, exactly what the shell's NSView
/// callbacks produce, so the `Layout` hit-test geometry is shared
/// Live state for one in-flight RFC-003 PaneSession.  Bit-for-bit
/// caps as carried on the wire.
#[derive(Clone, Copy, Debug)]
struct PaneSessionState {
    caps: u32,
}

impl PaneSessionState {
    fn has(&self, cap: u32) -> bool {
        (self.caps & cap) == cap
    }
}

/// verbatim.
struct CoreApp {
    renderer: MetalRenderer,
    layout: Layout,
    panes: Vec<Pane>,
    focused_idx: usize,
    custom_titles: Vec<Option<String>>,
    editing_title: Option<usize>,
    title_edit_buffer: String,
    /// Per-shelld-session right-side badge, set by L1 plugins via
    /// `MsgType::PaneBadge`.  Empty string clears via removal.
    pane_badges: std::collections::HashMap<u64, String>,
    /// Frames queued by event handlers (mouse_down etc.) to be
    /// written to the control socket by the main loop.  Avoids
    /// reaching the writer from inside the trait callbacks where
    /// the borrow tree doesn't permit it.
    pending_to_shell: Vec<(MsgType, Vec<u8>)>,
    /// RFC-003 pane sessions currently held by L1 plugins, keyed by
    /// shelld_session_id.  Membership routes L2 behaviour:
    ///   * LOCK_KEYS cap → key events forwarded as PaneSessionKey,
    ///                     not the PTY
    ///   * FREEZE_GRID cap → render keeps the last-painted instance
    ///                       buffer for that pane (C6)
    ///   * INPUT cap → informational; plugin writes via shelld
    pane_sessions: std::collections::HashMap<u64, PaneSessionState>,
    /// Rolling timestamps of recent Escape presses while a pane
    /// session is active; 3 within 5 s force-ends the session
    /// regardless of plugin opinion.  Bounded to the last 3 entries.
    esc_history: std::collections::VecDeque<std::time::Instant>,
    selection: Option<Selection>,
    selection_dragging: bool,
    layout_mode: LayoutMode,
    layout_picker_open: bool,
    sidebar_collapsed: bool,
    ime_preedit: String,
    client: Arc<ShelldClient>,
    /// Window physical dims + scale, updated by Resize frames.
    w_phys: f64,
    h_phys: f64,
    scale: f64,
    /// Set by anything that changes what the next frame should look
    /// like; cleared after each `render`.
    needs_render: bool,
    /// True once every pane's session has exited — the loop exits
    /// cleanly and the shell respawns a fresh core (which creates a
    /// fresh session), mirroring "marspot quits when all shells die".
    all_exited: bool,
    /// Last caret rect sent to the shell — dedupe so an idle cursor
    /// doesn't stream identical CaretRect frames at render cadence.
    last_caret_sent: Option<Option<(f64, f64, f64, f64)>>,
    /// `MARSPOT_L3=1`: panes are per-session L3 processes, so [+] spawns
    /// a fresh L3 (with an L2-allocated session) instead of an in-process
    /// shelld pane.  Clone of the event channel so a new L3's poke reader
    /// can wake the loop, exactly like the boot spawns.
    l3_mode: bool,
    event_tx: Sender<CoreEvent>,
}

impl CoreApp {
    /// L1 plugin → control socket → here: enter a PaneSession for the
    /// given shelld_session_id with `caps` capability bits.  Idempotent
    /// on the same caps; bumps the entry on a caps change.
    fn pane_session_begin(&mut self, shelld_session_id: u64, caps: u32) {
        let changed = self
            .pane_sessions
            .insert(shelld_session_id, PaneSessionState { caps })
            .map(|prev| prev.caps != caps)
            .unwrap_or(true);
        if changed {
            // FREEZE_GRID may want a redraw to (eventually) freeze
            // visibly; other caps don't change pixels right away.
            self.needs_render = true;
        }
    }

    /// L1 plugin released the pane back to live mode.
    fn pane_session_end(&mut self, shelld_session_id: u64) {
        if self.pane_sessions.remove(&shelld_session_id).is_some() {
            self.needs_render = true;
        }
    }

    /// Look up an active pane session by shelld_session_id; helper for
    /// the key/render branches.
    fn pane_session_for(&self, shelld_session_id: u64) -> Option<&PaneSessionState> {
        self.pane_sessions.get(&shelld_session_id)
    }

    /// True iff the *focused* pane is currently held by a plugin with
    /// the given capability.
    fn focused_pane_has_cap(&self, cap: u32) -> bool {
        let Some(p) = self.panes.get(self.focused_idx) else { return false; };
        let Some(sid) = p.shelld_session_id() else { return false; };
        self.pane_session_for(sid).is_some_and(|s| s.has(cap))
    }

    /// Returns the focused pane's shelld_session_id if it currently
    /// has any PaneSession (regardless of caps) — used for the Esc
    /// hatch + key routing.
    fn focused_pane_active_session(&self) -> Option<u64> {
        let p = self.panes.get(self.focused_idx)?;
        let sid = p.shelld_session_id()?;
        if self.pane_sessions.contains_key(&sid) {
            Some(sid)
        } else {
            None
        }
    }

    /// Record an Escape press at `now`.  Returns true if the rolling
    /// window contains ≥ 3 escapes within 5 s — the caller then sends
    /// PaneSessionUserEscape to force-end the session.
    fn note_escape_for_pane_session(&mut self, now: std::time::Instant) -> bool {
        const WINDOW: std::time::Duration = std::time::Duration::from_secs(5);
        const THRESHOLD: usize = 3;
        while let Some(&front) = self.esc_history.front() {
            if now.duration_since(front) > WINDOW {
                self.esc_history.pop_front();
            } else {
                break;
            }
        }
        self.esc_history.push_back(now);
        if self.esc_history.len() > THRESHOLD {
            self.esc_history.pop_front();
        }
        self.esc_history.len() >= THRESHOLD
    }

    /// L1 plugin → control socket → here: stash a per-shelld-session
    /// right-side decoration for the title strip.  Empty `text` clears
    /// any prior badge.  Forces a redraw on transition.
    fn set_pane_badge(&mut self, shelld_session_id: u64, text: String) {
        let prev = self.pane_badges.get(&shelld_session_id).cloned();
        let changed = if text.is_empty() {
            self.pane_badges.remove(&shelld_session_id).is_some()
        } else if prev.as_deref() != Some(text.as_str()) {
            self.pane_badges.insert(shelld_session_id, text);
            true
        } else {
            false
        };
        if changed {
            self.needs_render = true;
        }
    }

    fn rebuild_layout(&mut self) {
        let (cell_w, cell_h) = self.renderer.cell_dims();
        let sidebar_phys = if self.sidebar_collapsed {
            0.0
        } else {
            SIDEBAR_W_LOGICAL * self.scale
        };
        let (lc, lr) = self.layout_mode.dims();
        let layout = Layout::build(
            self.w_phys,
            self.h_phys,
            sidebar_phys,
            HEADER_PT * self.scale,
            CELL_TITLE_PT * self.scale,
            lc,
            lr,
            cell_w,
            cell_h,
        )
        .with_chrome(self.scale, self.layout_picker_open, self.panes.len());
        for (i, p) in self.panes.iter_mut().enumerate() {
            if let Some(rect) = layout.cells.get(i) {
                p.resize(rect.cols, rect.rows);
            }
        }
        self.layout = layout;
        self.needs_render = true;
        // The shape of the BG region just changed (sidebar / layout
        // mode / pane count / window dims).  Force a hard Clear on
        // the next IOSurface render so any newly-uncovered area
        // shows SIDEBAR_BG, not the previous frame's stale pixels.
        // (Steady-state frames use Load to dodge the cross-process
        // race; see `MetalRenderer::clear_bg_required`.)
        self.renderer.mark_bg_clear_required();
    }

    /// Spawn a fresh session and append it.  Refuses past
    /// `SESSION_COUNT_HARD_CAP`.  Sized to the cell it will land in
    /// (falling back to the first cell's shape) so the shell prompt
    /// prints at the right width from its very first byte.
    fn spawn_session(&mut self) {
        if self.panes.len() >= SESSION_COUNT_HARD_CAP {
            return;
        }
        let (cols, rows) = self
            .layout
            .cells
            .get(self.panes.len())
            .or_else(|| self.layout.cells.first())
            .map(|c| (c.cols, c.rows))
            .unwrap_or((INITIAL_COLS, INITIAL_ROWS));
        if self.l3_mode {
            // L2-allocated fresh session → its own L3 process, same path
            // as the boot spawns.  Keeps the L3 world pure: no shelld pane
            // ever mixes into an L3-mode window.
            match self
                .client
                .create_session(cols, rows, "")
                .and_then(|id| spawn_l3_pane(cols, rows, id, &self.event_tx))
            {
                Ok(pane) => {
                    self.panes.push(pane);
                    self.custom_titles.push(None);
                }
                Err(e) => lx_error!("core.spawn.l3_failed", &format!("{e}")),
            }
            return;
        }
        match self.client.new_session(cols, rows, "") {
            Ok(s) => {
                self.panes.push(Pane::new_shelld(s));
                self.custom_titles.push(None);
            }
            Err(e) => {
                lx_error!("core.spawn.shelld_failed", &format!("{e}"));
            }
        }
    }

    /// Per-session silent update (target #4 step 5a): bring up a
    /// replacement L3 (the current `marspot-session` binary) on each *idle*
    /// (non-focused) pane's session and let `L3Conn::poll` swap to it once
    /// it has replayed the bytelog + published — invisible, since the
    /// replayed screen matches.  The focused pane is left alone (a replay
    /// could blip an interactive TUI mid-keystroke); step 5b adds a
    /// click-to-swap affordance for it.  Skips panes already swapping or
    /// exited.  Behind `MARSPOT_L3=1` (no L3 panes otherwise → no-op).
    fn swap_idle_l3(&mut self) {
        // A session-only live update (SIGUSR2) staged a new engine in
        // pending/; promote it into current/ first so each replacement L3
        // (spawned from core's sibling = current/marspot-session) boots the
        // new binary.  A no-op if nothing's staged — then this is a plain
        // re-spawn of the same engine (still useful as a manual refresh).
        match marspot::updater::promote_pending_session() {
            Ok(true) => lx_event!(
                "SESSION_PROMOTE",
                "promoted staged marspot-session → current/ for swap"
            ),
            Ok(false) => {}
            Err(e) => lx_error!("core.promote.swap_failed", &format!("{e}")),
        }
        // L2/L3 updates are user-invisible by design: idle panes promote
        // silently when the replacement's grid mirror matches; focused
        // panes used to defer behind a ↻ refresh affordance, but the
        // swap is structurally atomic — both old and new L3 subscribe to
        // the same shelld session, the bytelog replay reconstructs an
        // identical grid, and the visible mirror only flips on the next
        // `try_promote` call (one render frame). So we begin the swap
        // for every pane unconditionally and let `try_promote` make the
        // switch when the new L3's grid is steady.
        for i in 0..self.panes.len() {
            let pane = &self.panes[i];
            if !pane.is_l3() || pane.is_exited() || pane.session().is_l3_swapping() {
                continue;
            }
            self.begin_pane_swap(i);
        }
    }

    /// Call right before moving focus to `new_idx`: if the currently-
    /// focused pane has a deferred update, it's about to become idle, so
    /// trigger its swap now (a replay is safe once it's not under the
    /// user's hands).  No-op if focus isn't actually changing.
    fn resolve_pending_on_defocus(&mut self, new_idx: usize) {
        if new_idx != self.focused_idx
            && self
                .panes
                .get(self.focused_idx)
                .is_some_and(|p| p.update_pending())
        {
            self.begin_pane_swap(self.focused_idx);
        }
    }

    /// Bring up a replacement L3 on pane `i`'s session and stage the swap
    /// (clearing any deferred-update flag).  Caller has checked it's a live,
    /// not-already-swapping L3 pane.
    fn begin_pane_swap(&mut self, i: usize) {
        let pane = &self.panes[i];
        let Some(sid) = pane.session().l3_session_id() else {
            return;
        };
        let (cols, rows) = (pane.session().grid().cols(), pane.session().grid().rows());
        match spawn_l3(cols, rows, sid, &self.event_tx) {
            Ok(spawn) => {
                self.panes[i].session_mut().begin_l3_swap(spawn);
                self.panes[i].set_update_pending(false);
                self.needs_render = true;
                lx_event!(
                    "L3_SWAP_STAGED",
                    "staged silent swap for L3 session",
                    session = sid,
                    pane = i
                );
            }
            Err(e) => lx_error!(
                "core.swap.spawn_failed",
                &format!("{e}"),
                session = sid,
                pane = i
            ),
        }
    }

    /// Terminate `panes[idx]` and keep all parallel state in sync.
    /// Caller refuses the call when it would leave zero sessions.
    fn close_session(&mut self, idx: usize) {
        if idx >= self.panes.len() {
            return;
        }
        // Ask shelld to terminate the session AND delete its bytelog
        // before we drop the pane locally. Without this, shelld keeps
        // the session in its `Sessions` map (with bytelog on disk), and
        // the next time the GUI calls `list_sessions` to refill a slot
        // it sees this id as alive and re-attaches — replaying the
        // entire bytelog into the new pane, so the user sees the
        // content they just clicked × to discard.
        if let Some(id) = self.panes[idx].shelld_session_id() {
            if let Err(e) = self.client.kill_session(id) {
                lx_warn!(
                    "core.close_session.kill_failed",
                    &format!("{e}"),
                    id = id,
                    pane_idx = idx
                );
            }
        }
        self.panes.remove(idx);
        if idx < self.custom_titles.len() {
            self.custom_titles.remove(idx);
        }
        if !self.panes.is_empty() {
            if self.focused_idx == idx {
                self.focused_idx = idx.min(self.panes.len() - 1);
            } else if self.focused_idx > idx {
                self.focused_idx -= 1;
            }
        } else {
            self.focused_idx = 0;
        }
        if let Some(sel) = self.selection {
            if sel.session_idx == idx {
                self.selection = None;
                self.selection_dragging = false;
            } else if sel.session_idx > idx {
                self.selection = Some(Selection {
                    session_idx: sel.session_idx - 1,
                    ..sel
                });
            }
        }
        match self.editing_title {
            Some(i) if i == idx => {
                self.editing_title = None;
                self.title_edit_buffer.clear();
            }
            Some(i) if i > idx => {
                self.editing_title = Some(i - 1);
            }
            _ => {}
        }
    }

    fn commit_title_edit(&mut self) {
        if let Some(idx) = self.editing_title.take() {
            if idx < self.custom_titles.len() {
                let trimmed = self.title_edit_buffer.trim().to_string();
                // Persist to shelld so the title survives a dual-core
                // silent swap.  Per [[project-silent-update-gate]] the
                // pre-2026-06-15 design kept titles in core memory only,
                // so any swap reset them to None on the new core boot.
                // shelld holds the title for the session's lifetime.
                if let Some(sid) = self.panes[idx].shelld_session_id() {
                    if let Err(e) = self.client.set_title(sid, &trimmed) {
                        lx_warn!(
                            "core.set_title.failed",
                            &format!("{e}"),
                            session = sid
                        );
                    }
                }
                self.custom_titles[idx] =
                    if trimmed.is_empty() { None } else { Some(trimmed) };
            }
            self.title_edit_buffer.clear();
        }
    }

    fn cancel_title_edit(&mut self) {
        self.editing_title = None;
        self.title_edit_buffer.clear();
    }

    fn copy_selection_to_clipboard(&mut self) -> bool {
        let Some(sel) = self.selection else { return false };
        let idx = sel.session_idx;
        // L3 owns the real grid + scrollback; L2's mirror is a window-only
        // synthetic grid that `grid_selection_text` can't read back, so the
        // text round-trips through the session process.  In-process panes
        // read it locally.  The round-trip is made reliable by a per-request
        // sequence id (see `request_selection_text`) so a late reply from a
        // timed-out request can't alias the next copy.
        let is_l3 = self.panes.get(idx).is_some_and(|p| p.is_l3());
        let text = if is_l3 {
            let blockwise = sel.mode == marspot::ui::SelectionMode::Blockwise;
            self.panes
                .get_mut(idx)
                .and_then(|p| p.session_mut().request_selection_text(sel.anchor, sel.focus, blockwise))
        } else {
            self.panes.get(idx).and_then(|pane| selection_text(pane, &sel))
        };
        match text {
            Some(text) => marspot::input::write_clipboard_text(&text),
            None => false,
        }
    }

    fn key(&mut self, event: MarspotKeyEvent, modifiers: Modifiers) {
        use marspot::input::{KeyState, LogicalKey, NamedKey};

        // RFC-003 LOCK_KEYS: if the focused pane is in a plugin-held
        // PaneSession that asked for the keyboard, route the event up
        // to L1 instead of forwarding to the PTY.  Also count Esc
        // presses against the force-end window; 3 in 5 s force-ends
        // the session via PaneSessionUserEscape.
        //
        // Sits ahead of Cmd-C / Cmd-B / title-edit because the
        // user might genuinely need Esc to force-end a stuck session,
        // and we don't want any L2 shortcut to swallow it first.
        if event.state == KeyState::Pressed {
            if let Some(active_sid) = self.focused_pane_active_session() {
                let has_lock = self
                    .pane_session_for(active_sid)
                    .is_some_and(|s| s.has(marspot::shell_proto::PANE_SESSION_CAP_LOCK_KEYS));
                let is_esc = matches!(
                    event.logical,
                    LogicalKey::Named(NamedKey::Escape)
                );
                if is_esc {
                    let force_end =
                        self.note_escape_for_pane_session(std::time::Instant::now());
                    if force_end {
                        let payload = marspot::shell_proto::encode_pane_session_user_escape(
                            active_sid,
                        );
                        self.pending_to_shell
                            .push((MsgType::PaneSessionUserEscape, payload));
                        // Also drop the L2 mirror immediately so the
                        // pane goes back to normal even if L1 lags.
                        self.pane_session_end(active_sid);
                        return;
                    }
                }
                if has_lock {
                    let wire = marspot::shell_proto::event_to_wire(&event, modifiers);
                    let payload = marspot::shell_proto::encode_pane_session_key(
                        active_sid, &wire,
                    );
                    self.pending_to_shell
                        .push((MsgType::PaneSessionKey, payload));
                    return;
                }
            }
        }

        // Cmd-C: copy current text selection to the macOS clipboard.
        if event.state == KeyState::Pressed
            && modifiers.super_key()
            && matches!(event.logical, LogicalKey::Char(c) if c.eq_ignore_ascii_case(&'c'))
        {
            if self.copy_selection_to_clipboard() {
                return;
            }
        }

        // Cmd-B: toggle the sidebar (VSCode / Cursor convention).
        if event.state == KeyState::Pressed
            && modifiers.super_key()
            && matches!(event.logical, LogicalKey::Char(c) if c.eq_ignore_ascii_case(&'b'))
        {
            self.sidebar_collapsed = !self.sidebar_collapsed;
            self.rebuild_layout();
            return;
        }

        // Title-edit mode intercepts the keyboard before the PTY
        // mapper sees anything.  Enter commits, Esc cancels,
        // Backspace pops a char, printable text appends.
        if self.editing_title.is_some() {
            if event.state == KeyState::Pressed && !modifiers.super_key() {
                match &event.logical {
                    LogicalKey::Named(NamedKey::Enter) => {
                        self.commit_title_edit();
                        self.needs_render = true;
                        return;
                    }
                    LogicalKey::Named(NamedKey::Escape) => {
                        self.cancel_title_edit();
                        self.needs_render = true;
                        return;
                    }
                    LogicalKey::Named(NamedKey::Backspace) => {
                        self.title_edit_buffer.pop();
                        self.needs_render = true;
                        return;
                    }
                    _ => {
                        if let Some(t) = &event.text {
                            for ch in t.chars() {
                                if !ch.is_control() {
                                    self.title_edit_buffer.push(ch);
                                }
                            }
                            self.needs_render = true;
                            return;
                        }
                    }
                }
            }
            return;
        }

        let Some(pane) = self.panes.get(self.focused_idx) else { return };

        // L3-backed pane: forward the key *event* to the session process,
        // which encodes with its own modes + local-echoes, then republishes
        // the grid (we re-read it on the GridReady wake).  No local write /
        // predict here.
        if pane.is_l3() {
            // Cmd-V: L3 is GUI-free and can't read the macOS pasteboard, so
            // L2 resolves it here and forwards the text; the session
            // bracketed-wraps it and writes the PTY.  (Without this, Cmd-V
            // was forwarded as a raw keystroke and silently did nothing —
            // paste was broken the moment L3 became the default.)
            if event.state == KeyState::Pressed
                && modifiers.super_key()
                && matches!(event.logical, LogicalKey::Char(c) if c.eq_ignore_ascii_case(&'v'))
            {
                if let Some(text) = marspot::input::read_clipboard_text() {
                    if self.panes[self.focused_idx].snap_to_live() {
                        self.needs_render = true;
                    }
                    self.panes[self.focused_idx].session_mut().forward_paste(&text);
                }
                return;
            }
            if self.panes[self.focused_idx].snap_to_live() {
                self.needs_render = true;
            }
            if self.selection.is_some() {
                self.selection = None;
                self.selection_dragging = false;
                self.needs_render = true;
            }
            self.panes[self.focused_idx]
                .session_mut()
                .forward_key(&event, modifiers);
            return;
        }

        let app_mode = pane.session().cursor_key_application_mode();
        let bracketed = pane.session().bracketed_paste_mode();
        if let Some(bytes) = key_event_to_bytes(
            &event,
            modifiers,
            app_mode,
            bracketed,
            marspot::input::read_clipboard_text,
        ) {
            // Typing snaps the focused pane's view back to live.
            if self.panes[self.focused_idx].snap_to_live() {
                self.needs_render = true;
            }
            // Typing into the PTY clears any text selection.
            if self.selection.is_some() {
                self.selection = None;
                self.selection_dragging = false;
                self.needs_render = true;
            }
            let session = self.panes[self.focused_idx].session_mut();
            let _ = session.write(&bytes);
            // Local-echo: paint each printable-ASCII byte to the grid
            // immediately, ahead of the PTY round trip — but only
            // for plain text runs.  An escape sequence (ESC-leading)
            // is a control code: the leading 0x1b isn't printable so
            // predict_byte rejects it, but the sequence's tail
            // (`[C`, `[A`, …) is ASCII printable and would smear
            // literally into the grid.  Skip the whole run when it
            // starts with ESC.
            let mut predicted = false;
            if !bytes.starts_with(&[0x1b]) {
                for &b in bytes.as_ref() {
                    if session.terminal_mut().predict_byte(b) {
                        predicted = true;
                    }
                }
            }
            if predicted {
                self.needs_render = true;
            }
        }
    }

    /// Hit-test the right-side plugin badge's clickable prefix (text
    /// before the first space).  Returns the pane index when a click
    /// at (x_phys, y_phys) hits the underlined prefix; None
    /// otherwise.  Mirrors the geometry the renderer uses in
    /// `render_metal::build_instances` so a visual hit lines up with
    /// the logical one.
    fn hit_test_pane_badge_prefix(
        &self,
        x_phys: f64,
        y_phys: f64,
    ) -> Option<usize> {
        let (cell_w, _) = self.renderer.cell_dims();
        let cell_w = cell_w as f64;
        let padding = self.layout.padding;
        let title_h = self.layout.cell_title_h;
        let cell_count = self.layout.cells.len();
        for (i, p) in self.panes.iter().enumerate().take(cell_count) {
            let sid = match p.shelld_session_id() {
                Some(s) => s,
                None => continue,
            };
            let badge = match self.pane_badges.get(&sid) {
                Some(b) if !b.is_empty() => b,
                _ => continue,
            };
            let prefix = match badge.split(' ').next() {
                Some(p) if !p.is_empty() => p,
                _ => continue,
            };
            let badge_chars = badge.chars().count() as f64;
            let prefix_chars = prefix.chars().count() as f64;
            let rect = &self.layout.cells[i];
            // Match the renderer's `reserved` carve-out for the
            // refresh affordance on the focused pane with a staged
            // update.
            let reserved = if p.update_pending() && i == self.focused_idx {
                cell_w * 1.5
            } else {
                0.0
            };
            let badge_x = rect.x + rect.w - padding - reserved - badge_chars * cell_w;
            let prefix_lo = badge_x;
            let prefix_hi = prefix_lo + prefix_chars * cell_w;
            let y_lo = rect.y_top;
            let y_hi = y_lo + title_h;
            if x_phys >= prefix_lo
                && x_phys < prefix_hi
                && y_phys >= y_lo
                && y_phys < y_hi
            {
                return Some(i);
            }
        }
        None
    }

    fn mouse_down(&mut self, x_phys: f64, y_phys: f64, modifiers: Modifiers) {
        let layout = &self.layout;
        let layout_btn_hit = layout.hit_test_layout_button(x_phys, y_phys);
        let sidebar_btn_hit = layout.hit_test_sidebar_button(x_phys, y_phys);
        let picker_option_hit = layout.hit_test_picker_option(x_phys, y_phys);
        let picker_panel_hit = layout.hit_test_picker_panel(x_phys, y_phys);
        let close_session_hit = layout.hit_test_close_session(x_phys, y_phys);
        let add_session_hit = layout.hit_test_add_session_button(x_phys, y_phys);
        let refresh_hit = layout.hit_test_cell_refresh(x_phys, y_phys);

        // Sidebar toggle: highest-priority chrome action so a click
        // on the chip never falls through to the cell underneath.
        if sidebar_btn_hit {
            self.sidebar_collapsed = !self.sidebar_collapsed;
            self.rebuild_layout();
            return;
        }
        if self.layout_picker_open {
            if let Some(opt_idx) = picker_option_hit {
                self.layout_mode = PICKER_LAYOUTS[opt_idx];
                self.layout_picker_open = false;
                self.rebuild_layout();
                return;
            }
            if layout_btn_hit || picker_panel_hit {
                self.layout_picker_open = false;
                self.rebuild_layout();
                return;
            }
            // Click outside the picker: close it and fall through so
            // the user doesn't have to click twice.
            self.layout_picker_open = false;
            self.rebuild_layout();
        } else if layout_btn_hit {
            self.layout_picker_open = true;
            self.rebuild_layout();
            return;
        }

        // Sidebar close-[×]: refuse to close the last session.
        if let Some(idx) = close_session_hit {
            if self.panes.len() > 1 && idx < self.panes.len() {
                self.close_session(idx);
                self.rebuild_layout();
            }
            return;
        }

        // Sidebar [+] add-session.
        if add_session_hit {
            if self.panes.len() < SESSION_COUNT_HARD_CAP {
                self.spawn_session();
                self.rebuild_layout();
            }
            return;
        }

        // Refresh affordance: click the deferred-update glyph on a pending
        // pane to trigger its silent swap now.  Sits inside the title strip,
        // so it must take priority over the title-edit hit below — but only
        // when that pane actually has an update staged (else fall through to
        // normal title behaviour).
        if let Some(i) = refresh_hit {
            if self.panes.get(i).is_some_and(|p| p.update_pending()) {
                self.begin_pane_swap(i);
                return;
            }
        }

        // Plugin badge prefix click: route to L1 (the plugin owns
        // what the prefix means and what cycling it does).  Sits in
        // the same title strip as title-edit + refresh; check here
        // before title-edit so a click on `P<n>` doesn't drop the
        // pane into rename mode.
        if let Some(i) = self.hit_test_pane_badge_prefix(x_phys, y_phys) {
            if let Some(sid) = self.panes.get(i).and_then(|p| p.shelld_session_id())
            {
                let payload = marspot::shell_proto::encode_pane_badge_clicked(sid);
                self.pending_to_shell
                    .push((MsgType::PaneBadgeClicked, payload));
                return;
            }
        }

        let layout = &self.layout;
        let row_phys = marspot::layout::SIDEBAR_ROW_H_PHYS;
        let top_pad_phys = layout.top_inset + layout.sidebar_top_pad_phys;
        let title_hit = layout.hit_test_cell_title(x_phys, y_phys);
        let sidebar_hit = layout.hit_test_sidebar_row(
            x_phys,
            y_phys,
            top_pad_phys,
            row_phys,
            self.panes.len(),
        );
        let cell_hit = layout.hit_test(x_phys, y_phys);
        let (cw, ch) = self.renderer.cell_dims();
        let cell_pos_hit = layout.hit_test_cell_pos(x_phys, y_phys, cw, ch);

        // Title-strip click → enter edit mode for that cell.
        if let Some(idx) = title_hit {
            if idx < self.panes.len() {
                self.commit_title_edit();
                self.resolve_pending_on_defocus(idx);
                self.focused_idx = idx;
                self.editing_title = Some(idx);
                self.title_edit_buffer = self
                    .custom_titles
                    .get(idx)
                    .and_then(|t| t.clone())
                    .unwrap_or_default();
                let _ = self.panes[self.focused_idx].snap_to_live();
                self.selection = None;
                self.selection_dragging = false;
                self.needs_render = true;
                return;
            }
        }

        // Click outside the title strip while editing commits first.
        if self.editing_title.is_some() {
            self.commit_title_edit();
            self.needs_render = true;
        }

        // Click in cell body → start a fresh selection there AND
        // focus that cell.
        let prior_selection = self.selection;
        self.selection = None;
        self.selection_dragging = false;
        if let Some((idx, col, row)) = cell_pos_hit {
            let pane = &self.panes.get(idx);
            if let Some(pane) = pane {
                let rows = pane.session().grid().rows() as u32;
                let vo = pane.view_offset() as u32;
                let abs = vo + rows.saturating_sub(1).saturating_sub(row as u32);
                self.selection = Some(Selection {
                    session_idx: idx,
                    anchor: (col, abs),
                    focus: (col, abs),
                    mode: if modifiers.alt_key() {
                        SelectionMode::Blockwise
                    } else {
                        SelectionMode::Linewise
                    },
                });
                self.selection_dragging = true;
                if idx != self.focused_idx {
                    self.resolve_pending_on_defocus(idx);
                    self.focused_idx = idx;
                }
                self.needs_render = true;
                return;
            }
        }
        if prior_selection.is_some() {
            self.needs_render = true;
        }

        let new_focus = sidebar_hit.or(cell_hit);
        if let Some(idx) = new_focus {
            if idx < self.panes.len() && idx != self.focused_idx {
                self.resolve_pending_on_defocus(idx);
                self.focused_idx = idx;
                let _ = self.panes[self.focused_idx].snap_to_live();
                self.needs_render = true;
            }
        }
    }

    fn mouse_drag(&mut self, x_phys: f64, y_phys: f64) {
        if !self.selection_dragging {
            return;
        }
        let (cw, ch) = self.renderer.cell_dims();
        let target_idx = match self.selection.as_ref() {
            Some(s) => s.session_idx,
            None => return,
        };
        let cell = match self.layout.cells.get(target_idx) {
            Some(c) => c.clone(),
            None => return,
        };
        let inner_x = cell.x + self.layout.padding;
        let inner_y = cell.y_top + self.layout.cell_title_h + self.layout.padding;

        // Past-edge auto-scroll, NSTextView-style (see src/main.rs
        // for the rate-limit rationale).
        let max_row = cell.rows.saturating_sub(1) as i64;
        let raw_row = ((y_phys - inner_y) / ch).floor() as i64;
        if raw_row < 0 {
            self.panes[target_idx].apply_scroll_lines(1);
        } else if raw_row > max_row {
            self.panes[target_idx].apply_scroll_lines(-1);
        }

        let dx = (x_phys - inner_x).max(0.0);
        let dy = (y_phys - inner_y).max(0.0);
        let mut col = (dx / cw).floor() as i64;
        let mut row = (dy / ch).floor() as i64;
        if col < 0 {
            col = 0;
        }
        if row < 0 {
            row = 0;
        }
        let max_col = cell.cols.saturating_sub(1) as i64;
        if col > max_col {
            col = max_col;
        }
        if row > max_row {
            row = max_row;
        }

        // Re-read view_offset AFTER any auto-scroll above.
        let pane = &self.panes[target_idx];
        let rows = pane.session().grid().rows() as u32;
        let vo = pane.view_offset() as u32;
        let abs = vo + rows.saturating_sub(1).saturating_sub(row as u32);

        let Some(sel) = self.selection.as_mut() else { return };
        sel.focus = (col as u16, abs);
        self.needs_render = true;
    }

    fn mouse_up(&mut self) {
        // A click without movement leaves anchor == focus → treat as
        // "no selection" so a stray single-click doesn't ghost a
        // single-cell highlight.
        if self.selection_dragging {
            self.selection_dragging = false;
            if let Some(sel) = self.selection {
                if sel.anchor == sel.focus {
                    self.selection = None;
                }
            }
        }
    }

    fn scroll(&mut self, dy_phys: f64, precise: bool) {
        let (_, cell_h) = self.renderer.cell_dims();
        let lines = scroll_lines(dy_phys, precise, cell_h);
        if lines == 0 {
            return;
        }
        if self.panes[self.focused_idx].apply_scroll_lines(lines) {
            self.needs_render = true;
        }
    }

    fn preedit(&mut self, text: String) {
        if self.ime_preedit != text {
            self.ime_preedit = text;
            self.needs_render = true;
        }
    }

    /// Drain pending shelld DATA into every pane's grid, keeping
    /// selection state honest (scroll-push bump + in-place-repaint
    /// drop — same contract as src/main.rs `user_event`).
    fn pump_all(&mut self) -> usize {
        let mut total = 0;
        // Snapshot which session ids are frozen by an L1 PaneSession;
        // we can't borrow `self.pane_sessions` and `self.panes` at
        // the same time inside the loop.
        let frozen: std::collections::HashSet<u64> = self
            .pane_sessions
            .iter()
            .filter_map(|(sid, st)| {
                if st.has(marspot::shell_proto::PANE_SESSION_CAP_FREEZE_GRID) {
                    Some(*sid)
                } else {
                    None
                }
            })
            .collect();
        for (i, p) in self.panes.iter_mut().enumerate() {
            // FREEZE_GRID: skip the pump entirely so the grid the
            // renderer sees stays exactly as it was when the plugin
            // took over.  PTY bytes still queue (shelld + client
            // channel), they get consumed in one shot when the
            // session ends and pump runs again.
            let is_frozen = p
                .shelld_session_id()
                .is_some_and(|sid| frozen.contains(&sid));
            if is_frozen {
                continue;
            }
            let n = p.pump();
            total += n;
            let pushed = p.drain_scroll_push_delta();
            // Slide the selection anchor + focus when the PTY pushed
            // rows into scrollback so the highlight tracks the same
            // bytes as they roll up.  We do NOT clear the selection
            // just because the PTY autonomously emitted bytes — a
            // claudecode pane prints a spinner every few hundred ms
            // and the old `has_bytes && !dragging → None` rule made
            // selection disappear before the user could Cmd-C.
            // Keyboard input + new clicks + focus changes still clear
            // selection in their own paths; PTY autonomy doesn't.
            if let Some(sel) = self.selection.as_mut() {
                if sel.session_idx == i && pushed > 0 {
                    let bump = pushed as u32;
                    sel.anchor.1 = sel.anchor.1.saturating_add(bump);
                    sel.focus.1 = sel.focus.1.saturating_add(bump);
                }
            }
        }
        if total > 0 {
            self.needs_render = true;
        }
        if !self.panes.is_empty() && self.panes.iter().all(|p| p.is_exited()) {
            for p in &mut self.panes {
                p.pump();
            }
            self.all_exited = true;
        }
        total
    }

    /// Render the full UI into `target_tex` and return the focused-
    /// pane caret rect (view-local physical pixels) for the IME
    /// candidate window, or `None` when the cursor is hidden.
    fn render(
        &mut self,
        target_tex: &objc2::rc::Retained<ProtocolObject<dyn MTLTexture>>,
    ) -> Option<(f64, f64, f64, f64)> {
        let focused = self.focused_idx;

        let labels: Vec<String> = (1..=self.panes.len()).map(|n| n.to_string()).collect();
        let states: Vec<SessionState> =
            self.panes.iter().map(|p| p.session().state()).collect();

        // Resolved label per cell: edit-mode buffer → user-set custom
        // title → default ordinal label.
        let resolved_labels: Vec<String> = (0..self.panes.len())
            .map(|i| {
                if self.editing_title == Some(i) {
                    self.title_edit_buffer.clone()
                } else if let Some(Some(custom)) = self.custom_titles.get(i) {
                    custom.clone()
                } else {
                    labels.get(i).cloned().unwrap_or_default()
                }
            })
            .collect();
        let titles: Vec<String> = (0..self.panes.len())
            .map(|i| {
                let mut s = resolved_labels.get(i).cloned().unwrap_or_default();
                if self.editing_title == Some(i) {
                    s.push('▏');
                }
                s
            })
            .collect();
        let sidebar_labels: Vec<String> = resolved_labels
            .iter()
            .map(|s| truncate_for_sidebar(s, MAX_SIDEBAR_LABEL_CHARS))
            .collect();

        // Cap views to the layout's cell count — sessions past it
        // stay alive in the sidebar without a main-area cell.
        let cell_count = self.layout.cells.len();
        let views: Vec<SessionView> = self
            .panes
            .iter()
            .take(cell_count)
            .enumerate()
            .map(|(i, p)| {
                let badge = p
                    .shelld_session_id()
                    .and_then(|sid| self.pane_badges.get(&sid).map(|s| s.as_str()))
                    .unwrap_or("");
                let mut v = p.view(
                    i == focused,
                    titles.get(i).map(|s| s.as_str()).unwrap_or(""),
                    badge,
                );
                if i == focused && p.view_offset() == 0 {
                    v.ime_preedit = self.ime_preedit.as_str();
                }
                v.selection = self
                    .selection
                    .as_ref()
                    .and_then(|sel| selection_view_for_pane(p, sel, i));
                v
            })
            .collect();
        let entries: Vec<SidebarEntry> = sidebar_labels
            .iter()
            .zip(states.iter())
            .map(|(label, state)| SidebarEntry {
                label: label.as_str(),
                state: *state,
            })
            .collect();
        let (cell_w, cell_h) = self.renderer.cell_dims();
        self.renderer
            .render_layout_to_texture(target_tex, &self.layout, &views, &entries, focused);
        self.needs_render = false;

        self.panes.get(focused).and_then(|pane| {
            if !pane.session().cursor_visible() {
                return None;
            }
            let (col, row) = pane.session().grid().cursor();
            self.layout
                .caret_view_phys_rect(focused, col, row, cell_w, cell_h)
        })
    }
}

fn main() {
    marspot::logx::init("core");
    lx_event!(
        "CORE_BOOT",
        "marspot-core started",
        version = env!("MARSPOT_VERSION_CORE"),
        git = option_env!("MARSPOT_GIT_SHA").unwrap_or("unknown"),
        built = option_env!("MARSPOT_BUILD_TS").unwrap_or("unknown"),
        pid = std::process::id()
    );

    let front_id: u32 = env_required(ENV_SURFACE_ID);
    // PROTO_VERSION=2 shell sets ENV_SURFACE_ID_BACK to the second
    // surface in the pair.  Pre-A2-A4 (PROTO_VERSION=1) shells don't
    // set it — fall back to front so back==front, collapsing the
    // double-buffer to a single-surface render path.  Correctness is
    // preserved (we just lose the race protection) and a mixed-version
    // install (NEW core + OLD shell after a botched dual-core swap)
    // doesn't crash on the missing env var.
    let back_id: u32 = std::env::var(ENV_SURFACE_ID_BACK)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(front_id);
    let w_phys: f64 = env_required(ENV_SURFACE_WIDTH);
    let h_phys: f64 = env_required(ENV_SURFACE_HEIGHT);
    let scale: f64 = env_required(ENV_SURFACE_SCALE);

    lx_event!(
        "SURFACE_ATTACH",
        "attaching IOSurface pair (PROTO_VERSION=2 double-buffer)",
        front_id = front_id,
        back_id = back_id,
        w_phys = w_phys,
        h_phys = h_phys,
        scale = scale
    );

    let front = IOSurface::lookup(front_id)
        .unwrap_or_else(|| panic!("[core] IOSurfaceLookup({front_id}) returned nil"));
    front.increment_use();
    let back = IOSurface::lookup(back_id)
        .unwrap_or_else(|| panic!("[core] IOSurfaceLookup({back_id}) returned nil"));
    back.increment_use();

    let renderer = MetalRenderer::new_headless().expect("[core] MetalRenderer::new_headless");
    // Double-buffer: own a (surface, texture) pair.  Per-frame render
    // alternates `writing_idx`; the shell's presenter listens for
    // `SurfaceReady(id)` and points at whichever slot is freshly done.
    // Eliminates the cross-process mid-render race that was the
    // dominant flash source (handoff 2026-06-15).
    let mut surfaces: [IOSurface; 2] = [front, back];
    let mut target_tex: [objc2::rc::Retained<ProtocolObject<dyn MTLTexture>>; 2] = [
        surfaces[0]
            .make_metal_texture(renderer.device())
            .expect("[core] make_metal_texture front"),
        surfaces[1]
            .make_metal_texture(renderer.device())
            .expect("[core] make_metal_texture back"),
    ];
    // Start writing into slot 0 — the shell's presenter starts at idx
    // 0 too (`set_pair` resets `current_idx` to 0), so the first
    // SurfaceReady(surfaces[0].id()) is a no-op flip but the
    // accompanying `frame_pending=true` makes the shell actually
    // present.
    let mut writing_idx: usize = 0;

    // Unified event channel: the control-socket reader pushes
    // CoreEvents; the shelld wake callback pushes `PumpShelld`.
    // Main loop blocks on `recv_timeout` so it sleeps until *any*
    // event arrives — idle CPU = 0.
    let (event_tx, event_rx): (Sender<CoreEvent>, Receiver<CoreEvent>) = mpsc::channel();
    let event_tx_for_wake = event_tx.clone();
    let wake = move || {
        let _ = event_tx_for_wake.send(CoreEvent::PumpShelld);
    };

    let shelld_sock = marspot::paths::shelld_socket();
    lx_info!(
        "core.shelld.connect",
        "connecting to shelld",
        sock = shelld_sock.display()
    );
    let client = match ShelldClient::connect(&shelld_sock, wake) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            lx_error!(
                "core.shelld.connect_failed",
                &format!("{e}"),
                sock = shelld_sock.display()
            );
            return;
        }
    };

    // Bootstrap the full 9-grid: reattach every surviving shelld
    // session (full bytelog history replays on attach), then fill the
    // remaining slots with fresh sessions — same contract as the
    // standalone marspot's startup (src/main.rs).
    //
    // Compute the layout BEFORE touching shelld so attach/new_session
    // get the real cell dimensions from the start.  Attaching at a
    // placeholder size and resizing afterwards would replay the whole
    // bytelog into the wrong grid and then churn it through a reflow
    // for nothing (and, pre-reflow, used to destroy it outright).
    let layout_mode = LayoutMode::Nine;
    let n_sessions = layout_mode.cells();
    let (boot_cols, boot_rows) = {
        let (cell_w, cell_h) = renderer.cell_dims();
        let (lc, lr) = layout_mode.dims();
        let boot = Layout::build(
            w_phys,
            h_phys,
            0.0, // sidebar starts collapsed
            HEADER_PT * scale,
            CELL_TITLE_PT * scale,
            lc,
            lr,
            cell_w,
            cell_h,
        );
        (boot.cells[0].cols, boot.cells[0].rows)
    };
    let mut panes: Vec<Pane> = Vec::with_capacity(n_sessions);
    // session_id → custom title, harvested from shelld so freshly-
    // booted cores repopulate their per-pane title map.  Empty for a
    // brand-new shelld; populated below from `list_sessions`
    // responses on both the l3_mode and fallback paths.
    let mut session_titles: std::collections::HashMap<u64, String> =
        std::collections::HashMap::new();

    // Per-session L3 is now the DEFAULT (target #4 step 6): one L3 process
    // per cell, each owning its own session process + shm grid in its own
    // address space.  L2 owns session *assignment* so the N children never
    // race for one session: reuse the live sessions first (bytelog replay
    // on attach), then `create_session` for the rest, handing each L3 its
    // exact id.  `MARSPOT_L3=0` opts back out to the in-process shelld grid
    // (kept as the escape hatch + the fallback if every L3 spawn fails).
    let l3_mode = std::env::var("MARSPOT_L3").as_deref() != Ok("0");
    if l3_mode {
        // A core update lands the new session engine in pending/; promote
        // it into current/ before spawning so each L3 (resolved as core's
        // sibling = current/marspot-session in an installed app) boots the
        // new binary in lockstep with this core.
        match marspot::updater::promote_pending_session() {
            Ok(true) => lx_event!(
                "SESSION_PROMOTE",
                "promoted staged marspot-session → current/ at boot"
            ),
            Ok(false) => {}
            Err(e) => lx_error!("core.promote.boot_failed", &format!("{e}")),
        }
        let raw_list = client.list_sessions().unwrap_or_else(|e| {
            lx_warn!(
                "core.shelld.list_sessions_failed",
                "starting fresh",
                err = format!("{e}")
            );
            Vec::new()
        });
        // Dev fingerprint: a freshly-restarted shelld returns []; a
        // surviving shelld returns the live session list.  If we see
        // 0 sessions here right after a dual-core swap, that's the
        // 2026-06-15 symptom — shelld got SIGTERM'd between cores,
        // every existing claudecode died via Pty::Drop, and this
        // boot is now creating a fresh 9-grid from scratch.
        //
        // SORT BY session_id ASCENDING — shelld's HashMap iteration
        // order is unspecified, so on every dual-core swap the same
        // 9 sessions would land at randomly shuffled pane positions
        // (user-visible: "I was working in pane 5, now I'm in pane 8").
        // Monotonic session_id means session 1 lands in pane 1
        // forever, session 9 lands in pane 9 forever.
        for s in &raw_list {
            if !s.title.is_empty() {
                session_titles.insert(s.session_id, s.title.clone());
            }
        }
        let mut alive_ids: Vec<u64> = raw_list
            .iter()
            .filter(|s| s.alive)
            .map(|s| s.session_id)
            .collect();
        alive_ids.sort();
        let mut dead_ids: Vec<u64> = raw_list
            .iter()
            .filter(|s| !s.alive)
            .map(|s| s.session_id)
            .collect();
        dead_ids.sort();
        lx_event!(
            "core.shelld.session_inventory",
            "list_sessions returned at boot",
            total = raw_list.len(),
            alive = alive_ids.len(),
            dead = dead_ids.len(),
            alive_ids = format!("{:?}", alive_ids),
            dead_ids = format!("{:?}", dead_ids),
            want = n_sessions
        );
        let mut ids: Vec<u64> = alive_ids.into_iter().take(n_sessions).collect();
        while ids.len() < n_sessions {
            match client.create_session(boot_cols, boot_rows, "") {
                Ok(id) => ids.push(id),
                Err(e) => {
                    lx_error!("core.shelld.create_session_failed", &format!("{e}"));
                    break;
                }
            }
        }
        for id in ids {
            match spawn_l3_pane(boot_cols, boot_rows, id, &event_tx) {
                Ok(pane) => panes.push(pane),
                Err(e) => lx_error!(
                    "core.spawn.l3_boot_failed",
                    &format!("{e}"),
                    session = id
                ),
            }
        }
        if panes.is_empty() {
            lx_warn!(
                "core.boot.l3_empty",
                "no L3 panes spawned — falling back to shelld panes"
            );
        }
    }

    // Normal boot (also the L3 fallback): reattach every surviving
    // shelld session, then fill the rest with fresh ones.  Skipped once
    // an L3 pane is up — step 3 renders exactly that one pane.
    if panes.is_empty() {
        let existing: Vec<marspot::shelld_proto::SessionInfo> = client
            .list_sessions()
            .unwrap_or_else(|e| {
                lx_warn!(
                    "core.shelld.list_sessions_failed",
                    "starting fresh",
                    err = format!("{e}")
                );
                Vec::new()
            })
            .into_iter()
            .filter(|s| s.alive)
            .collect();
        for info in existing.iter().take(n_sessions) {
            if !info.title.is_empty() {
                session_titles.insert(info.session_id, info.title.clone());
            }
            match client.attach(info.session_id, boot_cols, boot_rows) {
                Ok(s) => panes.push(Pane::new_shelld(s)),
                Err(e) => {
                    lx_error!(
                        "core.shelld.attach_failed",
                        &format!("{e}"),
                        session = info.session_id
                    );
                }
            }
        }
        while panes.len() < n_sessions {
            match client.new_session(boot_cols, boot_rows, "") {
                Ok(s) => panes.push(Pane::new_shelld(s)),
                Err(e) => {
                    lx_error!("core.shelld.new_session_failed", &format!("{e}"));
                    break;
                }
            }
        }
    }
    if panes.is_empty() {
        lx_error!("core.boot.no_sessions", "no sessions could be created — exiting");
        return;
    }

    // Repopulate per-pane custom titles from the shelld-side metadata
    // collected during boot.  Without this, every silent update reset
    // the user's custom labels back to None — see the SET_TITLE
    // round-trip on shelld for the persistence path.
    let custom_titles_init: Vec<Option<String>> = panes
        .iter()
        .map(|p| {
            p.shelld_session_id()
                .and_then(|sid| session_titles.get(&sid).cloned())
                .filter(|t| !t.is_empty())
        })
        .collect();
    let mut app = CoreApp {
        renderer,
        // Placeholder; `rebuild_layout` below builds the real one
        // (it needs `self` assembled to read mode + chrome state).
        layout: Layout::build(
            w_phys,
            h_phys,
            0.0,
            HEADER_PT * scale,
            CELL_TITLE_PT * scale,
            3,
            3,
            8.0,
            16.0,
        ),
        panes,
        focused_idx: 0,
        custom_titles: custom_titles_init,
        editing_title: None,
        title_edit_buffer: String::new(),
        pane_badges: std::collections::HashMap::new(),
        pending_to_shell: Vec::new(),
        pane_sessions: std::collections::HashMap::new(),
        esc_history: std::collections::VecDeque::with_capacity(3),
        selection: None,
        selection_dragging: false,
        layout_mode,
        layout_picker_open: false,
        sidebar_collapsed: true,
        ime_preedit: String::new(),
        client,
        w_phys,
        h_phys,
        scale,
        needs_render: true,
        all_exited: false,
        last_caret_sent: None,
        l3_mode,
        event_tx: event_tx.clone(),
    };
    app.rebuild_layout();
    lx_info!(
        "core.layout.ready",
        "initial layout built",
        mode = format!("{:?}", app.layout_mode),
        panes = app.panes.len(),
        cols = app.layout.cells[0].cols,
        rows = app.layout.cells[0].rows
    );

    // Bring up the shell ↔ core control socket inherited as fd 3.
    let control_fd: RawFd = std::env::var(ENV_CONTROL_FD)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_CONTROL_FD);
    lx_info!(
        "core.control.take_fd",
        "taking control socket",
        fd = control_fd
    );
    let control_stream = unsafe { UnixStream::from_raw_fd(control_fd) };
    let reader_stream = control_stream
        .try_clone()
        .expect("[core] try_clone control_stream");
    let mut control_writer = control_stream;
    let reader_tx = event_tx.clone();
    std::thread::spawn(move || reader_loop(reader_stream, reader_tx));

    // SIGUSR2 → per-session silent-update trigger (behind MARSPOT_L3=1 it
    // swaps idle L3 panes; a no-op otherwise).
    install_swap_trigger(event_tx.clone());

    lx_event!(
        "CORE_LOOP",
        "entering event loop (event-driven, no fixed cadence)"
    );

    let start = Instant::now();
    let mut frame: u64 = 0;
    // Track when we last saw forward progress (a pump that delivered
    // bytes / a render).  `last_progress` only advances inside the
    // hot path; CORE_EXIT logs how stale this is at exit, which is
    // the missing forensic anchor from the 2026-06-15 incident — we
    // could see `CORE_EXIT control socket closed by shell` but not
    // "was the loop alive for the last 30 s or hung that whole time".
    let mut last_progress = Instant::now();
    // Bytes pumped through the L3 mirror / shelld since boot.  Surfaces
    // at CORE_EXIT so a 0 here means "we never received anything",
    // distinguishing an early-aborted boot from a normal teardown.
    let mut bytes_pumped_total: u64 = 0;
    let mut first_tick = true;
    // Frame-interval cap: don't render faster than ~120 Hz.  Without
    // this, an app emitting bursty escape sequences (IME setMarkedText
    // every keystroke, claudecode painting prompts at full tilt) makes
    // the main loop spin into render-per-event mode, blowing through
    // both CPU and GPU on a sequence of frames the display can't show.
    // M-series GPUs draw the marspot grid in ~2-3 ms so a 120-Hz cap
    // leaves headroom over a 60-Hz monitor's vsync without leaving
    // perceptible input lag (one wasted frame = 8 ms ≈ key-to-photon
    // floor anyway).  Capped frames are NOT dropped — `needs_render`
    // stays true and the next loop iter renders as soon as the cap
    // elapses.  Pairs with the recv_timeout below: when a render is
    // gated, the loop wakes in ≤ FRAME_MIN_INTERVAL instead of the
    // 1-second idle timeout, so the deferred frame lands within ~8 ms.
    const FRAME_MIN_INTERVAL_MS: u64 = 8;
    let frame_min_interval = Duration::from_millis(FRAME_MIN_INTERVAL_MS);
    let mut last_render_at = Instant::now() - frame_min_interval;
    'main: loop {
        let first = if first_tick {
            first_tick = false;
            event_rx.try_recv().ok()
        } else {
            // When a render is gated by the frame-interval cap, wake
            // the loop in ≤ FRAME_MIN_INTERVAL to flush the deferred
            // frame; otherwise stay event-driven at the 1 s idle
            // timeout so CPU at rest stays near zero.
            let recv_timeout = if app.needs_render {
                let since = last_render_at.elapsed();
                if since < frame_min_interval {
                    frame_min_interval - since
                } else {
                    Duration::from_millis(0)
                }
            } else {
                Duration::from_secs(1)
            };
            match event_rx.recv_timeout(recv_timeout) {
                Ok(ev) => Some(ev),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break 'main,
            }
        };
        // Drain pending control-socket events.  Attach coalescing:
        // keep only the latest SurfaceAttach (resize fires fast in a
        // live drag — old attach payloads are stale by the time we
        // get to render); liveness frames are echoed within the same
        // drain pass.
        let mut pending_attach: Option<(u32, u32, f64, f64, f64)> = None;
        let mut to_ack: Vec<(MsgType, Vec<u8>)> = Vec::new();
        let mut closed = false;
        let process = |app: &mut CoreApp,
                           ev: CoreEvent,
                           pending_attach: &mut Option<(u32, u32, f64, f64, f64)>,
                           to_ack: &mut Vec<(MsgType, Vec<u8>)>,
                           closed: &mut bool| {
            match ev {
                CoreEvent::Key(event, mods) => app.key(event, mods),
                CoreEvent::MouseDown(x, y, mods) => app.mouse_down(x, y, mods),
                CoreEvent::MouseDrag(x, y) => app.mouse_drag(x, y),
                CoreEvent::MouseUp => app.mouse_up(),
                CoreEvent::Scroll(dy, precise) => app.scroll(dy, precise),
                CoreEvent::Focus(focused) => {
                    app.renderer.set_window_focused(focused);
                    app.needs_render = true;
                }
                CoreEvent::Preedit(text) => app.preedit(text),
                CoreEvent::Closed => *closed = true,
                CoreEvent::Resize(_new_id, _new_w, _new_h, _new_scale) => {
                    // Legacy PROTO_VERSION=1 path — kept as a tolerance
                    // hook but the dual-buffer shell only sends
                    // SurfaceAttach now.  Silently drop.
                }
                CoreEvent::SurfaceAttach(f_id, b_id, new_w, new_h, new_scale) => {
                    *pending_attach = Some((f_id, b_id, new_w, new_h, new_scale));
                }
                CoreEvent::PumpShelld => {
                    app.needs_render = true;
                }
                CoreEvent::L3Ready => {
                    // Just needs to wake the loop; `pump_all` re-reads
                    // the L3 mirror and flips needs_render if it changed.
                    app.needs_render = true;
                }
                CoreEvent::SwapIdleL3 => app.swap_idle_l3(),
                CoreEvent::Hello(v) => {
                    to_ack.push((MsgType::HelloAck, encode_hello_ack(v.min(PROTO_VERSION))));
                }
                CoreEvent::Ping(nonce) => {
                    to_ack.push((MsgType::Pong, encode_pong(nonce)));
                }
                CoreEvent::PaneBadge(sid, text) => {
                    app.set_pane_badge(sid, text);
                }
                CoreEvent::PaneSessionBegin(sid, caps) => {
                    app.pane_session_begin(sid, caps);
                }
                CoreEvent::PaneSessionEnd(sid) => {
                    app.pane_session_end(sid);
                }
            }
        };
        if let Some(ev) = first {
            process(&mut app, ev, &mut pending_attach, &mut to_ack, &mut closed);
        }
        while let Ok(ev) = event_rx.try_recv() {
            process(&mut app, ev, &mut pending_attach, &mut to_ack, &mut closed);
        }
        if closed {
            // The dominant CORE_EXIT path in real life — and the one
            // whose context was missing in the 2026-06-15 incident:
            // we'd see this line and nothing else.  Surface every
            // forensic anchor we have so a future "why did core 35078
            // die" question has answers without re-running.
            lx_event!(
                "CORE_EXIT",
                "control socket closed by shell; exiting event loop",
                reason = "control_eof",
                frames = frame,
                uptime_s = start.elapsed().as_secs(),
                bytes_pumped = bytes_pumped_total,
                last_progress_age_ms = last_progress.elapsed().as_millis() as u64,
                n_panes = app.panes.len(),
                focused_idx = app.focused_idx
            );
            break 'main;
        }
        for (ty, payload) in to_ack.drain(..) {
            let f = Frame::new(ty, payload);
            if let Err(e) = f.write_to(&mut control_writer) {
                lx_error!(
                    "core.liveness.write_failed",
                    &format!("{e}"),
                    msg_type = format!("{:?}", ty)
                );
            }
        }
        // Drain frames queued from inside CoreApp event handlers
        // (mouse_down → PaneBadgeClicked, future similar paths).
        for (ty, payload) in app.pending_to_shell.drain(..) {
            let f = Frame::new(ty, payload);
            if let Err(e) = f.write_to(&mut control_writer) {
                lx_error!(
                    "core.pending_to_shell.write_failed",
                    &format!("{e}"),
                    msg_type = format!("{:?}", ty)
                );
            }
        }
        if let Some((new_front, new_back, new_w, new_h, new_scale)) = pending_attach {
            // Shell handed us a freshly-created IOSurface pair at the
            // new size (resize / restart / pending-update spawn).
            // Look up both, rebuild both textures, rebuild the layout,
            // and immediately render into slot 0 + ack
            // SurfaceReady(new_front) so the shell can install + swap
            // the presenter to the new pair.
            let f_surf = IOSurface::lookup(new_front);
            let b_surf = IOSurface::lookup(new_back);
            match (f_surf, b_surf) {
                (Some(fs), Some(bs)) => {
                    fs.increment_use();
                    bs.increment_use();
                    let new_tex_f = fs.make_metal_texture(app.renderer.device());
                    let new_tex_b = bs.make_metal_texture(app.renderer.device());
                    match (new_tex_f, new_tex_b) {
                        (Ok(tf), Ok(tb)) => {
                            // Release the old pair (decrement_use balances
                            // the two increments we did at boot or in the
                            // previous attach).
                            surfaces[0].decrement_use();
                            surfaces[1].decrement_use();
                            surfaces = [fs, bs];
                            target_tex = [tf, tb];
                            writing_idx = 0;
                            app.w_phys = new_w;
                            app.h_phys = new_h;
                            app.scale = new_scale;
                            app.rebuild_layout();
                            // Render the latest content into slot 0 so
                            // the SurfaceReady ack reflects a real frame.
                            app.pump_all();
                            let _ = app.render(&target_tex[writing_idx]);
                            let ack = Frame::new(
                                MsgType::SurfaceReady,
                                encode_surface_ready(surfaces[writing_idx].id()),
                            );
                            if let Err(e) = ack.write_to(&mut control_writer) {
                                lx_error!(
                                    "core.surface_ready.write_failed",
                                    &format!("{e}")
                                );
                            }
                            // Next render writes the other half.
                            writing_idx = 1 - writing_idx;
                        }
                        (tf, tb) => {
                            if tf.is_err() {
                                lx_error!(
                                    "core.attach.metal_texture_front_failed",
                                    &format!("{:?}", tf.err())
                                );
                            }
                            if tb.is_err() {
                                lx_error!(
                                    "core.attach.metal_texture_back_failed",
                                    &format!("{:?}", tb.err())
                                );
                            }
                            fs.decrement_use();
                            bs.decrement_use();
                        }
                    }
                }
                _ => {
                    lx_warn!(
                        "core.attach.surface_lookup_nil",
                        "IOSurfaceLookup returned nil; dropping",
                        front_id = new_front,
                        back_id = new_back
                    );
                }
            }
        }
        let pumped = app.pump_all();
        if pumped > 0 {
            bytes_pumped_total = bytes_pumped_total.saturating_add(pumped as u64);
            last_progress = Instant::now();
        }
        if app.all_exited {
            lx_event!(
                "CORE_EXIT",
                "all sessions exited; exiting cleanly",
                frames = frame,
                uptime_s = start.elapsed().as_secs(),
                bytes_pumped = bytes_pumped_total,
                last_progress_age_ms = last_progress.elapsed().as_millis() as u64,
                n_panes = app.panes.len(),
                focused_idx = app.focused_idx
            );
            break 'main;
        }

        // Frame-interval cap: defer this frame if we just rendered
        // < FRAME_MIN_INTERVAL ago.  needs_render stays true so the
        // next loop iteration tries again — and the loop's
        // recv_timeout above is set to wake us inside the cap window,
        // so the deferred frame lands within ~8 ms, not 1 s.
        let render_gated_by_cap =
            app.needs_render && last_render_at.elapsed() < frame_min_interval;
        if app.needs_render && !render_gated_by_cap {
            // Double-buffer: render into the back slot
            // (`writing_idx`).  `render_layout_to_texture` calls
            // `waitUntilCompleted`, so the moment we return here the
            // surface bytes are settled and safe for the shell to
            // sample — that's what makes `SurfaceReady` the dual-
            // buffer race fix: we only ever flip to a slot the GPU
            // has already finished.
            let render_t0 = Instant::now();
            last_render_at = render_t0;
            let caret = app.render(&target_tex[writing_idx]);
            // Sampled per-frame DEBUG.  1/8 keeps a ~7-Hz heartbeat on
            // a busy display (60 Hz cap) without flooding when the
            // user runs `MARSPOT_LOG_CORE=debug` to investigate latency
            // / dropped frames.  surface_id ties it to the IOSurface
            // lifecycle on the shell side; dur_us is the actual GPU
            // wait, the dominant time cost in a frame.
            lx_debug_sampled!(
                "render.frame",
                8,
                "frame rendered",
                frame = frame,
                writing_idx = writing_idx,
                surface_id = surfaces[writing_idx].id(),
                dur_us = render_t0.elapsed().as_micros() as u64,
                n_panes = app.panes.len(),
                focused_idx = app.focused_idx
            );
            // Per-frame ack — the just-completed surface ID.  In v=2
            // this replaces the empty-payload `FrameRendered` poke:
            // shell uses the id to flip its presenter's `current_idx`,
            // then presents.  Same one frame round-trip the old path
            // had, but the present now samples a guaranteed-finished
            // surface instead of racing the writing one.
            let ack = Frame::new(
                MsgType::SurfaceReady,
                encode_surface_ready(surfaces[writing_idx].id()),
            );
            if let Err(e) = ack.write_to(&mut control_writer) {
                lx_error!("core.surface_ready.write_failed", &format!("{e}"));
            }
            // FrameRendered is the legacy v=1 wake.  A v=2 shell
            // already woke on SurfaceReady, so this is redundant for
            // a same-version shell.  An OLD shell paired with this
            // NEW core (rare — possible after a botched silent
            // update) only listens for FrameRendered, though, so we
            // keep emitting it for compatibility.  No-op on the v=2
            // shell side (handler just sets frame_pending again).
            let fr = Frame::new(MsgType::FrameRendered, Vec::new());
            if let Err(e) = fr.write_to(&mut control_writer) {
                lx_error!("core.frame_rendered.write_failed", &format!("{e}"));
            }
            // Flip: next render writes the other half.
            writing_idx = 1 - writing_idx;
            // Publish the focused-pane caret so the shell can anchor
            // the IME candidate window.  Dedupe — an idle cursor must
            // not stream identical frames at render cadence.
            if app.last_caret_sent != Some(caret) {
                app.last_caret_sent = Some(caret);
                let f = Frame::new(MsgType::CaretRect, encode_caret_rect(caret));
                if let Err(e) = f.write_to(&mut control_writer) {
                    lx_error!("core.caret_rect.write_failed", &format!("{e}"));
                }
            }
        }

        frame += 1;
        if frame.is_multiple_of(300) {
            let t = start.elapsed().as_secs_f64();
            let p = &app.panes[app.focused_idx];
            let state = match p.session().state() {
                SessionState::Active => "active",
                SessionState::Idle => "idle",
                SessionState::Exited => "exited",
            };
            lx_debug!(
                "core.heartbeat",
                "periodic heartbeat",
                frame = frame,
                t_s = format!("{t:.1}"),
                panes = app.panes.len(),
                focused = app.focused_idx,
                state = state,
                cols = p.session().grid().cols(),
                rows = p.session().grid().rows()
            );
        }
    }
}

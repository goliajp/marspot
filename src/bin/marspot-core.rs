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
    decode_resize, decode_scroll, decode_selection_text, encode_caret_rect, encode_hello_ack,
    encode_pong,
    encode_surface_ready, mods_to_struct, wire_to_event, Frame, MsgType, DEFAULT_CONTROL_FD,
    ENV_CONTROL_FD, ENV_SURFACE_HEIGHT, ENV_SURFACE_ID, ENV_SURFACE_SCALE, ENV_SURFACE_WIDTH,
    PROTO_VERSION,
};
use marspot::shelld_client::ShelldClient;
use marspot::{lx_debug, lx_error, lx_event, lx_info, lx_warn};
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
    Resize(u32, f64, f64, f64),
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
/// verbatim.
struct CoreApp {
    renderer: MetalRenderer,
    layout: Layout,
    panes: Vec<Pane>,
    focused_idx: usize,
    custom_titles: Vec<Option<String>>,
    editing_title: Option<usize>,
    title_edit_buffer: String,
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
            // immediately, ahead of the PTY round trip.
            let mut predicted = false;
            for &b in bytes.as_ref() {
                if session.terminal_mut().predict_byte(b) {
                    predicted = true;
                }
            }
            if predicted {
                self.needs_render = true;
            }
        }
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
        for (i, p) in self.panes.iter_mut().enumerate() {
            let n = p.pump();
            total += n;
            let pushed = p.drain_scroll_push_delta();
            let has_bytes = n > 0;
            if let Some(sel) = self.selection.as_mut() {
                if sel.session_idx == i {
                    if pushed > 0 {
                        let bump = pushed as u32;
                        sel.anchor.1 = sel.anchor.1.saturating_add(bump);
                        sel.focus.1 = sel.focus.1.saturating_add(bump);
                    }
                    if has_bytes && !self.selection_dragging {
                        self.selection = None;
                    }
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
                let mut v =
                    p.view(i == focused, titles.get(i).map(|s| s.as_str()).unwrap_or(""));
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

    let surface_id: u32 = env_required(ENV_SURFACE_ID);
    let w_phys: f64 = env_required(ENV_SURFACE_WIDTH);
    let h_phys: f64 = env_required(ENV_SURFACE_HEIGHT);
    let scale: f64 = env_required(ENV_SURFACE_SCALE);

    lx_event!(
        "SURFACE_ATTACH",
        "attaching IOSurface",
        surface_id = surface_id,
        w_phys = w_phys,
        h_phys = h_phys,
        scale = scale
    );

    let mut surface = IOSurface::lookup(surface_id)
        .unwrap_or_else(|| panic!("[core] IOSurfaceLookup({surface_id}) returned nil"));
    surface.increment_use();

    let renderer = MetalRenderer::new_headless().expect("[core] MetalRenderer::new_headless");
    let mut target_tex: objc2::rc::Retained<ProtocolObject<dyn MTLTexture>> = surface
        .make_metal_texture(renderer.device())
        .expect("[core] make_metal_texture");

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

    let n = panes.len();
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
        custom_titles: vec![None; n],
        editing_title: None,
        title_edit_buffer: String::new(),
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
    let mut first_tick = true;
    'main: loop {
        let first = if first_tick {
            first_tick = false;
            event_rx.try_recv().ok()
        } else {
            match event_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(ev) => Some(ev),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break 'main,
            }
        };
        // Drain pending control-socket events.  Resize coalescing:
        // keep only the latest Resize (see Step 4 notes); liveness
        // frames are echoed within the same drain pass.
        let mut pending_resize: Option<(u32, f64, f64, f64)> = None;
        let mut to_ack: Vec<(MsgType, Vec<u8>)> = Vec::new();
        let mut closed = false;
        let process = |app: &mut CoreApp,
                           ev: CoreEvent,
                           pending_resize: &mut Option<(u32, f64, f64, f64)>,
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
                CoreEvent::Resize(new_id, new_w, new_h, new_scale) => {
                    *pending_resize = Some((new_id, new_w, new_h, new_scale));
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
            }
        };
        if let Some(ev) = first {
            process(&mut app, ev, &mut pending_resize, &mut to_ack, &mut closed);
        }
        while let Ok(ev) = event_rx.try_recv() {
            process(&mut app, ev, &mut pending_resize, &mut to_ack, &mut closed);
        }
        if closed {
            lx_event!(
                "CORE_EXIT",
                "control socket closed by shell; exiting event loop"
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
        if let Some((new_id, new_w, new_h, new_scale)) = pending_resize {
            // Shell hands us a freshly-created IOSurface at the new
            // size; rebuild the render target + layout, then ack with
            // SurfaceReady so the shell can swap its presenter.
            let new_surface = match IOSurface::lookup(new_id) {
                Some(s) => {
                    s.increment_use();
                    Some(s)
                }
                None => {
                    lx_warn!(
                        "core.resize.surface_lookup_nil",
                        "IOSurfaceLookup returned nil; dropping",
                        surface_id = new_id
                    );
                    None
                }
            };
            if let Some(new_surface) = new_surface {
                match new_surface.make_metal_texture(app.renderer.device()) {
                    Ok(new_tex) => {
                        target_tex = new_tex;
                        surface.decrement_use();
                        surface = new_surface;
                        app.w_phys = new_w;
                        app.h_phys = new_h;
                        app.scale = new_scale;
                        app.rebuild_layout();
                        // Render the latest content into the new
                        // surface so the SurfaceReady ack is honest.
                        app.pump_all();
                        let _ = app.render(&target_tex);
                        let ack =
                            Frame::new(MsgType::SurfaceReady, encode_surface_ready(new_id));
                        if let Err(e) = ack.write_to(&mut control_writer) {
                            lx_error!("core.surface_ready.write_failed", &format!("{e}"));
                        }
                    }
                    Err(e) => {
                        lx_error!("core.resize.metal_texture_failed", &format!("{e}"));
                    }
                }
            }
        }
        app.pump_all();
        if app.all_exited {
            lx_event!("CORE_EXIT", "all sessions exited; exiting cleanly");
            break 'main;
        }

        if app.needs_render {
            let caret = app.render(&target_tex);
            // Tell the shell a complete frame is in the IOSurface so it
            // presents now — replaces its blind ~60 fps present timer (idle
            // CPU + occasional torn read from sampling mid-render).  Sent
            // after render() returns; the cross-process + thread-wake
            // latency before the shell actually samples comfortably exceeds
            // the GPU's sub-ms write completion, so the present sees a
            // settled surface.
            let fr = Frame::new(MsgType::FrameRendered, Vec::new());
            if let Err(e) = fr.write_to(&mut control_writer) {
                lx_error!("core.frame_rendered.write_failed", &format!("{e}"));
            }
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

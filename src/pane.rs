//! Single-session pane — the unit of single-terminal behavior shared
//! by every binary in this workspace.
//!
//! `mcli` holds exactly one `Pane`. `marspot` (the multi-session
//! container) holds a `Vec<Pane>` plus a `Layout` that assigns cell
//! rects and dispatches the focused pane. The deliberate consequence:
//! any single-session feature — key handling, scroll-into-scrollback,
//! resize-with-chrome-inset, future selection / vim mode / search /
//! find — lands in `Pane` and is automatically picked up by every
//! container.
//!
//! This is the "structure and individual cleanly separated" rule
//! (steel-cement-stone, the steel layer). `Session` / `Terminal` /
//! `Renderer` are the stone (cross-binary, semver-stable building
//! blocks). `Pane` is the steel (project-wide reusable, domain-aware
//! but not multi-session-aware). `Mcli` and `Marspot` are the cement
//! that wires panes into a window.

use std::os::unix::net::UnixStream;
use std::process::Child;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crate::grid::{Cell, Grid};
use crate::grid_shm::{
    GridShmReader, FLAG_APP_CURSOR_KEYS, FLAG_BRACKETED_PASTE, FLAG_CURSOR_VISIBLE,
    FLAG_MOUSE_SGR, FLAG_MOUSE_TRACKING,
};
use crate::input::{key_event_to_bytes, MarspotKeyEvent, Modifiers};
use crate::render::SessionView;
use crate::session::{Session, SessionState};
use crate::shell_proto::{
    encode_get_selection_text, encode_grid_resize, encode_grid_scroll, encode_key_event,
    encode_paste, encode_search_cancel, encode_search_more, encode_search_scrollback,
    event_to_wire, Frame, MsgType,
};
use crate::tools::search_bar::SearchBar;
use crate::tools::search_list::SearchList;
use crate::terminal::Terminal;

/// One pane's backend: a locally forked PTY (the legacy / mcli /
/// bench path) or a shelld-managed session (marspot's live-update
/// path).  Same observable interface; the GUI / Pane don't need to
/// branch on which is in use except where shelld-specific affordances
/// (session id, reattach) come into play.
pub enum PaneBackend {
    Local(Session),
    /// A per-session L3 process (`marspot-session`, target #4) that owns
    /// the PTY parser/terminal in its own address space and publishes
    /// its visible grid into shared memory.  This process (L2) holds only
    /// a synthetic mirror grid filled from the shm snapshot, plus the
    /// control socket to forward keystrokes.  Behind `MARSPOT_L3=1`;
    /// the in-process backends above stay the default.
    L3(L3Conn),
    /// RFC-004 B.2 — a slot whose session could not be brought up at
    /// boot (spawn/reattach/resurrect failure).  Holds the session id
    /// so the slot NEVER compacts away: the sid survives into the next
    /// `save_session_state`, the on-disk `sessions/<sid>/` dir stays
    /// eligible for resurrection, and the existing revive-on-keystroke
    /// path retries a fresh L3 at the same id.  Renders a static
    /// message grid; all I/O is a no-op.
    Vacant(VacantPane),
}

/// RFC-004 B.2 — the inert backend behind a slot that failed to
/// assemble.  See `PaneBackend::Vacant`.
pub struct VacantPane {
    session_id: u64,
    grid: Grid,
    /// RFC-006 — a dormant placeholder: the slot a pane left behind
    /// when it was MOVED to another window.  Holds the layout open
    /// but is not a session in any sense: it never revives on a
    /// keystroke (click only), counts as neither live nor
    /// resurrectable, and persists as layout (SavedPane flags bit 0).
    dormant: bool,
    /// A spawn for this slot is in flight.
    ///
    /// "Starting" is not a peer state to "vacant" — it *is* a vacant
    /// slot (no live session behind it) that additionally has work
    /// coming.  Modelling it as a flag rather than a second backend
    /// variant keeps that relationship visible, and keeps the twenty
    /// match arms that already handle `Vacant` correct by construction.
    ///
    /// Two things change while it holds: the slot says "starting"
    /// instead of "unavailable", and `is_exited` reports false so the
    /// revive-on-keystroke path can't fire a second spawn on top of the
    /// one already running.
    ///
    /// It is deliberately time-boxed — see `pending_since`.
    pending: bool,
    /// When the in-flight spawn started, for the pending state only.
    ///
    /// Suppressing `is_exited` is what stops a double spawn, but it also
    /// means the revive-on-keystroke path — the one affordance a user has
    /// for waking a dead slot — is disabled while it holds.  If the
    /// worker never reports back (its thread panicked, the event was
    /// dropped), the slot would sit at "starting…" forever with the
    /// guard locking out the only escape.  So the guard expires: past
    /// `PENDING_MAX`, the slot reports exited again and a keystroke can
    /// retry.  A late-arriving result is still adopted normally — the
    /// deadline only re-opens the escape hatch, it doesn't cancel
    /// anything.
    pending_since: Instant,
}

/// How long a slot may claim "a spawn is in flight" before a keystroke
/// is allowed to start another.  Generously past the 10 s
/// `wait_and_connect` budget a spawn can legitimately take, so a slow
/// boot is never cut short.
const PENDING_MAX: Duration = Duration::from_secs(30);

impl VacantPane {
    pub fn new(session_id: u64, cols: u16, rows: u16) -> Self {
        let mut grid = Grid::new(cols.max(20), rows.max(3));
        Self::paint_message(&mut grid, session_id, false);
        Self {
            session_id,
            grid,
            dormant: false,
            pending: false,
            pending_since: Instant::now(),
        }
    }

    /// A slot whose session is being spawned right now.
    pub fn new_pending(session_id: u64, cols: u16, rows: u16) -> Self {
        let mut grid = Grid::new(cols.max(20), rows.max(3));
        Self::paint_message(&mut grid, session_id, true);
        Self {
            session_id,
            grid,
            dormant: false,
            pending: true,
            pending_since: Instant::now(),
        }
    }

    /// RFC-006 — the placeholder a moved-out pane leaves behind.
    pub fn new_dormant(cols: u16, rows: u16) -> Self {
        let mut grid = Grid::new(cols.max(20), rows.max(3));
        let msg = "empty — click to start a shell";
        let row = (grid.rows() / 2).min(grid.rows().saturating_sub(1));
        let start = (grid.cols().saturating_sub(msg.len() as u16)) / 2;
        for (i, ch) in msg.chars().enumerate() {
            let col = start + i as u16;
            if col >= grid.cols() { break; }
            let mut cell = Cell::default();
            cell.ch = ch;
            grid.set_cell(col, row, cell);
        }
        Self {
            session_id: 0,
            grid,
            dormant: true,
            pending: false,
            pending_since: Instant::now(),
        }
    }

    pub fn is_dormant(&self) -> bool {
        self.dormant
    }

    /// A spawn is in flight *and* still within its deadline.  Past the
    /// deadline the slot behaves like a plain vacant one so the user can
    /// retry — see `pending_since`.
    pub fn is_pending(&self) -> bool {
        self.pending && self.pending_since.elapsed() < PENDING_MAX
    }

    fn paint_message(grid: &mut Grid, session_id: u64, pending: bool) {
        let msg = if pending {
            format!("session {session_id} starting…")
        } else {
            format!("session {session_id} unavailable — press any key to retry")
        };
        let row = 1u16.min(grid.rows().saturating_sub(1));
        for (i, ch) in msg.chars().enumerate() {
            let col = 2 + i as u16;
            if col >= grid.cols() {
                break;
            }
            grid.set_cell(col, row, Cell { ch, ..Default::default() });
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let mut grid = Grid::new(cols.max(20), rows.max(3));
        Self::paint_message(&mut grid, self.session_id, self.pending);
        self.grid = grid;
    }
}

impl PaneBackend {
    /// In-process terminal — **only** the local/shelld backends.  An L3
    /// pane has no `Terminal` in this address space (it lives in the
    /// session process); read its grid + modes via `grid()` /
    /// `cursor_visible()` / `cursor_key_application_mode()` /
    /// `bracketed_paste_mode()` instead.  Calling this on an L3 backend
    /// is a bug and panics.
    pub fn terminal(&self) -> &Terminal {
        match self {
            PaneBackend::Local(s) => s.terminal(),
            PaneBackend::L3(_) | PaneBackend::Vacant(_) => {
                unreachable!("L3/Vacant pane has no in-process Terminal")
            }
        }
    }

    pub fn terminal_mut(&mut self) -> &mut Terminal {
        match self {
            PaneBackend::Local(s) => &mut s.terminal,
            PaneBackend::L3(_) | PaneBackend::Vacant(_) => {
                unreachable!("L3/Vacant pane has no in-process Terminal")
            }
        }
    }

    /// Visible grid for the renderer.  Local/shelld read their own
    /// terminal; L3 reads the synthetic mirror last filled from shm.
    pub fn grid(&self) -> &Grid {
        match self {
            PaneBackend::Local(s) => s.terminal().grid(),
            PaneBackend::L3(c) => &c.grid,
            PaneBackend::Vacant(v) => &v.grid,
        }
    }

    /// The shelld session id behind this backend, when one exists.
    /// `None` for the in-process `Local` backend (which has its own PTY
    /// and no shelld concept). Used by the close-pane handler to ask
    /// shelld to terminate the session + delete its bytelog — without
    /// that, the bytelog persists on disk and shelld keeps the session
    /// alive, so the next `list_sessions` call attaches a fresh pane
    /// onto it and replays the buffer (the "I clicked × and my old
    /// content came back" bug).
    pub fn shelld_session_id(&self) -> Option<u64> {
        match self {
            PaneBackend::Local(_) => None,
            PaneBackend::L3(c) => Some(c.shelld_session_id()),
            // A vacant slot still *names* its session — that's the
            // whole point (sid survives save/restore + revive).
            PaneBackend::Vacant(v) => Some(v.session_id),
        }
    }

    pub fn cursor_visible(&self) -> bool {
        match self {
            PaneBackend::Local(s) => s.terminal().cursor_visible(),
            PaneBackend::L3(c) => c.cursor_visible,
            PaneBackend::Vacant(_) => false,
        }
    }

    pub fn cursor_key_application_mode(&self) -> bool {
        match self {
            PaneBackend::Local(s) => s.terminal().cursor_key_application_mode(),
            PaneBackend::L3(c) => c.app_cursor_keys,
            PaneBackend::Vacant(_) => false,
        }
    }

    pub fn bracketed_paste_mode(&self) -> bool {
        match self {
            PaneBackend::Local(s) => s.terminal().bracketed_paste_mode(),
            PaneBackend::L3(c) => c.bracketed_paste,
            PaneBackend::Vacant(_) => false,
        }
    }

    /// L3-backed?  Container input routing branches on this: an L3 pane
    /// forwards the key *event* to its session process (which encodes +
    /// local-echoes), rather than encoding to PTY bytes here.
    pub fn is_l3(&self) -> bool {
        matches!(self, PaneBackend::L3(_))
    }

    /// Forward a keystroke to an L3 session over the control socket.
    /// No-op for the in-process backends (they go through `write`).
    pub fn forward_key(&mut self, event: &MarspotKeyEvent, mods: Modifiers) {
        if let PaneBackend::L3(c) = self {
            c.forward_key(event, mods);
        }
    }

    /// Forward pasted clipboard text to an L3 session.  L2 resolves the
    /// macOS pasteboard (L3 is GUI-free) and forwards the text; L3 wraps it
    /// in bracketed-paste markers per its own terminal mode and writes the
    /// PTY.  No-op for in-process backends — they paste via `write`.
    pub fn forward_paste(&mut self, text: &str) {
        if let PaneBackend::L3(c) = self {
            c.forward_paste(text);
        }
    }

    /// Forward raw bytes via the cc inject-input frame (no
    /// bracketed-paste wrap, no key encoding).  No-op for non-L3
    /// backends.  Used by L2 to relay `CoreEvent::InjectInput`.
    pub fn forward_inject_input(&mut self, bytes: &[u8]) {
        if let PaneBackend::L3(c) = self {
            c.forward_inject_input(bytes);
        }
    }

    /// Ask the pane's L3 to hold (or release) its grid.  No-op on
    /// non-L3 backends: there is no separate process to hold.
    pub fn forward_pane_hold_grid(&mut self, on: bool) {
        if let PaneBackend::L3(c) = self {
            c.forward_pane_hold_grid(on);
        }
    }

    /// C5 — send a `SearchScrollback` frame to the L3 session
    /// behind this pane.  No-op on non-L3 backends (search is
    /// File-backed scrollback only, which only L3 owns).
    pub fn forward_search_scrollback(
        &mut self,
        query_id: u32,
        case_sensitive: bool,
        max_total: u32,
        query: &str,
    ) {
        if let PaneBackend::L3(c) = self {
            c.forward_search_scrollback(query_id, case_sensitive, max_total, query);
        }
    }

    /// C5 — cancel an in-flight L3 search by query_id.
    pub fn forward_search_cancel(&mut self, query_id: u32) {
        if let PaneBackend::L3(c) = self {
            c.forward_search_cancel(query_id);
        }
    }

    /// C5 — request more hits for the in-flight search (D-phase
    /// pagination; v1 only uses direction=0 → older).
    pub fn forward_search_more(&mut self, query_id: u32, count: u32) {
        if let PaneBackend::L3(c) = self {
            c.forward_search_more(query_id, count);
        }
    }

    /// Ask an L3 session to publish its window at `view_offset`.  No-op for
    /// in-process backends, which scroll their own grid at render time.
    pub fn forward_scroll(&mut self, view_offset: u16) {
        if let PaneBackend::L3(c) = self {
            c.forward_scroll(view_offset);
        }
    }

    /// Scrollback depth for an L3 pane (from its last snapshot) — the clamp
    /// bound L2 uses when scrolling it.  `0` for in-process backends, which
    /// clamp against their own grid instead.
    /// L3-reported mouse tracking active(L3 terminal 设了 DECSET 1000/
    /// 1002/1003).L2 在 apply_scroll_lines 用来决定 bypass clamp.
    pub fn l3_mouse_tracking_active(&self) -> bool {
        match self {
            PaneBackend::L3(c) => c.mouse_tracking_active(),
            _ => false,
        }
    }

    pub fn l3_mouse_sgr_active(&self) -> bool {
        match self {
            PaneBackend::L3(c) => c.mouse_sgr_active(),
            _ => false,
        }
    }

    /// Encode a wheel event(`button_64`=wheel up,`button_65`=down,
    /// `n_ticks` 次)+ forward 到 L3 PTY via InjectInput.viewport
    /// 中心当 mouse 位置兜底.SGR vs X11 legacy 由 `mouse_sgr_active`
    /// 决定.cols/rows 由 caller 传(Pane 这层没存,layout 里有).
    pub fn l3_inject_wheel(&mut self, button_64_or_65: u8, n_ticks: u32, cols: u16, rows: u16) {
        let x = (cols.max(1) / 2 + 1) as u32;
        let y = (rows.max(1) / 2 + 1) as u32;
        let sgr = self.l3_mouse_sgr_active();
        let mut buf = Vec::with_capacity(n_ticks as usize * 16);
        for _ in 0..n_ticks {
            if sgr {
                buf.extend_from_slice(
                    format!("\x1b[<{};{};{}M", button_64_or_65, x, y).as_bytes()
                );
            } else {
                buf.extend_from_slice(b"\x1b[M");
                buf.push(button_64_or_65 + 32);
                buf.push((x.min(223) as u8) + 32);
                buf.push((y.min(223) as u8) + 32);
            }
        }
        if let PaneBackend::L3(c) = self {
            c.forward_inject_input(&buf);
        }
    }

    pub fn l3_scrollback_len(&self) -> u16 {
        match self {
            PaneBackend::L3(c) => c.scrollback_len(),
            _ => 0,
        }
    }

    /// shelld session id for an L3 pane (so the container can spawn a
    /// replacement on the same session for a silent update).  `None` for
    /// in-process backends.
    pub fn l3_session_id(&self) -> Option<u64> {
        match self {
            PaneBackend::L3(c) => Some(c.session_id()),
            _ => None,
        }
    }

    /// Process pid of the L3 worker for this pane.  Used by the
    /// fd-vault silent-update path to SIGTERM the current L3 before
    /// spawning its replacement.
    pub fn l3_pid(&self) -> Option<i32> {
        match self {
            PaneBackend::L3(c) => Some(c.child.id() as i32),
            _ => None,
        }
    }

    /// Hot-swap an L3 pane's control stream (after a reader-loop EOF
    /// → main loop reconnects).  No-op on non-L3 backends.
    pub fn swap_l3_control(
        &mut self,
        new_control: UnixStream,
        selection_rx: Receiver<(u32, String)>,
    ) {
        if let PaneBackend::L3(c) = self {
            c.swap_control(new_control, selection_rx);
        }
    }


    /// A silent-update replacement is staged but not yet promoted.
    pub fn is_l3_swapping(&self) -> bool {
        matches!(self, PaneBackend::L3(c) if c.is_swapping())
    }

    /// Stage a silent-update replacement (new binary, same session).  No-op
    /// for in-process backends.  Container spawns the `L3Spawn` and hands
    /// it here; `poll` promotes once it has replayed + published.
    pub fn begin_l3_swap(&mut self, spawn: L3Spawn) {
        if let PaneBackend::L3(c) = self {
            c.begin_swap(spawn);
        }
    }

    /// Cmd-C on an L3 pane: ask the session process for the selection text
    /// (it owns the grid + scrollback).  `None` for in-process backends —
    /// the container reads their text locally via `ui::selection_text`.
    pub fn request_selection_text(
        &mut self,
        anchor: (u16, u32),
        focus: (u16, u32),
        blockwise: bool,
    ) -> Option<String> {
        match self {
            PaneBackend::L3(c) => c.request_selection_text(anchor, focus, blockwise),
            _ => None,
        }
    }

    pub fn pump(&mut self) -> usize {
        match self {
            PaneBackend::Local(s) => s.pump(),
            // For L3 there are no PTY bytes here — "pump" means re-read
            // the shm mirror.  Return 1 on a fresh frame so the container
            // requests a redraw, 0 when nothing changed (so the 1 s
            // heartbeat doesn't force a spurious render).
            PaneBackend::L3(c) => usize::from(c.poll()),
            PaneBackend::Vacant(_) => 0,
        }
    }

    pub fn is_exited(&self) -> bool {
        match self {
            PaneBackend::Local(s) => s.is_exited(),
            PaneBackend::L3(c) => c.is_exited(),
            // Vacant = born exited: the revive-on-keystroke path is
            // exactly how a vacant slot gets its session back.  Except
            // while a spawn is already in flight — reporting exited
            // there would let the next keystroke start a second one.
            // `is_pending` expires, so a spawn that never reports back
            // can't lock the slot out of that path forever.
            PaneBackend::Vacant(v) => !v.is_pending(),
        }
    }

    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self {
            PaneBackend::Local(s) => s.write(bytes),
            // L3 input is forwarded as key events, not raw bytes; the
            // session process owns its own PTY write + response path.
            PaneBackend::L3(_) | PaneBackend::Vacant(_) => Ok(0),
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        match self {
            PaneBackend::Local(s) => s.resize(cols, rows),
            // Forward the cell-grid resize to the session process; it
            // resizes its Terminal + PTY + reflows and republishes at the
            // new dims, which the mirror picks up on the next `poll`.
            PaneBackend::L3(c) => c.forward_resize(cols, rows),
            PaneBackend::Vacant(v) => v.resize(cols, rows),
        }
    }

    pub fn state(&self) -> SessionState {
        match self {
            PaneBackend::Local(s) => s.state(),
            PaneBackend::L3(c) => {
                if c.is_exited() {
                    SessionState::Exited
                } else {
                    SessionState::Active
                }
            }
            PaneBackend::Vacant(_) => SessionState::Exited,
        }
    }

    /// Forward raw bytes into the terminal without going through the
    /// PTY.  Only meaningful for the local backend (tmux-CC dispatch
    /// path); shelld owns the byte stream end-to-end, so the
    /// container doesn't get an extracted-payload entry point here.
    /// Calling on a shelld backend is a no-op (callers can branch
    /// before; this kept lenient to ease the transition).
    pub fn feed_terminal(&mut self, bytes: &[u8]) {
        match self {
            PaneBackend::Local(s) => s.feed_terminal(bytes),
            PaneBackend::L3(_) | PaneBackend::Vacant(_) => {}
        }
    }

    /// Drain raw PTY bytes without feeding the terminal.  Same
    /// lenience as `feed_terminal`: only the local backend has a
    /// meaningful implementation.
    pub fn drain_raw(&mut self) -> Vec<u8> {
        match self {
            PaneBackend::Local(s) => s.drain_raw(),
            PaneBackend::L3(_) | PaneBackend::Vacant(_) => Vec::new(),
        }
    }
}

/// L2's handle on a per-session L3 process (`marspot-session`).
///
/// The session process owns the PTY + VT parser + the authoritative
/// `Grid` (with scrollback) in its own address space; here we hold only
/// a synthetic *mirror* of its visible window, filled from the shared-
/// memory snapshot on each `poll()`, plus the control socket to forward
/// keystrokes.  The child is killed + reaped on drop (bounded teardown).
///
/// Step 3 keeps this minimal — fixed geometry, no scrollback mirror, a
/// full re-fill per changed frame.  Efficiency (incremental fill, dirty
/// rows) and resize/scroll forwarding land in later steps; see
/// `docs/per-session-l3.md`.
/// The freshly-spawned pieces of one L3 process, assembled by the
/// container (it owns the process-launch + reader-thread glue) and handed
/// to `L3Conn` either at birth ([`L3Conn::new`]) or as a silent-update
/// replacement ([`L3Conn::begin_swap`]).  Keeping this a plain bundle lets
/// `pane.rs` stay free of `Command`/`socketpair`/thread spawning.
pub struct L3Spawn {
    pub child: Child,
    pub control: UnixStream,
    pub reader: GridShmReader,
    pub selection_rx: Receiver<(u32, String)>,
}

/// Process handle for an L3 child.  RFC-003 Amendment 7: a `Reattached`
/// variant represents an L3 that survived an L2 swap — we know its pid
/// but didn't fork it, so we can't `try_wait` (kill 0 instead) and we
/// must NOT kill it on Drop (the user, not us, decides retirement).
pub enum L3Process {
    Spawned(Child),
    Reattached { pid: i32 },
}

impl L3Process {
    pub fn id(&self) -> u32 {
        match self {
            Self::Spawned(c) => c.id(),
            Self::Reattached { pid } => *pid as u32,
        }
    }

    /// `Child::try_wait` for the Spawned variant; emulated via
    /// `kill(pid, 0)` for the Reattached variant (we don't own the
    /// process so syscall waitpid isn't allowed).  Returns the same
    /// shape as `Child::try_wait` so call sites don't branch.
    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        match self {
            Self::Spawned(c) => c.try_wait(),
            Self::Reattached { pid } => {
                let alive = unsafe { libc::kill(*pid, 0) } == 0;
                if alive {
                    Ok(None)
                } else {
                    // Synthesise a "process gone" status; the only
                    // caller (`L3Conn::poll`) just needs Some(_).
                    // Spawn a quickly-exiting helper to obtain a real
                    // ExitStatus value cheaply.
                    let st = std::process::Command::new("/usr/bin/true")
                        .status()?;
                    Ok(Some(st))
                }
            }
        }
    }

    /// `Child::kill` for the Spawned variant; SIGTERM via pid for
    /// the Reattached variant.  Best-effort either way.
    pub fn kill(&mut self) -> std::io::Result<()> {
        match self {
            Self::Spawned(c) => c.kill(),
            Self::Reattached { pid } => {
                unsafe { libc::kill(*pid, libc::SIGTERM) };
                Ok(())
            }
        }
    }

    /// `Child::wait` for the Spawned variant; busy-poll `kill 0` for
    /// the Reattached variant (with a short timeout — we can't waitpid
    /// for a foreign process so we just verify it died).
    pub fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        match self {
            Self::Spawned(c) => c.wait(),
            Self::Reattached { pid } => {
                let deadline = std::time::Instant::now()
                    + std::time::Duration::from_millis(200);
                while std::time::Instant::now() < deadline {
                    if unsafe { libc::kill(*pid, 0) } != 0 {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                // ExitStatus is needed for the API contract; synthesise.
                std::process::Command::new("/usr/bin/true").status()
            }
        }
    }
}

fn new_control_writer(s: UnixStream) -> marspot_term::frame_writer::FrameWriter {
    marspot_term::frame_writer::FrameWriter::new("l2-l3-writer", s, marspot_term::frame_writer::cap::CONTROL_SOCKET)
}

pub struct L3Conn {
    child: L3Process,
    /// Write half of the L2↔L3 control socket — forwards key events.
    /// (A reader thread on the matching half lives in the container,
    /// turning L3's `GridReady` pokes into redraw wakes.)
    ///
    /// Queued, never written inline: macOS gives these sockets an 8 KiB
    /// send buffer (measured), so an L3 that stops reading — wedged,
    /// mid-execv, stopped — would otherwise park **L2's** loop inside
    /// `write_all`, freezing every pane instead of just its own.
    /// Write-only; the reader runs on a separate clone in its own thread.
    control: marspot_term::frame_writer::FrameWriter,
    reader: GridShmReader,
    /// shelld session this L3 drives — needed to spawn a replacement on the
    /// *same* session for a per-session silent update (target #4 step 5a).
    session_id: u64,
    /// A replacement L3 (new binary, same session) brought up behind the
    /// scenes; once it has replayed the bytelog + published a frame, `poll`
    /// atomically promotes it (kills the old child, points us at the new
    /// pieces).  Invisible because the replayed screen matches.
    pending: Option<L3Spawn>,
    /// Mirror of L3's visible grid, rebuilt from the shm snapshot.
    grid: Grid,
    /// Mode flags from the last snapshot, surfaced to the renderer /
    /// input encoder via `PaneBackend`'s accessors.
    cursor_visible: bool,
    app_cursor_keys: bool,
    bracketed_paste: bool,
    /// Mouse tracking active(DECSET 1000/1002/1003)on L3's terminal.
    /// L2 reads this to bypass scrollback_len clamping for wheel scroll
    /// — alt-screen TUI(claudecode 等)scrollback_len 永远 0,不 bypass
    /// L2 apply_scroll_lines short-circuit,L3 mouse forwarding 永远没
    /// 机会触发.
    mouse_tracking_active: bool,
    /// Mouse SGR encoding(DECSET 1006).L2 mouse-on 时按这个选 SGR
    /// 字节格式 vs X11 legacy.
    mouse_sgr_active: bool,
    /// Last shm publish seq we mirrored; lets `poll()` skip a re-fill
    /// when nothing changed (so L2's heartbeat doesn't force a render).
    last_seq: u64,
    /// Last cell dims we *requested* L3 resize to.  The mirror grid lags a
    /// frame behind a request (L3 has to reflow + republish first), so
    /// `forward_resize` dedups against this rather than the mirror — else
    /// every layout rebuild would re-send the same resize until the mirror
    /// caught up.
    req_cols: u16,
    req_rows: u16,
    /// Last scrollback view offset we *requested* L3 publish at (dedup, as
    /// with the dims).  L2 can't scroll the mirror itself — it holds only
    /// the visible window — so it asks L3 which window to publish.
    req_view_offset: u16,
    /// Scrollback depth from the last snapshot — L2 has no scrollback of
    /// its own, so this is what `apply_scroll_lines` clamps against.
    snap_scrollback_len: u32,
    /// Set once the child process has exited (observed by `poll`'s
    /// `try_wait`).  Read by `is_exited`/`state`, which are `&self`.
    exited: bool,
    /// Scratch buffer for the snapshot cell copy — reused across polls
    /// so the per-frame read allocates zero.
    scratch: Vec<Cell>,
    /// Scratch buffer for the per-row DECAWM wrapped flags published
    /// alongside the cells (shm v3+).  Mirrors the same allocate-once
    /// reuse as `scratch`.
    scratch_wrapped: Vec<bool>,
    /// Reply channel for `GetSelectionText`: the control-socket reader
    /// thread routes each `SelectionText` frame here, and
    /// `request_selection_text` blocks on it (Cmd-C round-trip).  L3 owns
    /// the grid + scrollback; L2's mirror is window-only.  Each reply
    /// carries the request `seq` it answers, so a late reply from a
    /// timed-out request can't be returned for a newer one.
    selection_rx: Receiver<(u32, String)>,
    /// Monotonic request id stamped on each `GetSelectionText` and echoed
    /// in the `SelectionText` reply (see `request_selection_text`).
    selection_seq: u32,
}

impl L3Conn {
    /// RFC-003 Amendment 7 step 4: assemble an L3Conn from pieces we
    /// reattached to (we didn't fork; the L3 was spawned by a prior
    /// L2 that has since swapped).  Same shape as `new` except the
    /// process handle wraps a foreign pid we don't own.
    pub fn reattach(
        pid: i32,
        control: UnixStream,
        reader: GridShmReader,
        selection_rx: Receiver<(u32, String)>,
        session_id: u64,
    ) -> Self {
        let (cols, rows) = (reader.cols(), reader.rows());
        let grid = Grid::new(cols, rows);
        Self {
            child: L3Process::Reattached { pid },
            control: new_control_writer(control),
            reader,
            session_id,
            pending: None,
            grid,
            cursor_visible: true,
            app_cursor_keys: false,
            mouse_tracking_active: false,
            mouse_sgr_active: false,
            bracketed_paste: false,
            last_seq: 0,
            req_cols: cols,
            req_rows: rows,
            req_view_offset: 0,
            snap_scrollback_len: 0,
            exited: false,
            scratch: Vec::new(),
            scratch_wrapped: Vec::new(),
            selection_rx,
            selection_seq: 0,
        }
    }

    /// Assemble from already-spawned pieces.  The container does the
    /// spawn (socketpair + shm region + `Command`); this is pure
    /// assembly so `Pane`/`PaneBackend` stay free of process-launch glue.
    pub fn new(spawn: L3Spawn, session_id: u64) -> Self {
        let (cols, rows) = (spawn.reader.cols(), spawn.reader.rows());
        let grid = Grid::new(cols, rows);
        Self {
            child: L3Process::Spawned(spawn.child),
            control: new_control_writer(spawn.control),
            reader: spawn.reader,
            session_id,
            pending: None,
            grid,
            cursor_visible: true,
            app_cursor_keys: false,
            mouse_tracking_active: false,
            mouse_sgr_active: false,
            bracketed_paste: false,
            last_seq: 0,
            req_cols: cols,
            req_rows: rows,
            req_view_offset: 0,
            snap_scrollback_len: 0,
            exited: false,
            scratch: Vec::new(),
            scratch_wrapped: Vec::new(),
            selection_rx: spawn.selection_rx,
            selection_seq: 0,
        }
    }

    /// shelld session this L3 drives (so the container can spawn a
    /// replacement on the same session for a silent update).
    fn session_id(&self) -> u64 {
        self.session_id
    }

    /// shelld session this L3 drives. Public so the GUI can ask shelld
    /// to terminate it when the user clicks [×] on the sidebar — the
    /// L3 child gets reaped on its own; this just keeps shelld from
    /// resurrecting the bytelog when a fresh pane opens later.
    pub(crate) fn shelld_session_id(&self) -> u64 {
        self.session_id
    }

    /// True while a replacement L3 is being brought up but not yet
    /// promoted — the container avoids stacking a second swap.
    fn is_swapping(&self) -> bool {
        self.pending.is_some()
    }

    /// Stage a replacement L3 (new binary, same session) for a per-session
    /// silent update.  It replays the bytelog into its own region in the
    /// background; `poll` promotes it once it has published a frame.
    fn begin_swap(&mut self, spawn: L3Spawn) {
        self.pending = Some(spawn);
    }

    /// Promote a staged replacement once it has published its first
    /// (replayed) frame: kill the old child and point ourselves at the new
    /// pieces.  Returns true if a promotion happened (mirror must re-fill).
    fn try_promote(&mut self) -> bool {
        let ready = self
            .pending
            .as_ref()
            .is_some_and(|p| p.reader.seq() > 0);
        if !ready {
            return false;
        }
        let next = self.pending.take().unwrap();
        // Kill + reap the old L3 (its reader thread ends on the resulting
        // EOF; shelld keeps the session alive for the replacement).
        let old_pid = self.child.id();
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.child = L3Process::Spawned(next.child);
        self.control = new_control_writer(next.control);
        self.reader = next.reader;
        self.selection_rx = next.selection_rx;
        // The new L3 booted fresh: reset the mirror bookkeeping so the next
        // poll re-fills from its region, and drop any stale scroll request
        // (it republishes at live).
        self.last_seq = 0;
        self.req_view_offset = 0;
        self.req_cols = self.reader.cols();
        self.req_rows = self.reader.rows();
        eprintln!(
            "[core] L3 session {} silent-swapped: old pid={old_pid} → new pid={}",
            self.session_id,
            self.child.id()
        );
        true
    }

    /// Re-read the shm mirror if L3 published a new frame.  Returns
    /// `true` when the mirror changed (caller should redraw).  Cheap
    /// no-op (one atomic load) when the seq is unchanged.
    fn poll(&mut self) -> bool {
        // Promote a staged silent-update replacement the moment it has
        // replayed + published — kills the old child and repoints us at the
        // new one. `force_fill` so we re-read the new region even if its
        // first seq coincides with our last.
        let mut force_fill = false;
        if self.pending.is_some() {
            force_fill = self.try_promote();
        }
        // Observe exit here (the only `&mut` entry point); `is_exited`
        // and `state` are `&self` and just read the flag.  Crash isolation:
        // an L3 dying (panic / kill / shell exit) is one session ending —
        // log it once and leave the pane showing its last frame (state →
        // Exited).  The process boundary means it can't take L2 or sibling
        // L3s down; the container reaps the child via `L3Conn::Drop`.
        if !self.exited {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.exited = true;
                eprintln!(
                    "[core] L3 session pid={} exited ({status}) — pane frozen at last frame, others unaffected",
                    self.child.id()
                );
            }
        }
        let seq = self.reader.seq();
        if seq == self.last_seq && !force_fill {
            return false;
        }
        let Some(snap) = self
            .reader
            .read(&mut self.scratch, &mut self.scratch_wrapped)
        else {
            return false; // never published yet (seq 0)
        };
        // Reshape the mirror if L3's geometry changed under us.
        if self.grid.cols() != snap.cols || self.grid.rows() != snap.rows {
            self.grid = Grid::new(snap.cols, snap.rows);
        }
        for row in 0..snap.rows {
            let base = row as usize * snap.cols as usize;
            for col in 0..snap.cols {
                self.grid
                    .set_cell(col, row, self.scratch[base + col as usize]);
            }
        }
        // Per-row DECAWM continuation flags — without this the L2-side
        // link scanner can't tell a soft-wrapped row from a real
        // newline, breaking URL/path detection that overflows a row.
        for row in 0..snap.rows {
            let flag = self
                .scratch_wrapped
                .get(row as usize)
                .copied()
                .unwrap_or(false);
            self.grid.set_row_wrapped(row, flag);
        }
        self.grid.set_cursor(snap.cursor_col, snap.cursor_row);
        self.cursor_visible = snap.flags & FLAG_CURSOR_VISIBLE != 0;
        self.app_cursor_keys = snap.flags & FLAG_APP_CURSOR_KEYS != 0;
        self.bracketed_paste = snap.flags & FLAG_BRACKETED_PASTE != 0;
        self.mouse_tracking_active = snap.flags & FLAG_MOUSE_TRACKING != 0;
        self.mouse_sgr_active = snap.flags & FLAG_MOUSE_SGR != 0;
        self.snap_scrollback_len = snap.scrollback_len;
        // Re-read the seq after the copy: if L3 republished mid-fill,
        // leave it stale so the next poll re-reads rather than missing a
        // frame.
        self.last_seq = seq;
        true
    }

    /// Replace this L3Conn's L2↔L3 control stream after a backend
    /// reconnect (`l3_reader_loop` saw EOF, main loop ran
    /// `wait_and_connect`, and now we hot-swap the write half so
    /// subsequent `forward_key`/`forward_resize`/etc go through the
    /// fresh stream).  Old stream's Drop closes the old fd.
    /// Both halves move together, and that is the whole point.
    ///
    /// This used to swap only `control` (the write half).  The reader
    /// half lives in a thread that owns the matching `selection_tx`, so
    /// after a reconnect the old thread was gone — its sender dropped —
    /// while `selection_rx` still pointed at that dead channel.
    /// `request_selection_text` then got `Disconnected` immediately and
    /// returned `None`, so **Cmd-C went permanently silent on that pane
    /// after any silent-update reconnect**, with no log to say why.
    /// `try_promote` (the swap path) always replaced both; this one
    /// didn't, and the asymmetry is what made it a bug.
    pub fn swap_control(
        &mut self,
        new_control: UnixStream,
        selection_rx: Receiver<(u32, String)>,
    ) {
        // Dropping the old writer ends its thread; anything still queued
        // for the dead socket goes with it, which is correct — those
        // frames were addressed to an L3 that no longer exists.
        self.control = new_control_writer(new_control);
        self.selection_rx = selection_rx;
    }

    /// Forward a keystroke to the session process, which encodes it with
    /// its own terminal modes and local-echoes.  Best-effort: a dead
    /// socket means L3 went away and the container will reap it.
    fn forward_key(&mut self, event: &MarspotKeyEvent, mods: Modifiers) {
        let wire = event_to_wire(event, mods);
        // Placeholder window id: L3 is window-blind and drops it.
        let frame = Frame::new(
            MsgType::KeyEvent,
            encode_key_event(&wire, marspot_term::shell_proto::FIRST_WINDOW_ID),
        );
        let _ = self.control.send(frame);
    }

    /// Forward pasted clipboard text; L3 bracketed-wraps it (per its own
    /// terminal mode) and writes the PTY.  Best-effort over the control
    /// socket.
    fn forward_paste(&mut self, text: &str) {
        let frame = Frame::new(MsgType::Paste, encode_paste(text));
        let _ = self.control.send(frame);
    }

    /// Forward raw bytes (no bracketed-paste wrap) for cc plugin
    /// inject-input — used by the profile-cycle state machine to
    /// push `claude5 --resume <uuid>\r` straight into the PTY.
    fn forward_inject_input(&mut self, bytes: &[u8]) {
        let frame = Frame::new(
            MsgType::InjectInput,
            crate::shell_proto::encode_inject_input(self.session_id, bytes),
        );
        let _ = self.control.send(frame);
    }

    fn forward_pane_hold_grid(&mut self, on: bool) {
        let frame = Frame::new(
            MsgType::PaneHoldGrid,
            crate::shell_proto::encode_pane_hold_grid(self.session_id, on),
        );
        let _ = self.control.send(frame);
    }

    /// C5 — send a search request to the L3 worker.  The L3 main
    /// loop cancels any in-flight worker, snapshots the File
    /// scrollback + live grid (B3 + B4), and replies with
    /// `SearchResults` frames on the same socket (decoded by
    /// `l3_reader_loop` into `CoreEvent::SearchResults`).
    fn forward_search_scrollback(
        &mut self,
        query_id: u32,
        case_sensitive: bool,
        max_total: u32,
        query: &str,
    ) {
        let payload = encode_search_scrollback(query_id, case_sensitive, max_total, query);
        let frame = Frame::new(MsgType::SearchScrollback, payload);
        let _ = self.control.send(frame);
    }

    fn forward_search_cancel(&mut self, query_id: u32) {
        let payload = encode_search_cancel(query_id);
        let frame = Frame::new(MsgType::SearchCancel, payload);
        let _ = self.control.send(frame);
    }

    fn forward_search_more(&mut self, query_id: u32, count: u32) {
        // v1: direction is always 0 (older); D-phase will expose newer.
        let payload = encode_search_more(query_id, count, 0);
        let frame = Frame::new(MsgType::SearchMore, payload);
        let _ = self.control.send(frame);
    }

    /// Forward a cell-grid resize to the session process (dedup'd against
    /// the last requested dims).  L3 resizes its Terminal + PTY, reflows,
    /// and republishes at the new dims; the mirror reshapes on the next
    /// `poll`.  Best-effort over the control socket.
    fn forward_resize(&mut self, cols: u16, rows: u16) {
        if (cols, rows) == (self.req_cols, self.req_rows) {
            return;
        }
        self.req_cols = cols;
        self.req_rows = rows;
        let frame = Frame::new(MsgType::GridResize, encode_grid_resize(cols, rows));
        let _ = self.control.send(frame);
    }

    /// Ask L3 to publish the window at `view_offset` rows up from live
    /// (dedup'd against the last requested offset).  Best-effort over the
    /// control socket; the new window arrives on the next `poll`.
    fn forward_scroll(&mut self, view_offset: u16) {
        if view_offset == self.req_view_offset {
            return;
        }
        self.req_view_offset = view_offset;
        let frame = Frame::new(MsgType::GridScroll, encode_grid_scroll(view_offset));
        let _ = self.control.send(frame);
    }

    /// Scrollback depth reported by the last snapshot — L2's clamp bound
    /// for scrolling an L3 pane (it has no scrollback of its own).
    fn scrollback_len(&self) -> u16 {
        self.snap_scrollback_len.min(u16::MAX as u32) as u16
    }

    /// L3-reported mouse tracking active(from snapshot flags).
    fn mouse_tracking_active(&self) -> bool {
        self.mouse_tracking_active
    }

    fn mouse_sgr_active(&self) -> bool {
        self.mouse_sgr_active
    }

    /// Cmd-C round-trip: ask L3 for the text under a selection and block
    /// for the reply (the reader thread routes `SelectionText` into
    /// `selection_rx`).  `None` on a write error, a dead/slow L3 (1 s
    /// timeout), or an empty selection.  Synchronous because the clipboard
    /// write needs the string now; the request is rare (a keystroke), so a
    /// brief block is fine.
    ///
    /// Each request carries a fresh `seq` echoed in the reply.  Under heavy
    /// output the reader can fall behind, so a request occasionally times
    /// out; its reply then arrives late.  Matching on `seq` (and skipping
    /// any reply older than the one we're waiting for) guarantees we never
    /// hand back a stale reply for the current copy — the bug where a copy
    /// pasted the *previous* selection.
    fn request_selection_text(
        &mut self,
        anchor: (u16, u32),
        focus: (u16, u32),
        blockwise: bool,
    ) -> Option<String> {
        // Drain any stale replies left from earlier timed-out requests.
        while self.selection_rx.try_recv().is_ok() {}
        self.selection_seq = self.selection_seq.wrapping_add(1);
        let want = self.selection_seq;
        let frame = Frame::new(
            MsgType::GetSelectionText,
            encode_get_selection_text(want, anchor, focus, blockwise),
        );
        if !self.control.send(frame) {
            return None;
        }
        // Wait up to ~1 s total for the reply tagged `want`, discarding any
        // earlier-seq reply that races in ahead of it.
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            let (seq, text) = self.selection_rx.recv_timeout(remaining).ok()?;
            if seq == want {
                return if text.is_empty() { None } else { Some(text) };
            }
            // Older reply (a prior request's late answer) — drop and keep
            // waiting.  A future seq can't happen (we send synchronously).
        }
    }

    fn is_exited(&self) -> bool {
        self.exited
    }
}

impl Drop for L3Conn {
    fn drop(&mut self) {
        // RFC-003 §6 Amendment 12 — DO NOT kill the L3 child here.
        //
        // Pre-RFC-003 this Drop killed+reaped the child as "bounded
        // teardown" because shelld owned the PTY and the L3 helper
        // was disposable.  RFC-003 reversed that: L3 owns the PTY +
        // shell, and L3 is the unit users care about preserving
        // across an L2 lifetime.  Two cases:
        //
        //   * close_session (user clicked × on a pane): marspot-core
        //     already SIGTERMed entry.pid + delete_session() BEFORE
        //     dropping the pane.  Killing again here is redundant.
        //   * L2 process exit (user Cmd-Q on marspot.app, supervisor
        //     restart, panic, crash): this Drop runs on every L3Conn
        //     during stack unwind.  Killing the child here destroys
        //     every L3 along with L2 — meaning a single Cmd-Q wipes
        //     the user's 9 active shells.  THAT is the "原来可以现在
        //     不行" regression from L4 retirement.
        //
        // Instead, drop the file handles (control socket + shm
        // reader) silently.  L3 sees the EOF, stays alive via
        // Amendment 11 debug-2/3, and waits for the next L2 boot to
        // shm_open + connect_with_handshake it (Amendment 7
        // reattach).  launchd reaps the orphaned L3 if it ever
        // exits later.
    }
}

/// A single live terminal session plus the small piece of UI state
/// (scrollback view offset, …) it owns independently of any container.
pub struct Pane {
    session: PaneBackend,
    /// How far this pane has receded from active use (0 live,
    /// 1 resting, 2 parked), set by L1 through `PaneRecede`.
    pub recede: u32,
    /// View offset into scrollback in rows. `0` = live tail; positive
    /// = looking back into history. A keystroke resets to 0 so the
    /// user's keypress always lands in a visible prompt.
    view_offset: u16,
    /// Last-seen value of `grid.scroll_push_count()`, captured by
    /// `drain_scroll_push_delta`.  The container (marspot's
    /// `user_event`) polls the delta after each pump so it can shift
    /// any live selection's abs coords by the number of lines that
    /// just rolled into scrollback — keeping the highlight pinned to
    /// the original content instead of the original screen position.
    last_seen_scroll_push: u64,
    /// A per-session silent update is staged for this pane but deferred
    /// because it's the *focused* pane (an idle pane swaps immediately; a
    /// replay would blip an interactive TUI under the user's hands).  The
    /// renderer draws a refresh affordance in the focused pane's title
    /// strip while this is set; clicking it triggers the swap.  Target #4
    /// step 5b.  Only ever set on an L3 pane.
    update_pending: bool,
    /// C1 — pane-attached interactive overlays (search bar in C2,
    /// result list in C3, …).  Default empty; layout math sums their
    /// `fixed_height_rows()` on each frame via `tool_fixed_height_sums`
    /// so an empty `tools` Vec is byte-identical to pre-C1 behaviour.
    /// See `docs/scrollback-search.md` §6.1.
    pub tools: Vec<Box<dyn marspot_term::render::PaneTool>>,
    /// C4 — active search highlight, set by C5's main loop when the
    /// user navigates to a hit.  `None` = no highlight (renderer
    /// passes empty `highlight_spans` to `build_instances`).
    pub active_highlight: Option<marspot_term::render::ActiveHighlight>,
    /// C5 — per-pane search overlay state.  `None` until Cmd+F
    /// opens it; `Some` until Esc closes it.  Independent of any
    /// other pane's search state (each pane has its own).
    pub search: Option<PaneSearch>,
    /// User-set title override.  `None` falls back to the render-time
    /// placeholder chain (plugin title → cwd basename → ordinal).
    ///
    /// Lives on the pane, not in a container-side parallel Vec: RFC-004
    /// B.3 already established that a title binds to its session by
    /// identity, not by slot — the old `custom_titles: Vec<Option<String>>`
    /// had to be hand-permuted in lockstep with every pane reorder, and
    /// boot carried a by-sid search to repair the pairing.  As a field it
    /// travels with the pane structurally and both go away.
    pub custom_title: Option<String>,
}

/// C5 — per-pane bundle of the search overlay state.  Wraps the
/// `SearchBar` (input) + `SearchList` (results) + the L2-side
/// query-id counter + debounce timer that drives the wire
/// `SearchScrollback` emission.
pub struct PaneSearch {
    pub bar: SearchBar,
    pub list: SearchList,
    /// Monotonic query id allocated locally; bumped on every
    /// `apply_pending_query`.  L3 uses it for last-write-wins
    /// cancellation (D15).
    pub next_query_id: u32,
    /// Last `query` we actually sent on the wire.  Suppresses
    /// re-emits when the user types and then deletes back to the
    /// same text.
    pub last_emitted_query: String,
}

impl PaneSearch {
    pub fn open() -> Self {
        let mut bar = SearchBar::new();
        bar.focused = true;
        Self {
            bar,
            list: SearchList::new(),
            next_query_id: 1,
            last_emitted_query: String::new(),
        }
    }
}

impl Pane {
    /// Wrap an already-spawned local Session.  The legacy entry
    /// point — mcli, bench, tests, and tmux-CC dispatch land here.
    pub fn new(session: Session) -> Self {
        Self {
            session: PaneBackend::Local(session),
            recede: 0,
            view_offset: 0,
            last_seen_scroll_push: 0,
            update_pending: false,
            tools: Vec::new(),
            active_highlight: None,
            search: None,
            custom_title: None,
        }
    }

    /// Wrap a per-session L3 process (target #4, behind `MARSPOT_L3=1`).
    /// The container assembles the `L3Conn` (spawn + shm + socket) and
    /// hands it here; the pane then behaves like any other — same render
    /// / scroll / focus path — reading its grid from the shm mirror.
    pub fn new_l3(conn: L3Conn) -> Self {
        Self {
            session: PaneBackend::L3(conn),
            recede: 0,
            view_offset: 0,
            last_seen_scroll_push: 0,
            update_pending: false,
            tools: Vec::new(),
            active_highlight: None,
            search: None,
            custom_title: None,
        }
    }

    /// RFC-004 B.2 — a slot whose session failed to assemble at boot.
    /// Keeps the sid alive so the slot never compacts; the revive path
    /// (keystroke on an exited pane) respawns the same session id.
    /// A slot rendering "starting…" while its session is spawned off
    /// the main loop.  Becomes a real L3 pane when the spawn lands.
    pub fn new_pending(session_id: u64, cols: u16, rows: u16) -> Self {
        Self {
            session: PaneBackend::Vacant(VacantPane::new_pending(session_id, cols, rows)),
            recede: 0,
            view_offset: 0,
            last_seen_scroll_push: 0,
            update_pending: false,
            tools: Vec::new(),
            active_highlight: None,
            search: None,
            custom_title: None,
        }
    }

    /// Replace this pane's backend in place, keeping the pane shell
    /// (tools, search, highlight) and resetting only what is tied to the
    /// old backend's content.
    ///
    /// Both outcomes of an off-loop spawn go through here — success
    /// installs an `L3` backend, failure installs a plain `Vacant` one —
    /// so the two paths can't drift into "one preserves pane state, the
    /// other silently drops it".
    ///
    /// `view_offset` and `last_seen_scroll_push` are reset deliberately:
    /// they index into the *previous* backend's scrollback and mean
    /// nothing against the new one.
    pub fn adopt_backend(&mut self, backend: PaneBackend) {
        self.session = backend;
        self.view_offset = 0;
        self.last_seen_scroll_push = 0;
    }

    pub fn new_vacant(session_id: u64, cols: u16, rows: u16) -> Self {
        Self {
            session: PaneBackend::Vacant(VacantPane::new(session_id, cols, rows)),
            recede: 0,
            view_offset: 0,
            last_seen_scroll_push: 0,
            update_pending: false,
            tools: Vec::new(),
            active_highlight: None,
            search: None,
            custom_title: None,
        }
    }

    /// Vacant slot? (RFC-004 B.2) — the revive-on-keystroke path
    /// covers these in addition to exited L3 panes.
    pub fn is_vacant(&self) -> bool {
        matches!(self.session, PaneBackend::Vacant(_))
    }

    /// RFC-006 — dormant placeholder (a moved-out pane's empty slot).
    /// Not live, not resurrectable, revives only on an explicit click.
    pub fn is_dormant(&self) -> bool {
        matches!(&self.session, PaneBackend::Vacant(v) if v.is_dormant())
    }

    /// RFC-006 — the placeholder a moved-out pane leaves behind.
    pub fn new_dormant(cols: u16, rows: u16) -> Self {
        Self {
            session: PaneBackend::Vacant(VacantPane::new_dormant(cols, rows)),
            recede: 0,
            view_offset: 0,
            last_seen_scroll_push: 0,
            update_pending: false,
            tools: Vec::new(),
            active_highlight: None,
            search: None,
            custom_title: None,
        }
    }

    /// L3-backed?  Container input routing forwards key *events* to L3
    /// instead of encoding PTY bytes locally.
    pub fn is_l3(&self) -> bool {
        self.session.is_l3()
    }

    /// Forward a keystroke to an L3 session (no-op otherwise).  Snaps the
    /// view back to live like `handle_key` does, since the keystroke's
    /// echo will land at the live tail.
    pub fn forward_key(&mut self, event: &MarspotKeyEvent, mods: Modifiers) -> bool {
        let need_redraw = self.view_offset != 0;
        self.view_offset = 0;
        // Snap L3 back to the live tail too: its echo lands there, and the
        // forward dedups so this is free when already live.
        self.session.forward_scroll(0);
        self.session.forward_key(event, mods);
        need_redraw
    }

    /// Returns the number of lines pushed into scrollback since the
    /// last call (then updates the bookmark).  Marspot calls this
    /// after each `pump` to keep live selections aligned with the
    /// content they were originally anchored to — every push moves
    /// the same content one row further from live bottom, so the
    /// selection's abs coords must rise by the same amount.
    pub fn drain_scroll_push_delta(&mut self) -> u64 {
        let now = self.session.grid().scroll_push_count();
        let delta = now.saturating_sub(self.last_seen_scroll_push);
        self.last_seen_scroll_push = now;
        delta
    }

    /// Read-only access to the backend.  Multi-session containers
    /// reach in for `state()` / `terminal()` / `is_exited()` etc.;
    /// the per-backend method matrix above hides which kind of
    /// session is underneath.
    pub fn session(&self) -> &PaneBackend {
        &self.session
    }

    /// Mutable access — same rationale.
    pub fn session_mut(&mut self) -> &mut PaneBackend {
        &mut self.session
    }

    /// Current view offset (rows into scrollback, 0 = live tail).
    /// F1+13 — grid-content "freshness" tag.  For L3 panes this is the
    /// shm reader's last accepted seq (bumps on every L3 publish).
    /// For Local (in-process) panes we fall back to a coarse signal
    /// derived from cursor + scroll-push count — enough to invalidate
    /// the renderer's per-pane instance cache when content changes,
    /// at the cost of some over-invalidation that the cache layer can
    /// absorb.
    pub fn grid_seq(&self) -> u64 {
        match &self.session {
            PaneBackend::L3(c) => c.last_seq,
            PaneBackend::Local(s) => {
                let g = s.terminal().grid();
                let (cc, cr) = g.cursor();
                // Pack into u64 so a movement (cursor) or push
                // (scroll_push_count) reliably changes the tag.
                ((g.scroll_push_count() & 0xFFFF_FFFF) << 32)
                    | ((cc as u64) << 16)
                    | (cr as u64)
            }
            // Static message grid — never changes after construction.
            PaneBackend::Vacant(_) => 0,
        }
    }

    pub fn view_offset(&self) -> u16 {
        self.view_offset
    }

    /// A deferred silent update is staged for this (focused) pane — the
    /// renderer shows a refresh affordance; a click triggers the swap.
    pub fn update_pending(&self) -> bool {
        self.update_pending
    }

    /// Mark/clear the deferred-update affordance (target #4 step 5b).
    pub fn set_update_pending(&mut self, v: bool) {
        self.update_pending = v;
    }

    /// Drain the PTY reader into the terminal parser. Returns the
    /// number of bytes consumed; `0` means the wake was spurious or
    /// the channel went empty. Containers use this in the
    /// `user_event` callback to decide whether to request_redraw.
    pub fn pump(&mut self) -> usize {
        self.session.pump()
    }

    /// True once the underlying shell has exited and all bytes have
    /// been pumped through the parser.
    pub fn is_exited(&self) -> bool {
        self.session.is_exited()
    }

    /// The shelld session id behind this pane, when it has one. `None`
    /// for the in-process `Local` backend.
    pub fn shelld_session_id(&self) -> Option<u64> {
        self.session.shelld_session_id()
    }

    /// Handle a key event. Writes the encoded bytes to the PTY and
    /// snaps the view back to the live tail if it was scrolled into
    /// history. Returns `true` when the caller should `request_redraw`
    /// (only when the view offset changed; the byte write triggers a
    /// PTY wake → `pump` → redraw on its own).
    pub fn handle_key(&mut self, event: &MarspotKeyEvent, mods: Modifiers) -> bool {
        // L3 panes encode in the session process; forward the event.
        if self.session.is_l3() {
            return self.forward_key(event, mods);
        }
        // Forward the terminal's DECCKM + bracketed-paste state so
        // arrow keys encode correctly for TUI apps in application
        // cursor key mode, and Cmd-V paste is wrapped in `\e[200~ /
        // \e[201~` when the app has opted in.
        let app_mode = self.session.cursor_key_application_mode();
        let bracketed = self.session.bracketed_paste_mode();
        let Some(bytes) =
            key_event_to_bytes(event, mods, app_mode, bracketed, crate::input::read_clipboard_text)
        else {
            return false;
        };
        let mut need_redraw = false;
        if self.view_offset != 0 {
            self.view_offset = 0;
            need_redraw = true;
        }
        let _ = self.session.write(&bytes);
        need_redraw
    }

    /// Apply a scroll delta in rows. `delta` is interpreted on the
    /// view_offset axis: positive = move the view back into older
    /// scrollback (view_offset increases), negative = forward to the
    /// live tail. Returns `true` if view_offset actually changed and
    /// the caller should `request_redraw`; `false` when the delta
    /// was sub-line or clamped at an edge.
    ///
    /// Sign / invert / accel multipliers live in the caller — Pane
    /// only knows about rows and scrollback bounds. mcli and marspot
    /// share this so both stay in lockstep on scrollback semantics
    /// while keeping flexible window-level scroll-direction prefs.
    pub fn apply_scroll_lines(&mut self, delta: i32) -> bool {
        if delta == 0 {
            return false;
        }
        // alt-screen TUI(claudecode)mouse tracking 模式 — L2 自己
        // encode wheel 成 SGR mouse event 字节通过 InjectInput 写 PTY.
        // 不走 view_offset / forward_scroll(view_offset u16 在 0 处会
        // saturate,wheel-down 滚不到底就是这个 bug;直接 inject 字节
        // 没这个状态机问题).cols/rows 用 session 当前 grid 尺寸.
        if self.session.is_l3() && self.session.l3_mouse_tracking_active() {
            let (cols, rows) = (self.session.grid().cols(), self.session.grid().rows());
            let (button, n) = if delta > 0 {
                (64u8, delta as u32)   // wheel up
            } else {
                (65u8, (-delta) as u32) // wheel down
            };
            // Cap one event at a screenful of ticks.  Each tick is ~12
            // bytes into the child's stdin, and a momentum scroll can
            // hand us a delta of hundreds — which both over-scrolls the
            // TUI and dumps kilobytes into a tty that may not be reading
            // (see `PtyWriter` on why that used to wedge the pane).  The
            // momentum stream delivers many events regardless, so the
            // cap costs no reachable scroll distance.
            let n = n.min(rows.max(1) as u32);
            if n > 0 {
                self.session.l3_inject_wheel(button, n, cols, rows);
            }
            return true;
        }
        // L3 owns its scrollback; L2 has only the visible-window mirror, so
        // it clamps against the depth the snapshot reported and asks L3 to
        // publish the new window. In-process backends scroll their own grid.
        let max = if self.session.is_l3() {
            self.session.l3_scrollback_len() as i32
        } else {
            self.session.grid().scrollback_len() as i32
        };
        let new = (self.view_offset as i32 + delta).clamp(0, max) as u16;
        if new == self.view_offset {
            return false;
        }
        self.view_offset = new;
        self.session.forward_scroll(new); // no-op for in-process backends
        true
    }

    /// Auto-pin the viewport when a row scrolls into scrollback while
    /// the user is viewing history.  L3 runs the symmetric bump in its
    /// publish loop (`marspot-session::main` keeps a `last_scroll_push`
    /// counter), so this side just keeps L2's local `view_offset`
    /// cache in lockstep — no `forward_scroll` because L3 already
    /// published at the bumped offset on its side, saving a round-trip
    /// and the one-frame slip that would otherwise be visible.
    /// No-op when `view_offset == 0` (live tail): nothing to pin.
    pub fn bump_view_offset_on_scroll_push(&mut self, delta: u16) {
        if self.view_offset == 0 || delta == 0 {
            return;
        }
        let max = if self.session.is_l3() {
            self.session.l3_scrollback_len() as u16
        } else {
            self.session.grid().scrollback_len() as u16
        };
        // F1+7 — if `max` (L3-reported scrollback_len) is less than
        // the current `view_offset`, L3 just temporarily lost its
        // scrollback context — most commonly because it entered the
        // alt-screen for a TUI (claudecode etc.) whose own
        // scrollback is shorter than the saved main's history we
        // were viewing.  Clamping view_offset down to `max` would
        // visually look like a "bounce back to live" after every
        // PTY chunk emitted by the TUI.  Leave view_offset as-is
        // until the next jump or snap_to_live moves it intentionally;
        // the renderer will show whatever L3 published last.
        if max < self.view_offset {
            return;
        }
        let new = self.view_offset.saturating_add(delta).min(max);
        self.view_offset = new;
    }

    /// C5 — explicit view_offset setter, used by the search-jump
    /// path which computes the offset analytically (not via
    /// scroll deltas).
    pub fn set_view_offset(&mut self, off: u16) {
        self.view_offset = off;
    }

    /// Force the view back to the live tail. Used by container code
    /// when a non-key event (e.g. PTY write from a remote source)
    /// arrives and we want the user to see fresh output immediately.
    pub fn snap_to_live(&mut self) -> bool {
        if self.view_offset == 0 {
            return false;
        }
        self.view_offset = 0;
        self.session.forward_scroll(0); // no-op for in-process backends
        true
    }

    /// Resize the terminal grid if `(cols, rows)` differ from the
    /// current shape. No-op when shape is unchanged — `Session::resize`
    /// is cheap but does a SIGWINCH to the shell, which some processes
    /// react to (clear-screen redraws, reflow); avoiding spurious
    /// resize keeps user experience quiet.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        // L3 dedups inside the backend against its last *requested* dims
        // (the mirror grid lags a frame behind a resize request, so
        // guarding on it here would re-send every layout rebuild until the
        // mirror caught up).  In-process backends guard on the live grid.
        if self.session.is_l3() {
            self.session.resize(cols, rows);
            return;
        }
        if (cols, rows) != (self.session.grid().cols(), self.session.grid().rows()) {
            self.session.resize(cols, rows);
        }
    }

    /// Build a `SessionView` snapshot for the renderer. `focused`
    /// tells the renderer whether to draw the cursor filled (active
    /// pane) or hollow (background pane); `title` is the chrome title
    /// strip text (empty for mcli — it has no per-pane chrome above
    /// the grid).
    ///
    /// Non-focused panes render at `view_offset = 0` (live tail) so
    /// the layout reads "the focused pane shows whatever history I
    /// scrolled back into, the others stay live". The pane's own
    /// `view_offset` is preserved across focus changes — scroll back
    /// in pane A, switch to B, come back to A: you're where you left
    /// off.
    pub fn view<'a>(
        &'a self,
        focused: bool,
        title: &'a str,
        right_badge: &'a str,
    ) -> SessionView<'a> {
        // An L3 mirror is *already* the window L3 published at the requested
        // scroll offset (and holds no scrollback to offset into), so it
        // always renders at 0. In-process panes offset into their own grid:
        // the focused pane shows its scrollback, others stay live.
        let view_offset = if self.session.is_l3() {
            0
        } else if focused {
            self.view_offset
        } else {
            0
        };
        let (top_fixed_h_cells, bot_fixed_h_cells) = self.tool_fixed_height_sums();
        let seq = self.grid_seq();
        let search_overlay = self.search.as_ref().map(|s| {
            // Take up to the next `VIEWPORT` rows starting at
            // `visible_top` — matches what the renderer paints.
            let from = s.list.visible_top;
            let take = crate::tools::search_list::VIEWPORT.min(
                s.list.hits.len().saturating_sub(from),
            );
            let hits: Vec<marspot_term::render::SearchHitView> = s
                .list
                .hits
                .iter()
                .skip(from)
                .take(take)
                .enumerate()
                .map(|(i, h)| marspot_term::render::SearchHitView {
                    snippet: h.snippet.clone(),
                    is_focused: s.list.focused == Some(from + i),
                })
                .collect();
            let counter = if s.list.hits.is_empty() {
                None
            } else {
                Some((
                    s.list.focused_one_based(),
                    s.list.hits.len() as u32,
                ))
            };
            marspot_term::render::SearchOverlayView {
                query: s.bar.query.clone(),
                query_cursor: s.bar.cursor.min(u16::MAX as usize) as u16,
                case_sensitive: s.bar.case_sensitive,
                counter,
                hits,
            }
        });
        SessionView {
            grid: self.session.grid(),
            view_offset,
            cursor_visible: self.session.cursor_visible(),
            focused,
            title,
            selection: None,
            ime_preedit: "",
            update_pending: self.update_pending,
            dormant: self.is_dormant(),
            recede: self.recede,
            right_badge,
            top_fixed_h_cells,
            bot_fixed_h_cells,
            highlight_spans: self
                .active_highlight
                .as_ref()
                .map(|h| h.spans.as_slice())
                .unwrap_or(&[]),
            search_overlay,
            seq,
        }
    }

    /// C1 — sum the fixed-row heights of attached PaneTools, split by
    /// slot (Top vs Bottom).  Returns `(top, bot)`.  Empty `tools` →
    /// `(0, 0)`, byte-identical to pre-C1 layout.
    fn tool_fixed_height_sums(&self) -> (u16, u16) {
        let mut top: u16 = 0;
        let mut bot: u16 = 0;
        for t in &self.tools {
            match t.slot() {
                marspot_term::render::ToolSlot::TopFixed => {
                    top = top.saturating_add(t.fixed_height_rows())
                }
                marspot_term::render::ToolSlot::BottomFixed => {
                    bot = bot.saturating_add(t.fixed_height_rows())
                }
                marspot_term::render::ToolSlot::Overlay => {}
            }
        }
        (top, bot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pending guard must expire.
    ///
    /// `is_exited()` is what the revive-on-keystroke path checks, and
    /// `pending` suppresses it so a second spawn can't pile onto the
    /// first.  But that also disables the user's only way to wake a dead
    /// slot, so a spawn whose worker never reports back would strand the
    /// slot at "starting…" permanently.  Past `PENDING_MAX` the guard
    /// lifts and a keystroke can retry.
    #[test]
    fn pending_guard_expires_so_a_lost_spawn_cannot_strand_a_slot() {
        let mut v = VacantPane::new_pending(7, 80, 24);
        assert!(v.is_pending(), "a fresh spawn is in flight");

        // Fake the deadline having passed rather than sleeping 30 s.
        v.pending_since = Instant::now() - (PENDING_MAX + Duration::from_secs(1));
        assert!(
            !v.is_pending(),
            "past the deadline the slot must stop claiming a spawn is in flight"
        );

        let backend = PaneBackend::Vacant(v);
        assert!(
            backend.is_exited(),
            "an expired pending slot must report exited so revive-on-keystroke works"
        );
    }

    /// While the spawn is genuinely in flight, the slot must NOT report
    /// exited — otherwise the next keystroke starts a second spawn on
    /// top of the first.
    #[test]
    fn in_flight_spawn_suppresses_revive() {
        let v = VacantPane::new_pending(9, 80, 24);
        let backend = PaneBackend::Vacant(v);
        assert!(!backend.is_exited());
    }

    /// A plain vacant slot (boot-assembly failure) is born exited: that
    /// is precisely how the user gets it back.
    #[test]
    fn plain_vacant_slot_is_revivable_immediately() {
        let backend = PaneBackend::Vacant(VacantPane::new(11, 80, 24));
        assert!(backend.is_exited());
        assert_eq!(backend.shelld_session_id(), Some(11));
    }

    /// A vacant slot keeps naming its session — the sid is what lets the
    /// slot survive save/restore and be resurrected in place, so losing
    /// it would let the slot compact away (the RFC-004 drift bug).
    #[test]
    fn vacant_slot_retains_its_session_id_when_pending() {
        let backend = PaneBackend::Vacant(VacantPane::new_pending(42, 80, 24));
        assert_eq!(backend.shelld_session_id(), Some(42));
    }
}

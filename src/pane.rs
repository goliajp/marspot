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
};
use crate::input::{key_event_to_bytes, MarspotKeyEvent, Modifiers};
use crate::render::SessionView;
use crate::session::{Session, SessionState};
use crate::shell_proto::{
    encode_get_selection_text, encode_grid_resize, encode_grid_scroll, encode_key_event,
    encode_paste, event_to_wire, Frame, MsgType,
};
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
            PaneBackend::L3(_) => unreachable!("L3 pane has no in-process Terminal"),
        }
    }

    pub fn terminal_mut(&mut self) -> &mut Terminal {
        match self {
            PaneBackend::Local(s) => &mut s.terminal,
            PaneBackend::L3(_) => unreachable!("L3 pane has no in-process Terminal"),
        }
    }

    /// Visible grid for the renderer.  Local/shelld read their own
    /// terminal; L3 reads the synthetic mirror last filled from shm.
    pub fn grid(&self) -> &Grid {
        match self {
            PaneBackend::Local(s) => s.terminal().grid(),
            PaneBackend::L3(c) => &c.grid,
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
        }
    }

    pub fn cursor_visible(&self) -> bool {
        match self {
            PaneBackend::Local(s) => s.terminal().cursor_visible(),
            PaneBackend::L3(c) => c.cursor_visible,
        }
    }

    pub fn cursor_key_application_mode(&self) -> bool {
        match self {
            PaneBackend::Local(s) => s.terminal().cursor_key_application_mode(),
            PaneBackend::L3(c) => c.app_cursor_keys,
        }
    }

    pub fn bracketed_paste_mode(&self) -> bool {
        match self {
            PaneBackend::Local(s) => s.terminal().bracketed_paste_mode(),
            PaneBackend::L3(c) => c.bracketed_paste,
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

    /// RFC-003 §6 Amendment 14 — frame-based L3 silent self-update.
    /// Writes a `RequestSelfUpdate` to the L3's control socket; the
    /// L3 either ACKs and execvs in place (PTY + shell preserved) or
    /// declines (logged).  No-op for non-L3 backends.
    pub fn request_l3_self_update(&mut self) -> bool {
        match self {
            PaneBackend::L3(c) => c.request_self_update(),
            _ => false,
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
        }
    }

    pub fn is_exited(&self) -> bool {
        match self {
            PaneBackend::Local(s) => s.is_exited(),
            PaneBackend::L3(c) => c.is_exited(),
        }
    }

    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self {
            PaneBackend::Local(s) => s.write(bytes),
            // L3 input is forwarded as key events, not raw bytes; the
            // session process owns its own PTY write + response path.
            PaneBackend::L3(_) => Ok(0),
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        match self {
            PaneBackend::Local(s) => s.resize(cols, rows),
            // Forward the cell-grid resize to the session process; it
            // resizes its Terminal + PTY + reflows and republishes at the
            // new dims, which the mirror picks up on the next `poll`.
            PaneBackend::L3(c) => c.forward_resize(cols, rows),
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
            PaneBackend::L3(_) => {}
        }
    }

    /// Drain raw PTY bytes without feeding the terminal.  Same
    /// lenience as `feed_terminal`: only the local backend has a
    /// meaningful implementation.
    pub fn drain_raw(&mut self) -> Vec<u8> {
        match self {
            PaneBackend::Local(s) => s.drain_raw(),
            PaneBackend::L3(_) => Vec::new(),
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

pub struct L3Conn {
    child: L3Process,
    /// Write half of the L2↔L3 control socket — forwards key events.
    /// (A reader thread on the matching half lives in the container,
    /// turning L3's `GridReady` pokes into redraw wakes.)
    control: UnixStream,
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
            control,
            reader,
            session_id,
            pending: None,
            grid,
            cursor_visible: true,
            app_cursor_keys: false,
            bracketed_paste: false,
            last_seq: 0,
            req_cols: cols,
            req_rows: rows,
            req_view_offset: 0,
            snap_scrollback_len: 0,
            exited: false,
            scratch: Vec::new(),
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
            control: spawn.control,
            reader: spawn.reader,
            session_id,
            pending: None,
            grid,
            cursor_visible: true,
            app_cursor_keys: false,
            bracketed_paste: false,
            last_seq: 0,
            req_cols: cols,
            req_rows: rows,
            req_view_offset: 0,
            snap_scrollback_len: 0,
            exited: false,
            scratch: Vec::new(),
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
        self.control = next.control;
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
        let Some(snap) = self.reader.read(&mut self.scratch) else {
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
        self.grid.set_cursor(snap.cursor_col, snap.cursor_row);
        self.cursor_visible = snap.flags & FLAG_CURSOR_VISIBLE != 0;
        self.app_cursor_keys = snap.flags & FLAG_APP_CURSOR_KEYS != 0;
        self.bracketed_paste = snap.flags & FLAG_BRACKETED_PASTE != 0;
        self.snap_scrollback_len = snap.scrollback_len;
        // Re-read the seq after the copy: if L3 republished mid-fill,
        // leave it stale so the next poll re-reads rather than missing a
        // frame.
        self.last_seq = seq;
        true
    }

    /// Forward a keystroke to the session process, which encodes it with
    /// its own terminal modes and local-echoes.  Best-effort: a dead
    /// socket means L3 went away and the container will reap it.
    fn forward_key(&mut self, event: &MarspotKeyEvent, mods: Modifiers) {
        let wire = event_to_wire(event, mods);
        let frame = Frame::new(MsgType::KeyEvent, encode_key_event(&wire));
        let _ = frame.write_to(&mut self.control);
    }

    /// Forward pasted clipboard text; L3 bracketed-wraps it (per its own
    /// terminal mode) and writes the PTY.  Best-effort over the control
    /// socket.
    fn forward_paste(&mut self, text: &str) {
        let frame = Frame::new(MsgType::Paste, encode_paste(text));
        let _ = frame.write_to(&mut self.control);
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
        let _ = frame.write_to(&mut self.control);
    }

    /// RFC-003 §6 Amendment 14 — ask the L3 to `execv` into the
    /// current/marspot-session binary.  Best-effort: the L3 either
    /// ACKs (and we'll see its control EOF shortly as the execv
    /// discards the inherited fd), declines (logged on the reader
    /// side), or ignores entirely if it's an older build that
    /// doesn't speak this frame (`Frame::read_from`'s forward-compat
    /// path drops the unknown variant — see `shell_proto.rs`).
    pub fn request_self_update(&mut self) -> bool {
        let frame = Frame::new(MsgType::RequestSelfUpdate, Vec::new());
        frame.write_to(&mut self.control).is_ok()
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
        let _ = frame.write_to(&mut self.control);
    }

    /// Scrollback depth reported by the last snapshot — L2's clamp bound
    /// for scrolling an L3 pane (it has no scrollback of its own).
    fn scrollback_len(&self) -> u16 {
        self.snap_scrollback_len.min(u16::MAX as u32) as u16
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
        frame.write_to(&mut self.control).ok()?;
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
}

impl Pane {
    /// Wrap an already-spawned local Session.  The legacy entry
    /// point — mcli, bench, tests, and tmux-CC dispatch land here.
    pub fn new(session: Session) -> Self {
        Self {
            session: PaneBackend::Local(session),
            view_offset: 0,
            last_seen_scroll_push: 0,
            update_pending: false,
        }
    }

    /// Wrap a per-session L3 process (target #4, behind `MARSPOT_L3=1`).
    /// The container assembles the `L3Conn` (spawn + shm + socket) and
    /// hands it here; the pane then behaves like any other — same render
    /// / scroll / focus path — reading its grid from the shm mirror.
    pub fn new_l3(conn: L3Conn) -> Self {
        Self {
            session: PaneBackend::L3(conn),
            view_offset: 0,
            last_seen_scroll_push: 0,
            update_pending: false,
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
        SessionView {
            grid: self.session.grid(),
            view_offset,
            cursor_visible: self.session.cursor_visible(),
            focused,
            title,
            selection: None,
            ime_preedit: "",
            update_pending: self.update_pending,
            right_badge,
        }
    }
}

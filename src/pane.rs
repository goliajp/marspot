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

use crate::input::{key_event_to_bytes, MarspotKeyEvent, Modifiers};
use crate::render::SessionView;
use crate::session::{Session, SessionState};
use crate::shelld_client::{SessionState as ShelldState, ShelldSession};
use crate::terminal::Terminal;

/// One pane's backend: a locally forked PTY (the legacy / mcli /
/// bench path) or a shelld-managed session (marspot's live-update
/// path).  Same observable interface; the GUI / Pane don't need to
/// branch on which is in use except where shelld-specific affordances
/// (session id, reattach) come into play.
pub enum PaneBackend {
    Local(Session),
    Shelld(ShelldSession),
}

impl PaneBackend {
    pub fn terminal(&self) -> &Terminal {
        match self {
            PaneBackend::Local(s) => s.terminal(),
            PaneBackend::Shelld(s) => s.terminal(),
        }
    }

    pub fn terminal_mut(&mut self) -> &mut Terminal {
        match self {
            PaneBackend::Local(s) => &mut s.terminal,
            PaneBackend::Shelld(s) => &mut s.terminal,
        }
    }

    pub fn pump(&mut self) -> usize {
        match self {
            PaneBackend::Local(s) => s.pump(),
            PaneBackend::Shelld(s) => s.pump(),
        }
    }

    pub fn is_exited(&self) -> bool {
        match self {
            PaneBackend::Local(s) => s.is_exited(),
            PaneBackend::Shelld(s) => s.is_exited(),
        }
    }

    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self {
            PaneBackend::Local(s) => s.write(bytes),
            PaneBackend::Shelld(s) => s.write(bytes),
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        match self {
            PaneBackend::Local(s) => s.resize(cols, rows),
            PaneBackend::Shelld(s) => {
                let _ = s.resize(cols, rows);
            }
        }
    }

    pub fn state(&self) -> SessionState {
        match self {
            PaneBackend::Local(s) => s.state(),
            PaneBackend::Shelld(s) => match s.state() {
                ShelldState::Active => SessionState::Active,
                ShelldState::Idle => SessionState::Idle,
                ShelldState::Exited => SessionState::Exited,
            },
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
            PaneBackend::Shelld(_) => {
                // Phase 4: tmux-CC mode still runs through a local
                // Session; shelld panes ignore this entry point.
            }
        }
    }

    /// Drain raw PTY bytes without feeding the terminal.  Same
    /// lenience as `feed_terminal`: only the local backend has a
    /// meaningful implementation.
    pub fn drain_raw(&mut self) -> Vec<u8> {
        match self {
            PaneBackend::Local(s) => s.drain_raw(),
            PaneBackend::Shelld(_) => Vec::new(),
        }
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
}

impl Pane {
    /// Wrap an already-spawned local Session.  The legacy entry
    /// point — mcli, bench, tests, and tmux-CC dispatch land here.
    pub fn new(session: Session) -> Self {
        Self {
            session: PaneBackend::Local(session),
            view_offset: 0,
            last_seen_scroll_push: 0,
        }
    }

    /// Wrap a shelld-managed session.  marspot's main path uses
    /// this so an in-process update doesn't take the shell with it.
    pub fn new_shelld(session: ShelldSession) -> Self {
        Self {
            session: PaneBackend::Shelld(session),
            view_offset: 0,
            last_seen_scroll_push: 0,
        }
    }

    /// Returns the number of lines pushed into scrollback since the
    /// last call (then updates the bookmark).  Marspot calls this
    /// after each `pump` to keep live selections aligned with the
    /// content they were originally anchored to — every push moves
    /// the same content one row further from live bottom, so the
    /// selection's abs coords must rise by the same amount.
    pub fn drain_scroll_push_delta(&mut self) -> u64 {
        let now = self.session.terminal().grid().scroll_push_count();
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

    /// Handle a key event. Writes the encoded bytes to the PTY and
    /// snaps the view back to the live tail if it was scrolled into
    /// history. Returns `true` when the caller should `request_redraw`
    /// (only when the view offset changed; the byte write triggers a
    /// PTY wake → `pump` → redraw on its own).
    pub fn handle_key(&mut self, event: &MarspotKeyEvent, mods: Modifiers) -> bool {
        // Forward the terminal's DECCKM + bracketed-paste state so
        // arrow keys encode correctly for TUI apps in application
        // cursor key mode, and Cmd-V paste is wrapped in `\e[200~ /
        // \e[201~` when the app has opted in.
        let app_mode = self.session.terminal().cursor_key_application_mode();
        let bracketed = self.session.terminal().bracketed_paste_mode();
        let Some(bytes) = key_event_to_bytes(event, mods, app_mode, bracketed) else {
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
        let max = self.session.terminal().grid().scrollback_len() as i32;
        let new = (self.view_offset as i32 + delta).clamp(0, max) as u16;
        if new == self.view_offset {
            return false;
        }
        self.view_offset = new;
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
        true
    }

    /// Resize the terminal grid if `(cols, rows)` differ from the
    /// current shape. No-op when shape is unchanged — `Session::resize`
    /// is cheap but does a SIGWINCH to the shell, which some processes
    /// react to (clear-screen redraws, reflow); avoiding spurious
    /// resize keeps user experience quiet.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if (cols, rows)
            != (
                self.session.terminal().grid().cols(),
                self.session.terminal().grid().rows(),
            )
        {
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
    pub fn view<'a>(&'a self, focused: bool, title: &'a str) -> SessionView<'a> {
        SessionView {
            grid: self.session.terminal().grid(),
            view_offset: if focused { self.view_offset } else { 0 },
            cursor_visible: self.session.terminal().cursor_visible(),
            focused,
            title,
            selection: None,
            ime_preedit: "",
        }
    }
}

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

use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use objc2::runtime::ProtocolObject;
use objc2_metal::MTLTexture;

use marspot::input::{key_event_to_bytes, MarspotKeyEvent, Modifiers};
use marspot::iosurface::IOSurface;
use marspot::layout::Layout;
use marspot::pane::Pane;
use marspot::render::{SessionView, SidebarEntry};
use marspot::render_metal::MetalRenderer;
use marspot::session::SessionState;
use marspot::shell_proto::{
    decode_focus, decode_hello, decode_key_event, decode_mouse, decode_ping, decode_preedit,
    decode_resize, decode_scroll, encode_caret_rect, encode_hello_ack, encode_pong,
    encode_surface_ready, mods_to_struct, wire_to_event, Frame, MsgType, DEFAULT_CONTROL_FD,
    ENV_CONTROL_FD, ENV_SURFACE_HEIGHT, ENV_SURFACE_ID, ENV_SURFACE_SCALE, ENV_SURFACE_WIDTH,
    PROTO_VERSION,
};
use marspot::shelld_client::{default_socket_path, ShelldClient};
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
                eprintln!("[core] control read error: {e}");
                let _ = tx.send(CoreEvent::Closed);
                return;
            }
        }
    }
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
    }

    /// Spawn a fresh session and append it.  Refuses past
    /// `SESSION_COUNT_HARD_CAP`.
    fn spawn_session(&mut self) {
        if self.panes.len() >= SESSION_COUNT_HARD_CAP {
            return;
        }
        match self.client.new_session(INITIAL_COLS, INITIAL_ROWS, "") {
            Ok(s) => {
                self.panes.push(Pane::new_shelld(s));
                self.custom_titles.push(None);
            }
            Err(e) => {
                eprintln!("[core] failed to spawn session via shelld: {e}");
            }
        }
    }

    /// Terminate `panes[idx]` and keep all parallel state in sync.
    /// Caller refuses the call when it would leave zero sessions.
    fn close_session(&mut self, idx: usize) {
        if idx >= self.panes.len() {
            return;
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

    fn copy_selection_to_clipboard(&self) -> bool {
        let Some(sel) = self.selection else { return false };
        let Some(pane) = self.panes.get(sel.session_idx) else {
            return false;
        };
        match selection_text(pane, &sel) {
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
        let term = pane.session().terminal();
        let app_mode = term.cursor_key_application_mode();
        let bracketed = term.bracketed_paste_mode();
        if let Some(bytes) = key_event_to_bytes(&event, modifiers, app_mode, bracketed) {
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
                let rows = pane.session().terminal().grid().rows() as u32;
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
        let rows = pane.session().terminal().grid().rows() as u32;
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
            let term = pane.session().terminal();
            if !term.cursor_visible() {
                return None;
            }
            let (col, row) = term.grid().cursor();
            self.layout
                .caret_view_phys_rect(focused, col, row, cell_w, cell_h)
        })
    }
}

fn main() {
    eprintln!(
        "marspot-core {} (git {} built {})  pid={}",
        env!("CARGO_PKG_VERSION"),
        option_env!("MARSPOT_GIT_SHA").unwrap_or("unknown"),
        option_env!("MARSPOT_BUILD_TS").unwrap_or("unknown"),
        std::process::id()
    );

    let surface_id: u32 = env_required(ENV_SURFACE_ID);
    let w_phys: f64 = env_required(ENV_SURFACE_WIDTH);
    let h_phys: f64 = env_required(ENV_SURFACE_HEIGHT);
    let scale: f64 = env_required(ENV_SURFACE_SCALE);

    eprintln!("[core] attaching surface {surface_id} ({w_phys}×{h_phys} @ {scale}x)");

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

    let shelld_sock = default_socket_path();
    eprintln!("[core] connecting to shelld at {}", shelld_sock.display());
    let client = match ShelldClient::connect(&shelld_sock, wake) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("[core] shelld connect failed: {e}");
            return;
        }
    };

    // Bootstrap the full 9-grid: reattach every surviving shelld
    // session (full bytelog history replays on attach), then fill the
    // remaining slots with fresh sessions — same contract as the
    // standalone marspot's startup (src/main.rs).
    let layout_mode = LayoutMode::Nine;
    let n_sessions = layout_mode.cells();
    let existing: Vec<marspot::shelld_proto::SessionInfo> = client
        .list_sessions()
        .unwrap_or_else(|e| {
            eprintln!("[core] list_sessions failed: {e} — starting fresh");
            Vec::new()
        })
        .into_iter()
        .filter(|s| s.alive)
        .collect();
    let mut panes: Vec<Pane> = Vec::with_capacity(n_sessions);
    for info in existing.iter().take(n_sessions) {
        match client.attach(info.session_id, INITIAL_COLS, INITIAL_ROWS) {
            Ok(s) => panes.push(Pane::new_shelld(s)),
            Err(e) => {
                eprintln!("[core] attach {} failed: {e}", info.session_id);
            }
        }
    }
    while panes.len() < n_sessions {
        match client.new_session(INITIAL_COLS, INITIAL_ROWS, "") {
            Ok(s) => panes.push(Pane::new_shelld(s)),
            Err(e) => {
                eprintln!("[core] new_session failed: {e}");
                break;
            }
        }
    }
    if panes.is_empty() {
        eprintln!("[core] no sessions could be created — exiting");
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
    };
    app.rebuild_layout();
    eprintln!(
        "[core] layout {:?} ({} panes) cell0 = {} cols × {} rows",
        app.layout_mode,
        app.panes.len(),
        app.layout.cells[0].cols,
        app.layout.cells[0].rows
    );

    // Bring up the shell ↔ core control socket inherited as fd 3.
    let control_fd: RawFd = std::env::var(ENV_CONTROL_FD)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_CONTROL_FD);
    eprintln!("[core] taking control socket from fd {control_fd}");
    let control_stream = unsafe { UnixStream::from_raw_fd(control_fd) };
    let reader_stream = control_stream
        .try_clone()
        .expect("[core] try_clone control_stream");
    let mut control_writer = control_stream;
    let reader_tx = event_tx.clone();
    std::thread::spawn(move || reader_loop(reader_stream, reader_tx));

    eprintln!("[core] entering event loop (event-driven, no fixed cadence)");

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
            eprintln!("[core] control socket closed by shell; exiting event loop");
            break 'main;
        }
        for (ty, payload) in to_ack.drain(..) {
            let f = Frame::new(ty, payload);
            if let Err(e) = f.write_to(&mut control_writer) {
                eprintln!("[core] liveness ack {:?} write failed: {e}", ty);
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
                    eprintln!("[core] Resize: IOSurfaceLookup({new_id}) returned nil; dropping");
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
                            eprintln!("[core] SurfaceReady write failed: {e}");
                        }
                    }
                    Err(e) => {
                        eprintln!("[core] Resize: make_metal_texture failed: {e}");
                    }
                }
            }
        }
        app.pump_all();
        if app.all_exited {
            eprintln!("[core] all sessions exited; exiting cleanly");
            break 'main;
        }

        if app.needs_render {
            let caret = app.render(&target_tex);
            // Publish the focused-pane caret so the shell can anchor
            // the IME candidate window.  Dedupe — an idle cursor must
            // not stream identical frames at render cadence.
            if app.last_caret_sent != Some(caret) {
                app.last_caret_sent = Some(caret);
                let f = Frame::new(MsgType::CaretRect, encode_caret_rect(caret));
                if let Err(e) = f.write_to(&mut control_writer) {
                    eprintln!("[core] CaretRect write failed: {e}");
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
            eprintln!(
                "[core] frame {frame} t={t:.1}s panes={} focused={} ({state}) grid={}x{}",
                app.panes.len(),
                app.focused_idx,
                p.session().terminal().grid().cols(),
                p.session().terminal().grid().rows(),
            );
        }
    }
}

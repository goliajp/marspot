use objc2_app_kit::{NSScreen, NSView};
use objc2_foundation::MainThreadMarker;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{Modifiers, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

use mars::input::key_event_to_bytes;
use mars::layout::Layout;
use mars::render::{Renderer, SessionView, SidebarEntry};
use mars::session::{Session, SessionState};
use mars::terminal::Terminal;
use mars::tmux;

/// Mars's only proxy event — "something woke us up, drain all sessions".
#[derive(Debug, Clone)]
pub enum MarsEvent {
    Wake,
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const GIT_SHA: &str = env!("MARS_GIT_SHA");

/// Phase B: nine independent terminals in a 3×3 grid plus a sidebar.
/// In tmux mode this collapses to 1×1 (the tmux client renders one
/// active pane at a time; future work can split panes into cells).
const GRID_COLS_LAYOUT: usize = 3;
const GRID_ROWS_LAYOUT: usize = 3;
const TMUX_GRID_COLS: usize = 1;
const TMUX_GRID_ROWS: usize = 1;
const SIDEBAR_W_LOGICAL: f64 = 200.0;
/// Default window in logical points; physical pixels = logical × scale.
const DEFAULT_WIN_W: f64 = 1440.0;
const DEFAULT_WIN_H: f64 = 900.0;

/// Initial dimensions for sessions before the first Resized event sizes
/// them properly.  Resized fires almost immediately at startup.
const INITIAL_COLS: u16 = 40;
const INITIAL_ROWS: u16 = 12;

/// Sidebar layout constants in **logical points** — must match
/// `render.rs::SIDEBAR_TOP_PAD` / `SIDEBAR_ROW_H` so click hit-testing
/// lands on the same pixels as the drawn rows.
const SIDEBAR_TOP_PAD_PT: f64 = 14.0;
const SIDEBAR_ROW_PT: f64 = 22.0;

/// Headless modes (snapshot / bench parse / bench render) use a fixed
/// terminal grid so numbers are reproducible across runs.
const GRID_COLS: u16 = 80;
const GRID_ROWS: u16 = 24;

/// In tmux mode mars hosts a single `Session` running `tmux -CC` and
/// re-uses the rendering / sidebar machinery to surface tmux's
/// windows.  When `Some`, normal multi-cell behaviour is bypassed.
struct TmuxState {
    parser: tmux::Parser,
    /// Window list as we've seen it from %window-add / %window-renamed.
    /// Order is insertion-order; sidebar renders in this order.
    windows: Vec<TmuxWindow>,
    /// Currently-active window id (last %window-pane-changed / explicit
    /// select-window we sent).  None until tmux first tells us.
    active_window: Option<u32>,
    /// What we last asked tmux to do.  Used to route the next
    /// `%end` block back to the right parser.  `None` when no
    /// command is in flight.
    pending: Option<PendingCommand>,
    /// True until we've issued the initial `list-windows` query.
    needs_initial_query: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PendingCommand {
    ListWindows,
    /// `select-window` etc. — fire-and-forget, no response parsing.
    FireAndForget,
}

#[derive(Clone, Debug)]
struct TmuxWindow {
    id: u32,
    name: String,
    /// Wall-clock instant of the most recent `%output` we saw for any
    /// pane in this window.  Drives the sidebar's per-window state
    /// dot (Active vs Idle).
    last_output: Option<std::time::Instant>,
    /// Set once we see `%window-close` for this id.  Kept in the list
    /// briefly so the sidebar can render Exited; pruned afterwards.
    closed: bool,
}

impl TmuxState {
    fn new() -> Self {
        Self {
            parser: tmux::Parser::new(),
            windows: Vec::new(),
            active_window: None,
            pending: None,
            needs_initial_query: true,
        }
    }
}

struct Mars {
    window: Option<Window>,
    renderer: Option<Renderer>,
    /// Cached layout from the last Resized.  Drives both rendering and
    /// mouse-click hit-testing.
    layout: Option<Layout>,
    /// `Some` when launched with `--tmux`; otherwise we run the
    /// standard 9-cell grid.
    tmux: Option<TmuxState>,
    /// One Session per terminal cell on screen.
    sessions: Vec<Session>,
    /// Index into `sessions` of the session currently receiving keyboard
    /// input + mouse-wheel scrolling.
    focused_idx: usize,
    /// Scrollback view offset of the focused session.  Phase B keeps a
    /// single shared offset; Phase C will move this onto Session so each
    /// cell remembers its own scroll position.
    view_offset: u16,
    /// Cursor position on screen at the last MouseInput event — used so
    /// click hit-testing knows where the cursor was when the button
    /// went down.
    cursor_phys: (f64, f64),
    /// Latest known modifier state, updated by WindowEvent::ModifiersChanged.
    /// winit's KeyEvent does not carry the live modifier flags on macOS, so
    /// we have to track them out-of-band.
    modifiers: ModifiersState,
    /// Self-instrumentation: when set to `Some(t0)`, the next render that
    /// commits to the layer will measure `t0.elapsed()` as the
    /// keystroke-to-pixel latency and record it.  Cleared after the next
    /// successful layer.setContents.  Off-path entirely when MARS_LATENCY
    /// is unset (taken once at startup → `record_latency`).
    pending_keystroke_t0: Option<std::time::Instant>,
    /// Cumulative latency samples, written to MARS_LATENCY's path on Drop.
    /// Always allocated but only pushed to when `record_latency` is true.
    latency_samples: Vec<u64>,
    record_latency: bool,
    latency_out_path: Option<String>,
    /// MARS_PROFILE counters — set when MARS_PROFILE_OUT is configured.
    /// Counts paths through user_event / RedrawRequested / render / feed
    /// so we can tell whether the live pipeline is render-throttled,
    /// event-throttled, or feed-throttled.
    prof: ProfileCounters,
    profile_out_path: Option<String>,
}

#[derive(Default)]
struct ProfileCounters {
    user_events: u64,
    chunks_drained: u64,
    bytes_fed: u64,
    request_redraws: u64,
    redraw_requested_calls: u64,
    render_calls: u64,
    render_total_ns: u64,
    feed_total_ns: u64,
    drain_total_ns: u64,
    started_at: Option<std::time::Instant>,
}

impl ApplicationHandler<MarsEvent> for Mars {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let title = format!("Mars v{} ({})", VERSION, GIT_SHA);
        let attrs = Window::default_attributes()
            .with_title(title)
            .with_inner_size(LogicalSize::new(DEFAULT_WIN_W, DEFAULT_WIN_H));
        let window = event_loop.create_window(attrs).expect("create window");

        // mainScreen() can return a 1x screen even when our window will
        // land on a 2x one. Survey all screens and use the max so the
        // CALayer is composited at the right density.
        let main_thread = MainThreadMarker::new()
            .expect("Mars must be created on the main thread");
        let screens = NSScreen::screens(main_thread);
        let mut scales: Vec<f32> = Vec::new();
        for i in 0..screens.len() {
            let s = unsafe { screens.objectAtIndex(i) };
            scales.push(s.backingScaleFactor() as f32);
        }
        let max_scale = scales.iter().cloned().fold(1.0_f32, f32::max);
        let scale: f32 = std::env::var("MARS_SCALE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(max_scale);

        let renderer = unsafe {
            let handle = window.window_handle().expect("window handle").as_raw();
            let RawWindowHandle::AppKit(appkit) = handle else {
                panic!("Mars only supports the AppKit backend");
            };
            let nsview: &NSView = &*(appkit.ns_view.as_ptr() as *const NSView);
            Renderer::new(nsview, scale).expect("renderer init")
        };

        let size = window.inner_size();
        let phys_w = (size.width as f64) * (scale as f64);
        let phys_h = (size.height as f64) * (scale as f64);
        let mut renderer = renderer;
        renderer.resize(phys_w, phys_h);

        window.request_redraw();

        self.window = Some(window);
        self.renderer = Some(renderer);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: MarsEvent) {
        match event {
            MarsEvent::Wake => {
                self.prof.user_events += 1;
                let drain_t0 = std::time::Instant::now();
                let total_bytes = if self.tmux.is_some() {
                    self.pump_tmux_session()
                } else {
                    let mut total = 0;
                    for s in &mut self.sessions {
                        let feed_t0 = std::time::Instant::now();
                        let n = s.pump();
                        self.prof.feed_total_ns += feed_t0.elapsed().as_nanos() as u64;
                        total += n;
                    }
                    total
                };
                self.prof.bytes_fed += total_bytes as u64;
                self.prof.drain_total_ns += drain_t0.elapsed().as_nanos() as u64;
                if total_bytes > 0 {
                    if let Some(w) = &self.window {
                        w.request_redraw();
                        self.prof.request_redraws += 1;
                    }
                }
                // Multi-session: keep the window alive even when
                // individual cells exit — they'll just stop producing
                // bytes.  Phase D will draw an "exited" indicator in
                // the sidebar.  Quit only when *every* session is dead.
                if self.sessions.iter().all(|s| s.is_exited()) {
                    for s in &mut self.sessions {
                        s.pump();
                    }
                    event_loop.exit();
                }
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                // winit's PhysicalSize on macOS is already physical pixels.
                let phys_w = size.width as f64;
                let phys_h = size.height as f64;
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(phys_w, phys_h);
                    let (cell_w, cell_h) = r.cell_dims();
                    // Sidebar is logical-points; convert by querying current
                    // backing scale (1.0 fallback).
                    let scale = MainThreadMarker::new()
                        .and_then(NSScreen::mainScreen)
                        .map(|s| s.backingScaleFactor() as f64)
                        .unwrap_or(1.0);
                    let sidebar_phys = SIDEBAR_W_LOGICAL * scale;
                    let (lc, lr) = if self.tmux.is_some() {
                        (TMUX_GRID_COLS, TMUX_GRID_ROWS)
                    } else {
                        (GRID_COLS_LAYOUT, GRID_ROWS_LAYOUT)
                    };
                    let layout = Layout::build(
                        phys_w,
                        phys_h,
                        sidebar_phys,
                        lc,
                        lr,
                        cell_w,
                        cell_h,
                    );
                    // Resize every session to its layout cell.
                    for (i, s) in self.sessions.iter_mut().enumerate() {
                        if let Some(rect) = layout.cells.get(i) {
                            if (rect.cols, rect.rows)
                                != (s.terminal.grid().cols(), s.terminal.grid().rows())
                            {
                                s.resize(rect.cols, rect.rows);
                            }
                        }
                    }
                    self.layout = Some(layout);
                    // Sync render so the next CA commit lands a fresh
                    // CGImage at the new size — avoids the live-resize
                    // flicker we worked through earlier.
                    self.render_now();
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::Focused(focused) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.set_window_focused(focused);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let Some(bytes) = key_event_to_bytes(&event, self.modifiers) {
                    if self.record_latency && self.pending_keystroke_t0.is_none() {
                        self.pending_keystroke_t0 = Some(std::time::Instant::now());
                    }
                    // Typing snaps the focused session's view back to live.
                    if self.view_offset != 0 {
                        self.view_offset = 0;
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                    let _ = self.sessions[self.focused_idx].write(&bytes);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_phys = (position.x, position.y);
            }
            WindowEvent::MouseInput {
                state, button, ..
            } => {
                use winit::event::{ElementState, MouseButton};
                if state == ElementState::Pressed && button == MouseButton::Left {
                    if let Some(layout) = &self.layout {
                        let (px, py) = self.cursor_phys;
                        let scale = MainThreadMarker::new()
                            .and_then(NSScreen::mainScreen)
                            .map(|s| s.backingScaleFactor() as f64)
                            .unwrap_or(1.0);
                        let row_phys = SIDEBAR_ROW_PT * scale;
                        let top_pad_phys = SIDEBAR_TOP_PAD_PT * scale;

                        // In tmux mode, sidebar rows map to tmux windows;
                        // a click sends `select-window` to tmux instead
                        // of changing mars's focused_idx.
                        if self.tmux.is_some() {
                            let n_windows = self.tmux.as_ref().unwrap().windows.len();
                            if let Some(row) = layout.hit_test_sidebar_row(
                                px,
                                py,
                                top_pad_phys,
                                row_phys,
                                n_windows,
                            ) {
                                let target =
                                    self.tmux.as_ref().unwrap().windows.get(row).map(|w| w.id);
                                if let Some(id) = target {
                                    self.tmux_select_window(id);
                                    if let Some(w) = &self.window {
                                        w.request_redraw();
                                    }
                                }
                                return;
                            }
                            // Fall through: clicks on the (single) cell
                            // do nothing in tmux mode.
                            return;
                        }

                        let new_focus = layout
                            .hit_test_sidebar_row(
                                px,
                                py,
                                top_pad_phys,
                                row_phys,
                                self.sessions.len(),
                            )
                            .or_else(|| layout.hit_test(px, py));
                        if let Some(idx) = new_focus {
                            if idx < self.sessions.len() && idx != self.focused_idx {
                                self.focused_idx = idx;
                                self.view_offset = 0;
                                if let Some(w) = &self.window {
                                    w.request_redraw();
                                }
                            }
                        }
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let cell_h = self
                    .renderer
                    .as_ref()
                    .map(|r| r.cell_dims().1)
                    .unwrap_or(15.0);
                let lines_f = match delta {
                    MouseScrollDelta::LineDelta(_, y) => -(y as f64) * 3.0,
                    MouseScrollDelta::PixelDelta(p) => -p.y / cell_h,
                };
                if lines_f.abs() < 0.5 {
                    return;
                }
                let max = self.sessions[self.focused_idx]
                    .terminal
                    .grid()
                    .scrollback_len() as i32;
                let new = (self.view_offset as i32 + lines_f as i32).clamp(0, max) as u16;
                if new != self.view_offset {
                    self.view_offset = new;
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                self.prof.redraw_requested_calls += 1;
                let render_t0 = std::time::Instant::now();
                self.render_now();
                self.prof.render_total_ns += render_t0.elapsed().as_nanos() as u64;
                self.prof.render_calls += 1;
                if self.prof.started_at.is_none() {
                    self.prof.started_at = Some(std::time::Instant::now());
                }
                // Latency instrumentation — close the loop opened by
                // the most recent keystroke.
                if let Some(t0) = self.pending_keystroke_t0.take() {
                    let ns = t0.elapsed().as_nanos() as u64;
                    self.latency_samples.push(ns);
                }
            }
            _ => {}
        }
    }
}

impl Mars {
    /// In tmux mode: drain the single session's raw bytes, run them
    /// through `tmux::Parser`, and route extracted pane Output back
    /// to the terminal.  Window-state events update `tmux.windows`
    /// so the sidebar re-renders with the new list on the next
    /// redraw.  Returns the total bytes fed to the terminal.
    fn pump_tmux_session(&mut self) -> usize {
        let raw = self.sessions[0].drain_raw();
        if raw.is_empty() {
            return 0;
        }
        let events = self.tmux.as_mut().unwrap().parser.feed(&raw);
        let mut bytes_fed = 0;
        let mut got_signal_from_tmux = false;
        for ev in events {
            got_signal_from_tmux = true;
            if std::env::var("MARS_TMUX_DEBUG").is_ok() {
                eprintln!("[tmux] {:?}", ev);
            }
            match ev {
                tmux::Event::Output { bytes, .. } => {
                    bytes_fed += bytes.len();
                    // Mark the active window as recently-active so its
                    // sidebar dot turns green.  Only the attached
                    // client's window streams output in -CC mode, so
                    // tagging the active one is the right call.
                    let now = std::time::Instant::now();
                    let tmux = self.tmux.as_mut().unwrap();
                    if let Some(active) = tmux.active_window {
                        if let Some(w) = tmux.windows.iter_mut().find(|w| w.id == active) {
                            w.last_output = Some(now);
                        }
                    }
                    self.sessions[0].feed_terminal(&bytes);
                }
                tmux::Event::WindowAdd { window_id } => {
                    let tmux = self.tmux.as_mut().unwrap();
                    if !tmux.windows.iter().any(|w| w.id == window_id) {
                        tmux.windows.push(TmuxWindow {
                            id: window_id,
                            name: format!("@{}", window_id),
                            last_output: None,
                            closed: false,
                        });
                    }
                    // A new window appeared — refresh the list to pick
                    // up its name (tmux doesn't always send a separate
                    // %window-renamed for the default name).
                    self.queue_list_windows();
                }
                tmux::Event::WindowClose { window_id } => {
                    let tmux = self.tmux.as_mut().unwrap();
                    if let Some(w) = tmux.windows.iter_mut().find(|w| w.id == window_id) {
                        w.closed = true;
                    }
                }
                tmux::Event::WindowRenamed { window_id, name } => {
                    let tmux = self.tmux.as_mut().unwrap();
                    if let Some(w) = tmux.windows.iter_mut().find(|w| w.id == window_id) {
                        w.name = name;
                    } else {
                        tmux.windows.push(TmuxWindow {
                            id: window_id,
                            name,
                            last_output: None,
                            closed: false,
                        });
                    }
                }
                tmux::Event::WindowPaneChanged { window_id, .. } => {
                    self.tmux.as_mut().unwrap().active_window = Some(window_id);
                }
                tmux::Event::SessionsChanged => {
                    // Window list might have changed in a way we can't
                    // infer from per-window events alone — re-query.
                    self.queue_list_windows();
                }
                tmux::Event::End { output, .. } => {
                    // Always try to interpret %end blocks as window-list
                    // format.  tmux's attach handshake unsolicitedly
                    // emits such a block, and we want it as much as we
                    // want our own list-windows response.  Parsing is
                    // tolerant: if output isn't `@<id> <name>\n` lines,
                    // ingest leaves the windows list alone.
                    let tmux = self.tmux.as_mut().unwrap();
                    Self::ingest_list_windows_response(tmux, &output);
                    tmux.pending = None;
                }
                tmux::Event::CommandError { .. } => {
                    // tmux rejected our command (probably timing —
                    // we sent before tmux finished initialising).
                    // Just clear pending; queue_list_windows will
                    // retry when a future event prompts us.
                    self.tmux.as_mut().unwrap().pending = None;
                }
                tmux::Event::Exit { .. } => {
                    // Tmux server exited; let the all-sessions-exited
                    // path close mars naturally.
                }
                _ => {}
            }
        }
        // First time we hear from tmux at all — refresh the client so
        // the current pane re-streams its visible content (tmux doesn't
        // auto-redraw on attach), and request the window list in case
        // the implicit handshake didn't already include it.
        if got_signal_from_tmux {
            let needs_init = self.tmux.as_mut().unwrap().needs_initial_query;
            if needs_init {
                self.tmux.as_mut().unwrap().needs_initial_query = false;
                let _ = self.sessions[0].write(b"refresh-client\n");
                self.queue_list_windows();
            }
        }
        bytes_fed
    }

    /// Send `list-windows -F "#{window_id} #{window_name}"` to tmux,
    /// flagging that we're expecting a parseable response.  No-op if a
    /// command is already in flight (we serialise — multiple in-flight
    /// commands would need response routing we don't have).
    fn queue_list_windows(&mut self) {
        let tmux = match self.tmux.as_mut() {
            Some(t) => t,
            None => return,
        };
        if tmux.pending.is_some() {
            return;
        }
        tmux.pending = Some(PendingCommand::ListWindows);
        let _ = self.sessions[0].write(b"list-windows -F \"#{window_id} #{window_name}\"\n");
    }

    /// Send `select-window -t @<id>` for the user-clicked window.
    /// Fire-and-forget: tmux will emit %window-pane-changed when the
    /// switch lands, which updates `active_window` for us.
    fn tmux_select_window(&mut self, window_id: u32) {
        let tmux = match self.tmux.as_mut() {
            Some(t) => t,
            None => return,
        };
        // Don't queue if list-windows is in flight; select-window's
        // own response can collide.  In practice they're cheap and
        // serial; rare flap is acceptable for v1.
        if tmux.pending.is_none() {
            tmux.pending = Some(PendingCommand::FireAndForget);
        }
        let cmd = format!("select-window -t @{}\n", window_id);
        let _ = self.sessions[0].write(cmd.as_bytes());
    }

    /// Parse the body of a list-windows %end block — one
    /// `@<id> <name>` per line — and reconcile with `tmux.windows`.
    /// Preserves `last_output` on existing windows; appends new ones;
    /// drops entries that vanished from the new list.
    fn ingest_list_windows_response(tmux: &mut TmuxState, output: &[u8]) {
        let mut new_list: Vec<TmuxWindow> = Vec::new();
        for line in output.split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            let s = match std::str::from_utf8(line) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let s = s.trim();
            if !s.starts_with('@') {
                continue;
            }
            let mut iter = s[1..].splitn(2, ' ');
            let id: u32 = match iter.next().and_then(|t| t.parse().ok()) {
                Some(n) => n,
                None => continue,
            };
            let name = iter.next().unwrap_or("").to_string();
            // Carry over last_output if we already had this window.
            let last_output = tmux
                .windows
                .iter()
                .find(|w| w.id == id)
                .and_then(|w| w.last_output);
            new_list.push(TmuxWindow {
                id,
                name,
                last_output,
                closed: false,
            });
        }
        if !new_list.is_empty() {
            tmux.windows = new_list;
        }
    }

    /// Build a SessionView for each session and hand them all to the
    /// renderer.  Used by both RedrawRequested and the synchronous
    /// path in WindowEvent::Resized.
    fn render_now(&mut self) {
        if self.layout.is_none() || self.renderer.is_none() {
            return;
        }
        let focused = self.focused_idx;
        let view_offset = self.view_offset;

        // Sidebar source-of-truth depends on mode: in tmux mode, list
        // tmux windows; otherwise list sessions by ordinal number.
        const PER_WINDOW_ACTIVE_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);
        if let Some(t) = &self.tmux {
            if std::env::var("MARS_TMUX_DEBUG").is_ok() {
                eprintln!("[render] tmux.windows.len()={}", t.windows.len());
            }
        }
        let (labels, states, sidebar_focus) = if let Some(t) = &self.tmux {
            if t.windows.is_empty() {
                (
                    vec!["(no windows)".to_string()],
                    vec![SessionState::Idle],
                    0usize,
                )
            } else {
                let labels: Vec<String> = t.windows.iter().map(|w| w.name.clone()).collect();
                let states: Vec<SessionState> = t
                    .windows
                    .iter()
                    .map(|w| {
                        if w.closed {
                            SessionState::Exited
                        } else {
                            match w.last_output {
                                Some(t) if t.elapsed() < PER_WINDOW_ACTIVE_WINDOW => {
                                    SessionState::Active
                                }
                                _ => SessionState::Idle,
                            }
                        }
                    })
                    .collect();
                let active = t.active_window;
                let sidebar_focus = t
                    .windows
                    .iter()
                    .position(|w| Some(w.id) == active)
                    .unwrap_or(0);
                (labels, states, sidebar_focus)
            }
        } else {
            (
                (1..=self.sessions.len()).map(|n| n.to_string()).collect(),
                self.sessions.iter().map(|s| s.state()).collect(),
                focused,
            )
        };

        let views: Vec<SessionView> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(i, s)| SessionView {
                grid: s.terminal.grid(),
                view_offset: if i == focused { view_offset } else { 0 },
                cursor_visible: s.terminal.cursor_visible(),
                focused: i == focused,
            })
            .collect();
        let entries: Vec<SidebarEntry> = labels
            .iter()
            .zip(states.iter())
            .map(|(label, state)| SidebarEntry {
                label: label.as_str(),
                state: *state,
            })
            .collect();
        let layout = self.layout.as_ref().unwrap();
        let renderer = self.renderer.as_mut().unwrap();
        renderer.render_layout(layout, &views, &entries, sidebar_focus);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(path) = parse_named_arg(&args, "--snapshot") {
        run_snapshot(&path);
        return;
    }
    if let Some(spec) = parse_named_arg(&args, "--bench") {
        run_bench(&spec);
        return;
    }

    let event_loop: EventLoop<MarsEvent> = EventLoop::with_user_event()
        .build()
        .expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    // Every session's reader thread calls the same closure on chunk +
    // EOF; we forward to the winit event loop as Wake.
    let proxy = event_loop.create_proxy();

    let tmux_mode = args.iter().any(|a| a == "--tmux");
    let n_sessions = if tmux_mode {
        TMUX_GRID_COLS * TMUX_GRID_ROWS
    } else {
        GRID_COLS_LAYOUT * GRID_ROWS_LAYOUT
    };

    let mut sessions = Vec::with_capacity(n_sessions);
    for _ in 0..n_sessions {
        let proxy_clone = proxy.clone();
        let wake = move || {
            let _ = proxy_clone.send_event(MarsEvent::Wake);
        };
        let s = if tmux_mode {
            // tmux -CC: attach to "mars" session (creating it if absent).
            // -A is "attach if exists, else new" — handy for re-launches.
            Session::spawn_with(
                "tmux",
                &["-CC", "new-session", "-A", "-s", "mars"],
                INITIAL_COLS,
                INITIAL_ROWS,
                wake,
            )
            .expect("spawn tmux -CC session")
        } else {
            Session::spawn(INITIAL_COLS, INITIAL_ROWS, wake)
                .expect("spawn initial session")
        };
        sessions.push(s);
    }

    let latency_out_path = std::env::var("MARS_LATENCY").ok();
    let record_latency = latency_out_path.is_some();
    let profile_out_path = std::env::var("MARS_PROFILE").ok();

    let mut app = Mars {
        window: None,
        renderer: None,
        layout: None,
        tmux: if tmux_mode { Some(TmuxState::new()) } else { None },
        sessions,
        focused_idx: 0,
        view_offset: 0,
        cursor_phys: (0.0, 0.0),
        modifiers: ModifiersState::empty(),
        pending_keystroke_t0: None,
        latency_samples: Vec::new(),
        record_latency,
        latency_out_path,
        prof: ProfileCounters::default(),
        profile_out_path,
    };
    event_loop.run_app(&mut app).expect("run app");
}

impl Drop for Mars {
    fn drop(&mut self) {
        if let Some(path) = self.latency_out_path.take() {
            let mut s = String::with_capacity(self.latency_samples.len() * 12);
            s.push('[');
            for (i, ns) in self.latency_samples.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&ns.to_string());
            }
            s.push(']');
            if let Err(e) = std::fs::write(&path, s) {
                eprintln!("mars: failed to write latency log to {path}: {e}");
            }
        }
        if let Some(path) = self.profile_out_path.take() {
            let p = &self.prof;
            let json = format!(
                r#"{{"user_events":{ue},"chunks_drained":{cd},"bytes_fed":{bf},"request_redraws":{rr},"redraw_requested_calls":{rrc},"render_calls":{rc},"render_total_ns":{rtn},"feed_total_ns":{ftn},"drain_total_ns":{dtn}}}"#,
                ue = p.user_events,
                cd = p.chunks_drained,
                bf = p.bytes_fed,
                rr = p.request_redraws,
                rrc = p.redraw_requested_calls,
                rc = p.render_calls,
                rtn = p.render_total_ns,
                ftn = p.feed_total_ns,
                dtn = p.drain_total_ns,
            );
            if let Err(e) = std::fs::write(&path, json) {
                eprintln!("mars: failed to write profile log to {path}: {e}");
            }
        }
    }
}

fn parse_named_arg(args: &[String], name: &str) -> Option<String> {
    let mut iter = args.iter().skip(1);
    let prefix = format!("{}=", name);
    while let Some(a) = iter.next() {
        if a == name {
            return iter.next().cloned();
        }
        if let Some(rest) = a.strip_prefix(prefix.as_str()) {
            return Some(rest.to_string());
        }
    }
    None
}

/// Headless render: build a Renderer with no view, render one frame into
/// an offscreen bitmap, encode as PNG, write to `path`. The terminal is
/// pre-loaded with a demo banner so the snapshot has visible content
/// without needing a live PTY.
fn run_snapshot(path: &str) {
    let scale: f32 = std::env::var("MARS_SNAPSHOT_SCALE")
        .ok()
        .and_then(|s| s.parse().ok())
        .or_else(|| {
            MainThreadMarker::new()
                .and_then(NSScreen::mainScreen)
                .map(|s| s.backingScaleFactor() as f32)
        })
        .unwrap_or(1.0);
    let logical_w: u32 = 960;
    let logical_h: u32 = 600;
    let phys_w = (logical_w as f32 * scale) as u32;
    let phys_h = (logical_h as f32 * scale) as u32;

    let mut terminal = Terminal::new(GRID_COLS, GRID_ROWS);
    feed_demo_content(&mut terminal);

    let mut renderer = Renderer::new_offscreen(scale).expect("offscreen renderer");
    renderer.resize(phys_w as f64, phys_h as f64);
    let bgra = renderer
        .snapshot(phys_w, phys_h, terminal.grid())
        .expect("snapshot");

    let mut rgba = bgra.clone();
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }

    let file = std::fs::File::create(path).expect("create snapshot file");
    let buf_writer = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(buf_writer, phys_w, phys_h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(&rgba).expect("png write");

    eprintln!(
        "wrote snapshot: {} ({}x{} physical, scale={})",
        path, phys_w, phys_h, scale
    );
}

/// Headless benchmark dispatcher.  Spec is `<mode>:<arg>`.
///
/// Modes:
///   parse:<path>   feed bytes from `path` through Terminal::feed and
///                  report wall-clock throughput (bytes/sec)
///   render:<n>     run `n` full-frame renders against a synthetic
///                  worst-case grid (full coloured cells), report
///                  per-frame p50/p95/p99 nanoseconds
///
/// Both modes write a single line of JSON to stdout so harness scripts
/// can grep / parse without depending on prose formatting.
fn run_bench(spec: &str) {
    let (mode, arg) = match spec.split_once(':') {
        Some(p) => p,
        None => {
            eprintln!(
                "--bench expects <mode>:<arg>, e.g. parse:/tmp/cat-ascii.bin or render:1000"
            );
            std::process::exit(2);
        }
    };
    match mode {
        "parse" => bench_parse(arg),
        "render" => bench_render(arg),
        other => {
            eprintln!("unknown bench mode: {other}");
            std::process::exit(2);
        }
    }
}

fn bench_parse(path: &str) {
    let bytes = std::fs::read(path).unwrap_or_else(|e| {
        eprintln!("bench: read {path}: {e}");
        std::process::exit(2);
    });
    // Use the same grid dimensions as a typical mars window (auto-fit
    // 122×39 on the user's default 960×600 layout) so the parser path
    // exercises wrap / scroll the way it does in real use.
    let mut terminal = Terminal::new(GRID_COLS, GRID_ROWS);
    let t0 = std::time::Instant::now();
    terminal.feed(&bytes);
    let elapsed_ns = t0.elapsed().as_nanos() as u64;
    let bytes_per_sec = if elapsed_ns > 0 {
        (bytes.len() as u128 * 1_000_000_000 / elapsed_ns as u128) as u64
    } else {
        0
    };
    println!(
        r#"{{"mode":"parse","path":"{}","bytes":{},"elapsed_ns":{},"bytes_per_sec":{}}}"#,
        path,
        bytes.len(),
        elapsed_ns,
        bytes_per_sec
    );
}

fn bench_render(arg: &str) {
    let n: u32 = arg.parse().unwrap_or_else(|_| {
        eprintln!("bench: render needs an integer iteration count");
        std::process::exit(2);
    });

    // Build a worst-case grid: every cell carries a non-default fg colour
    // (forces a SetRGBFillColor per glyph run), every cell is non-blank
    // (no skipping), and the content alternates printable ASCII so glyph
    // run-length compression has to stop frequently.  This is the upper
    // bound on per-frame cost given the current architecture.
    let mut terminal = Terminal::new(GRID_COLS, GRID_ROWS);
    // Fill with rotating SGR colours + printable ASCII.
    let mut payload: Vec<u8> = Vec::with_capacity(64 * 1024);
    for r in 0..GRID_ROWS {
        for c in 0..GRID_COLS {
            // SGR 30..=37 cycling foreground.
            let colour = 30 + ((r as u32 + c as u32) % 8) as u8;
            payload.extend_from_slice(format!("\x1b[{}m", colour).as_bytes());
            let ch = ((c % 95) as u8) + 32; // printable ASCII 32..127
            payload.push(ch);
        }
        if r + 1 < GRID_ROWS {
            payload.extend_from_slice(b"\r\n");
        }
    }
    terminal.feed(&payload);

    // Render headlessly into an offscreen Renderer.  Use scale=1 to
    // match the user's display so numbers transfer to the live path.
    let mut renderer = Renderer::new_offscreen(1.0).expect("offscreen renderer");
    // Default 960×600 logical → physical at scale 1.
    let phys_w = 960.0_f64;
    let phys_h = 600.0_f64;

    // Warm-up: 5 iterations to fill char_cache and prime CGContext alloc.
    for _ in 0..5 {
        let _ = renderer.snapshot(phys_w as u32, phys_h as u32, terminal.grid());
    }

    let mut samples: Vec<u64> = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let t0 = std::time::Instant::now();
        let _ = renderer.snapshot(phys_w as u32, phys_h as u32, terminal.grid());
        samples.push(t0.elapsed().as_nanos() as u64);
    }
    samples.sort_unstable();
    let p = |q: f64| -> u64 {
        let idx = ((samples.len() as f64) * q) as usize;
        samples[idx.min(samples.len() - 1)]
    };
    println!(
        r#"{{"mode":"render","iterations":{},"p50_ns":{},"p95_ns":{},"p99_ns":{},"min_ns":{},"max_ns":{}}}"#,
        n,
        p(0.50),
        p(0.95),
        p(0.99),
        samples[0],
        samples[samples.len() - 1],
    );
}

fn feed_demo_content(terminal: &mut Terminal) {
    let banner = format!("mars v{} ({})\r\n", VERSION, GIT_SHA);
    terminal.feed(banner.as_bytes());
    terminal.feed(b"\r\n");
    terminal.feed(b"hello mars\r\n");
    terminal.feed(b"the engine is alive\r\n");
    terminal.feed(b"\r\n");
    terminal.feed(b"  pty + parser + grid + render (CoreText)\r\n");
    terminal.feed(b"\r\n");
    terminal.feed(b"  ascii printable: !\"#$%&'()*+,-./0123456789:;<=>?@\r\n");
    terminal.feed(b"                   ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_\r\n");
    terminal.feed(b"                   `abcdefghijklmnopqrstuvwxyz{|}~\r\n");
}

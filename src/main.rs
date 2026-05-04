use objc2_app_kit::NSScreen;
use objc2_foundation::MainThreadMarker;

use mars::app::{run_app, EventProxy, MarsApp, MarsAppCtx, WindowAttrs};
use mars::input::{key_event_to_bytes, MarsKeyEvent, Modifiers as MarsModifiers};
use mars::layout::Layout;
use mars::render::{Renderer, SessionView, SidebarEntry};
use mars::render_metal::{make_target_texture, MetalRenderer};
use mars::session::{Session, SessionState};
use mars::terminal::Terminal;
use mars::tmux;

/// Renderer dispatch: Metal-on-CAMetalLayer (default — 7× faster
/// typing latency per docs/perf.md) or AppKit-on-CGImage (set
/// `MARS_APPKIT=1`, kept as a fallback for regression bisects /
/// troubleshooting).  Both implement the same surface — keep this
/// enum in lock-step with their public API.
enum RendererImpl {
    Appkit(Renderer),
    Metal(MetalRenderer),
}

impl RendererImpl {
    fn cell_dims(&self) -> (f64, f64) {
        match self {
            Self::Appkit(r) => r.cell_dims(),
            Self::Metal(r) => r.cell_dims(),
        }
    }
    fn resize(&mut self, w: f64, h: f64) {
        match self {
            Self::Appkit(r) => r.resize(w, h),
            Self::Metal(r) => r.resize(w, h),
        }
    }
    fn set_window_focused(&mut self, focused: bool) {
        match self {
            Self::Appkit(r) => r.set_window_focused(focused),
            Self::Metal(r) => r.set_window_focused(focused),
        }
    }
    fn render_layout(
        &mut self,
        layout: &Layout,
        views: &[SessionView],
        sidebar: &[SidebarEntry],
        focused_idx: usize,
    ) {
        match self {
            Self::Appkit(r) => r.render_layout(layout, views, sidebar, focused_idx),
            Self::Metal(r) => r.render_layout(layout, views, sidebar, focused_idx),
        }
    }
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
/// 2100×1300 means a 3×3 grid fits ~75 cols × 30 rows per cell with
/// Monaco 12 — usable for real shell work, not just a "9 dots in a
/// row" demo.  macOS auto-clamps to the display's content rect, so
/// users on smaller screens get the largest window that fits.
const DEFAULT_WIN_W: f64 = 2100.0;
const DEFAULT_WIN_H: f64 = 1300.0;

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
    renderer: Option<RendererImpl>,
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

impl MarsApp for Mars {
    fn resumed(&mut self, ctx: &MarsAppCtx) {
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

        let nsview = ctx.ns_view();
        // Metal is the default; MARS_APPKIT=1 falls back to the
        // AppKit/CGImage path (kept for regression bisects).
        let renderer = if std::env::var("MARS_APPKIT").as_deref() == Ok("1") {
            eprintln!("[mars] MARS_APPKIT=1 → using AppKit Renderer");
            RendererImpl::Appkit(
                Renderer::new(nsview, scale).expect("renderer init"),
            )
        } else {
            RendererImpl::Metal(
                MetalRenderer::new(nsview, scale).expect("metal renderer init"),
            )
        };
        self.renderer = Some(renderer);
        // run_app delivers an explicit Resized after resumed; that does
        // the renderer.resize + layout build + initial render.
    }

    fn user_event(&mut self, ctx: &MarsAppCtx) {
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
            ctx.request_redraw();
            self.prof.request_redraws += 1;
        }
        // Multi-session: keep the window alive even when individual
        // cells exit — they'll just stop producing bytes.  Phase D
        // will draw an "exited" indicator in the sidebar.  Quit only
        // when *every* session is dead.
        if self.sessions.iter().all(|s| s.is_exited()) {
            for s in &mut self.sessions {
                s.pump();
            }
            ctx.exit();
        }
    }

    fn key_event(&mut self, ctx: &MarsAppCtx, event: MarsKeyEvent, modifiers: MarsModifiers) {
        if let Some(bytes) = key_event_to_bytes(&event, modifiers) {
            if self.record_latency && self.pending_keystroke_t0.is_none() {
                self.pending_keystroke_t0 = Some(std::time::Instant::now());
            }
            // Typing snaps the focused session's view back to live.
            if self.view_offset != 0 {
                self.view_offset = 0;
                ctx.request_redraw();
            }
            let session = &mut self.sessions[self.focused_idx];
            let _ = session.write(&bytes);
            // Local-echo: paint each printable-ASCII byte to the grid
            // immediately, ahead of the PTY round trip.
            // Terminal::predict_byte is a no-op for bytes that aren't
            // safe to predict (control chars, alt-screen mode, atlas
            // full, etc.).
            let mut predicted = false;
            for &b in bytes.as_ref() {
                if session.terminal.predict_byte(b) {
                    predicted = true;
                }
            }
            if predicted {
                ctx.request_redraw();
            }
        }
    }

    fn mouse_down(&mut self, ctx: &MarsAppCtx, x_phys: f64, y_phys: f64) {
        let Some(layout) = &self.layout else { return };
        let scale = ctx.scale();
        let row_phys = SIDEBAR_ROW_PT * scale;
        let top_pad_phys = SIDEBAR_TOP_PAD_PT * scale;

        // In tmux mode, sidebar rows map to tmux windows; a click
        // sends `select-window` to tmux instead of changing
        // focused_idx.
        if self.tmux.is_some() {
            let n_windows = self.tmux.as_ref().unwrap().windows.len();
            if let Some(row) = layout.hit_test_sidebar_row(
                x_phys, y_phys, top_pad_phys, row_phys, n_windows,
            ) {
                let target = self
                    .tmux
                    .as_ref()
                    .unwrap()
                    .windows
                    .get(row)
                    .map(|w| w.id);
                if let Some(id) = target {
                    self.tmux_select_window(id);
                    ctx.request_redraw();
                }
            }
            // Clicks on the (single) cell do nothing in tmux mode.
            return;
        }

        let new_focus = layout
            .hit_test_sidebar_row(
                x_phys,
                y_phys,
                top_pad_phys,
                row_phys,
                self.sessions.len(),
            )
            .or_else(|| layout.hit_test(x_phys, y_phys));
        if let Some(idx) = new_focus {
            if idx < self.sessions.len() && idx != self.focused_idx {
                self.focused_idx = idx;
                self.view_offset = 0;
                ctx.request_redraw();
            }
        }
    }

    fn scroll(&mut self, ctx: &MarsAppCtx, _dx_phys: f64, dy_phys: f64, precise: bool) {
        let cell_h = self
            .renderer
            .as_ref()
            .map(|r| r.cell_dims().1)
            .unwrap_or(15.0);
        // Match iTerm2 / native macOS scrolling: with the OS-level
        // natural-scrolling preference on (the default), swiping
        // FINGER DOWN on the trackpad reveals earlier content (look
        // back into scrollback).  The OS already gives the right
        // sign in scrollingDeltaY for that mapping; no negation
        // needed (the previous negation inverted the gesture and
        // felt wrong to users coming from iTerm2).
        //
        // Configurable via env (read once on first scroll):
        //   MARS_SCROLL_INVERT=1     flip direction (for users who
        //                            keep "natural scroll" off in
        //                            System Settings or just prefer
        //                            it that way).
        //   MARS_SCROLL_FACTOR=<f>   multiplier; default 1.0.  Use
        //                            0.5 for slower, 2.0 for faster.
        //                            Trackpad path divides by cell_h
        //                            so the factor scales line count
        //                            proportionally.
        let (invert, factor) = scroll_config();
        let sign: f64 = if invert { -1.0 } else { 1.0 };
        let lines_f = if precise {
            sign * factor * dy_phys / cell_h
        } else {
            sign * factor * dy_phys * 3.0
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
            ctx.request_redraw();
        }
    }

    fn resized(&mut self, ctx: &MarsAppCtx, phys_w: f64, phys_h: f64) {
        if let Some(r) = self.renderer.as_mut() {
            r.resize(phys_w, phys_h);
            let (cell_w, cell_h) = r.cell_dims();
            let scale = ctx.scale();
            let sidebar_phys = SIDEBAR_W_LOGICAL * scale;
            let (lc, lr) = if self.tmux.is_some() {
                (TMUX_GRID_COLS, TMUX_GRID_ROWS)
            } else {
                (GRID_COLS_LAYOUT, GRID_ROWS_LAYOUT)
            };
            let layout = Layout::build(
                phys_w, phys_h, sidebar_phys, lc, lr, cell_w, cell_h,
            );
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
            // Sync render so the next CA commit lands a fresh frame
            // at the new size — avoids the live-resize flicker.
            self.render_now();
        }
    }

    fn focused(&mut self, ctx: &MarsAppCtx, focused: bool) {
        if let Some(r) = self.renderer.as_mut() {
            r.set_window_focused(focused);
            ctx.request_redraw();
        }
    }

    fn close_requested(&mut self, ctx: &MarsAppCtx) {
        ctx.exit();
    }

    fn redraw(&mut self, _ctx: &MarsAppCtx) {
        self.prof.redraw_requested_calls += 1;
        let render_t0 = std::time::Instant::now();
        self.render_now();
        self.prof.render_total_ns += render_t0.elapsed().as_nanos() as u64;
        self.prof.render_calls += 1;
        if self.prof.started_at.is_none() {
            self.prof.started_at = Some(std::time::Instant::now());
        }
        // Latency instrumentation — close the loop opened by the most
        // recent keystroke.
        if let Some(t0) = self.pending_keystroke_t0.take() {
            let ns = t0.elapsed().as_nanos() as u64;
            self.latency_samples.push(ns);
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

    install_shell_zdot_shim();

    // Every session's reader thread calls the same closure on chunk +
    // EOF; we forward to the AppKit run loop as user_event.
    let proxy = EventProxy::new();

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
            proxy_clone.wake();
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

    let app = Mars {
        renderer: None,
        layout: None,
        tmux: if tmux_mode { Some(TmuxState::new()) } else { None },
        sessions,
        focused_idx: 0,
        view_offset: 0,
        pending_keystroke_t0: None,
        latency_samples: Vec::new(),
        record_latency,
        latency_out_path,
        prof: ProfileCounters::default(),
        profile_out_path,
    };

    let attrs = WindowAttrs {
        title: format!("Mars v{} ({})", VERSION, GIT_SHA),
        width_logical: DEFAULT_WIN_W,
        height_logical: DEFAULT_WIN_H,
    };
    run_app(app, proxy, attrs);
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

/// Lay down a per-user ZDOTDIR shim so the spawned zsh sources the
/// user's real `~/.zshrc`, then `unsetopt PROMPT_SP` so zsh doesn't
/// emit a reverse-video `%` ("PROMPT_EOL_MARK") on every fresh
/// session.
///
/// Why: mars's parser doesn't fully reconcile the byte sequence zsh
/// emits when PROMPT_SP fires (the `%` mark + line-fill + CR + space
/// + CR + `\033[J` + ...).  The space at col 0 should overwrite the
/// `%` cell, but mars leaves it visible.  Until the parser bug is
/// found, suppress the trigger at the shell level.
///
/// Side-effects: mars sets `ZDOTDIR` for the whole process so all
/// child shells pick it up.  The shim sources $HOME/.zshrc so the
/// user's normal init still runs.
fn install_shell_zdot_shim() {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return,
    };
    let dir = std::path::PathBuf::from(&home).join(".cache/mars/zdot");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    // Write/overwrite the shim every launch so updates ship without
    // user intervention.  The body is short and deterministic — diff
    // before write would be just-as-much I/O.
    let shim = r#"# Auto-generated by mars: ZDOTDIR shim that sources the user's
# real .zshrc, then disables zsh's PROMPT_SP option (and clears
# PROMPT_EOL_MARK as belt-and-braces) so fresh sessions don't show a
# reverse-video "%" mark before the prompt.
[[ -f "$HOME/.zshrc" ]] && source "$HOME/.zshrc"
unsetopt PROMPT_SP 2>/dev/null
PROMPT_EOL_MARK=""
"#;
    let path = dir.join(".zshrc");
    let _ = std::fs::write(&path, shim);
    // SAFETY: we're at startup, no threads have spawned yet.  This
    // env-var change is inherited by every forkpty child.
    unsafe { std::env::set_var("ZDOTDIR", &dir) };
}

/// Read scroll behaviour overrides from env once.  See `MarsApp::scroll`
/// for the default mapping.  Returns `(invert, factor)`.
fn scroll_config() -> (bool, f64) {
    use std::sync::OnceLock;
    static CFG: OnceLock<(bool, f64)> = OnceLock::new();
    *CFG.get_or_init(|| {
        let invert = std::env::var("MARS_SCROLL_INVERT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let factor = std::env::var("MARS_SCROLL_FACTOR")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|f| *f > 0.0 && *f < 100.0)
            .unwrap_or(1.0);
        (invert, factor)
    })
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
///   scroll:<path>[:<start>[:<step>]]
///                  feed `path` to populate scrollback, then walk
///                  view_offset from `start` (default 5000) toward 0
///                  in `step`-line decrements (default 3 — one wheel
///                  detent) and time each viewport repaint via
///                  `Grid::cell_at_view`.  Reports per-tick p50/p95/p99
///                  nanoseconds.  Picks up `MARS_DISK_SCROLLBACK` so
///                  the same harness can probe both storage variants.
///
/// All modes write a single line of JSON to stdout so harness scripts
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
        "metal-render" => bench_metal_render(arg),
        "scroll" => bench_scroll(arg, /* cold */ false),
        "scroll-cold" => bench_scroll(arg, /* cold */ true),
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

/// Metal counterpart to `bench_render`.  Same worst-case grid (every
/// cell coloured + non-blank, alternating ASCII so glyph runs break
/// frequently) and same 960×600 target dims, but routes through
/// `MetalRenderer::render_layout_to_texture` so the timing covers
/// `build_instances` + BG/FG-pass encoding + GPU execution
/// (waitUntilCompleted blocks until the frame is fully rendered).
///
/// Apples-to-apples vs `--bench render` modulo two intentional
/// differences:
///   * No CGImage create + setContents (Metal renders straight to
///     the target).  This is the architectural reason for using Metal.
///   * No CPU-side pixel readback (the texture is StorageModePrivate).
///     The AppKit path's `snapshot` does include the BGRA copy-out.
fn bench_metal_render(arg: &str) {
    let n: u32 = arg.parse().unwrap_or_else(|_| {
        eprintln!("bench: metal-render needs an integer iteration count");
        std::process::exit(2);
    });

    let mut terminal = Terminal::new(GRID_COLS, GRID_ROWS);
    let mut payload: Vec<u8> = Vec::with_capacity(64 * 1024);
    for r in 0..GRID_ROWS {
        for c in 0..GRID_COLS {
            let colour = 30 + ((r as u32 + c as u32) % 8) as u8;
            payload.extend_from_slice(format!("\x1b[{}m", colour).as_bytes());
            let ch = ((c % 95) as u8) + 32;
            payload.push(ch);
        }
        if r + 1 < GRID_ROWS {
            payload.extend_from_slice(b"\r\n");
        }
    }
    terminal.feed(&payload);

    let mut renderer = MetalRenderer::new_headless().expect("headless metal renderer");
    let phys_w: u32 = 960;
    let phys_h: u32 = 600;
    let target = make_target_texture(renderer.device(), phys_w, phys_h)
        .expect("render-target texture");

    let (cell_w, cell_h) = renderer.cell_dims();
    let layout = Layout::build(
        phys_w as f64,
        phys_h as f64,
        0.0,
        1,
        1,
        cell_w,
        cell_h,
    );
    let view = SessionView {
        grid: terminal.grid(),
        view_offset: 0,
        cursor_visible: true,
        focused: true,
    };
    let views = std::slice::from_ref(&view);

    // Warm-up: 5 iters fill char_cache + atlas + GPU caches.
    for _ in 0..5 {
        renderer.render_layout_to_texture(&target, &layout, views, &[], 0);
    }

    let mut samples: Vec<u64> = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let t0 = std::time::Instant::now();
        renderer.render_layout_to_texture(&target, &layout, views, &[], 0);
        samples.push(t0.elapsed().as_nanos() as u64);
    }
    samples.sort_unstable();
    let p = |q: f64| -> u64 {
        let idx = ((samples.len() as f64) * q) as usize;
        samples[idx.min(samples.len() - 1)]
    };
    println!(
        r#"{{"mode":"metal-render","iterations":{},"p50_ns":{},"p95_ns":{},"p99_ns":{},"min_ns":{},"max_ns":{}}}"#,
        n,
        p(0.50),
        p(0.95),
        p(0.99),
        samples[0],
        samples[samples.len() - 1],
    );
}

/// `--bench scroll:<path>[:<start>[:<step>]]` — measure viewport repaint
/// latency under simulated downward scrolling.  This is the read-path
/// gate for disk-backed scrollback: the user's 99 %-case is "scroll
/// slowly toward live", so per-tick `cell_at_view` walks must stay
/// well under one frame budget (~16 ms; the floor we enforce is much
/// tighter).
///
/// `scroll-cold` is the same harness with one extra step between
/// feed and walk: it asks the kernel to evict the disk-backed
/// scrollback's resident pages (`MADV_DONTNEED`) so the walk
/// measures cold-page page-fault cost — the realistic experience of
/// a user who returns to scrollback hours after the writes.  No-op
/// on the Memory variant.
///
/// Lifecycle:
///   1. Construct `Terminal` (honours `MARS_DISK_SCROLLBACK` so the
///      same bench probes memory and disk paths).
///   2. Feed the scenario file to populate scrollback.
///   3. (cold only) madvise(DONTNEED) on the disk region.
///   4. Starting at `view_offset = start` (clamped to scrollback len),
///      walk every cell in the viewport via `Grid::cell_at_view` and
///      time the walk.  Decrement `view_offset` by `step` and repeat
///      until live (offset 0).
///   5. Report p50/p95/p99 nanoseconds per repaint.
fn bench_scroll(arg: &str, cold: bool) {
    let parts: Vec<&str> = arg.split(':').collect();
    if parts.is_empty() || parts[0].is_empty() {
        eprintln!("bench: scroll expects <path>[:<start>[:<step>]]");
        std::process::exit(2);
    }
    let path = parts[0];
    let start: u16 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(5000);
    let step: u16 = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(3);
    if step == 0 {
        eprintln!("bench: scroll step must be > 0");
        std::process::exit(2);
    }

    let bytes = std::fs::read(path).unwrap_or_else(|e| {
        eprintln!("bench: read {path}: {e}");
        std::process::exit(2);
    });

    let mut terminal = Terminal::new(GRID_COLS, GRID_ROWS);
    terminal.feed(&bytes);
    if cold {
        terminal.grid().evict_disk_scrollback_pages_for_bench();
    }

    // Clamp start to the actual scrollback depth — for memory storage
    // (10 K-line ring) feeding 100 K lines leaves only the most recent
    // 10 K addressable; pretending to start past that just measures
    // the "default cell" return path.
    let sb_len = terminal.grid().scrollback_len() as u16;
    let actual_start = start.min(sb_len);

    // Number of ticks: floor(actual_start / step) + 1 (final tick at 0).
    let tick_count = (actual_start as usize / step as usize) + 1;
    let mut samples: Vec<u64> = Vec::with_capacity(tick_count);
    let mut sink: u64 = 0;

    let mut offset = actual_start;
    loop {
        let t0 = std::time::Instant::now();
        for r in 0..GRID_ROWS {
            for c in 0..GRID_COLS {
                let cell = terminal.grid().cell_at_view(offset, c, r);
                sink = sink.wrapping_add(cell.ch as u64);
            }
        }
        samples.push(t0.elapsed().as_nanos() as u64);
        if offset == 0 {
            break;
        }
        offset = offset.saturating_sub(step);
    }
    // Black-hole the read sum so the optimiser can't elide the cell walk.
    std::hint::black_box(sink);

    samples.sort_unstable();
    let n = samples.len();
    let p = |q: f64| -> u64 {
        let idx = ((n as f64 - 1.0) * q).round() as usize;
        samples[idx.min(n - 1)]
    };
    let mode_label = if cold { "scroll-cold" } else { "scroll" };
    println!(
        r#"{{"mode":"{}","path":"{}","ticks":{},"start_offset":{},"step":{},"sb_len":{},"p50_ns":{},"p95_ns":{},"p99_ns":{},"min_ns":{},"max_ns":{}}}"#,
        mode_label,
        path,
        n,
        actual_start,
        step,
        sb_len,
        p(0.50),
        p(0.95),
        p(0.99),
        samples[0],
        samples[n - 1],
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

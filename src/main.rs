use objc2_app_kit::NSScreen;
use objc2_foundation::MainThreadMarker;

use marspot::app::{run_app, EventProxy, MarspotApp, MarspotAppCtx, WindowAttrs};
use marspot::input::{key_event_to_bytes, MarspotKeyEvent, Modifiers as MarspotModifiers};
use marspot::layout::Layout;
use marspot::render::{SessionView, SidebarEntry};
use marspot::render_metal::{make_target_texture, MetalRenderer};
use marspot::session::SessionState;
use marspot::terminal::Terminal;
use marspot::tmux;
use marspot::{lx_debug, lx_error, lx_warn};
use marspot::ui::{
    scroll_lines, selection_text, selection_view_for_pane, truncate_for_sidebar, LayoutMode,
    Selection, SelectionMode, CELL_TITLE_PT, MAX_SIDEBAR_LABEL_CHARS, PICKER_LAYOUTS,
    SESSION_COUNT_HARD_CAP, SIDEBAR_W_LOGICAL,
};

// Standalone marspot is essentially L2 (core) packaged with its own
// NSWindow rather than going through the shell+core split — so its
// user-visible version is the L2 / core version.
pub const VERSION: &str = env!("MARSPOT_VERSION_CORE");
pub const GIT_SHA: &str = env!("MARSPOT_GIT_SHA");
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

/// Header strip height in **logical points** — top band that
/// reserves space for the traffic-light buttons (and, later,
/// focused-session status content).  All other vertical layout
/// (sidebar items, cell rects) starts BELOW this band.  The strip
/// renders in cell-BG colour so the window reads as one continuous
/// dark surface; the buttons float over it without their own
/// separator.
const HEADER_PT: f64 = 32.0;

// Sidebar row geometry now lives on `Layout` itself
// (`Layout::sidebar_top_pad_phys` + `layout::SIDEBAR_ROW_H_PHYS`),
// so render*.rs paint and main.rs hit-test consume the same source
// of truth.  The earlier scaled-logical-point constants sat on
// values that didn't actually match the renderer (40 logical-pt at
// 2x = 80 phys vs renderer's 14 phys), causing click hit-tests to
// drift below the visually painted rows.

/// Headless modes (snapshot / bench parse / bench render) use a fixed
/// terminal grid so numbers are reproducible across runs.
const GRID_COLS: u16 = 80;
const GRID_ROWS: u16 = 24;

/// In tmux mode marspot hosts a single `Session` running `tmux -CC` and
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

struct Marspot {
    renderer: Option<MetalRenderer>,
    /// Cached layout from the last Resized.  Drives both rendering and
    /// mouse-click hit-testing.
    layout: Option<Layout>,
    /// `Some` when launched with `--tmux`; otherwise we run the
    /// standard 9-cell grid.
    tmux: Option<TmuxState>,
    /// One Pane per terminal cell on screen. Each Pane owns its
    /// Session + its own scrollback view offset, so switching focus
    /// across panes preserves each pane's scroll position (Phase C
    /// done — this used to be a single `view_offset` field).
    panes: Vec<marspot::pane::Pane>,
    /// Index into `panes` of the pane currently receiving keyboard
    /// input + mouse-wheel scrolling.
    focused_idx: usize,
    /// Self-instrumentation: when set to `Some(t0)`, the next render that
    /// commits to the layer will measure `t0.elapsed()` as the
    /// keystroke-to-pixel latency and record it.  Cleared after the next
    /// successful layer.setContents.  Off-path entirely when MARSPOT_LATENCY
    /// is unset (taken once at startup → `record_latency`).
    pending_keystroke_t0: Option<std::time::Instant>,
    /// Cumulative latency samples, written to MARSPOT_LATENCY's path on Drop.
    /// Always allocated but only pushed to when `record_latency` is true.
    latency_samples: Vec<u64>,
    record_latency: bool,
    latency_out_path: Option<String>,
    /// MARSPOT_PROFILE counters — set when MARSPOT_PROFILE_OUT is configured.
    /// Counts paths through user_event / RedrawRequested / render / feed
    /// so we can tell whether the live pipeline is render-throttled,
    /// event-throttled, or feed-throttled.
    prof: ProfileCounters,
    profile_out_path: Option<String>,
    /// User-edited cell titles, parallel to `sessions`.  `None` falls
    /// back to the default session label (the row number, or the
    /// tmux window name in tmux mode).  Persisting across runs is
    /// out of scope for the first cut.
    custom_titles: Vec<Option<String>>,
    /// When `Some(i)`, session `i`'s cell title is being edited:
    /// keyboard input goes into `title_edit_buffer` instead of the
    /// PTY, and the renderer draws a caret at the end of the title.
    /// Enter commits, Esc cancels.
    editing_title: Option<usize>,
    /// Current edit buffer for the title under edit (only meaningful
    /// while `editing_title.is_some()`).
    title_edit_buffer: String,
    /// Active text selection in the live grid of one of the
    /// sessions, if any.  Drag in the cell body extends `focus`;
    /// `dragging` says whether the mouse is still down (drag
    /// continues to grow the selection) vs released (selection is
    /// final, ready to be copied).  Cleared on typing into the
    /// PTY, focus change, or click in another cell.
    selection: Option<Selection>,
    /// True between mouse_down (in a cell body) and mouse_up — drag
    /// events update the selection only while this is set.
    selection_dragging: bool,
    /// Current main-area grid shape.  Determines how many cells the
    /// layout builder lays down; NOT tied to `sessions.len()`.
    layout_mode: LayoutMode,
    /// `true` while the layout-picker overlay is showing.  The picker
    /// floats over the main area; while open, mouse_down hits hit-test
    /// the picker first and swallow background clicks.
    layout_picker_open: bool,
    /// `true` when the user has collapsed the sidebar (Cmd-B).  The
    /// next `rebuild_layout_at` zeroes `sidebar_phys`, handing the
    /// reclaimed width to the cell grid.  `Layout::build` already
    /// supports `sidebar_w == 0` (headless / snapshot path), so the
    /// rest of the render + hit-test code follows for free.
    sidebar_collapsed: bool,
    /// Active IME preedit string for the focused pane.  Updated by
    /// `ime_preedit_changed`; cleared when the composition commits
    /// or is cancelled.  Empty string == no composition in flight.
    /// The renderer paints this inline at the cursor position with
    /// a hairline underline so the user can see what the IME will
    /// eventually send.
    ime_preedit: String,
    /// MARSPOT_PROFILE_RSS instrumentation — when set, every ~1 s the
    /// main loop appends one TSV row of per-subsystem RSS to this
    /// path.  Off-path entirely when the env var is unset.  See
    /// `Marspot::maybe_dump_rss` for the row format and the Phase 1
    /// docs for why we sample.
    profile_rss_path: Option<std::path::PathBuf>,
    /// Wall-clock origin for the elapsed-seconds column in the dump.
    /// Lazily set on the first `maybe_dump_rss` so a `MARSPOT_PROFILE_RSS`
    /// run that starts mid-soak still gets a `t=0` row.
    rss_dump_started_at: Option<std::time::Instant>,
    /// Most recent dump instant — drives the 1 Hz throttle.
    last_rss_dump: Option<std::time::Instant>,
    /// Cross-thread wake handle, cloned on demand to power
    /// per-session reader threads spawned at runtime (e.g. via the
    /// sidebar [+] button).  Same proxy main passed into `run_app`.
    #[allow(dead_code)]
    event_proxy: EventProxy,
    /// Connection to `marspot-shelld`.  Sessions are spawned and
    /// driven through this; marspot itself never forks shells, so
    /// a `marspot` process restart (silent update, manual relaunch)
    /// doesn't take any user shells with it.  `None` in tmux-CC
    /// mode, where the lone session goes through `Session::spawn_with`
    /// directly (shelld doesn't speak the tmux control protocol yet).
    shelld: Option<std::sync::Arc<marspot::shelld_client::ShelldClient>>,
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

impl MarspotApp for Marspot {
    fn resumed(&mut self, ctx: &MarspotAppCtx) {
        // mainScreen() can return a 1x screen even when our window will
        // land on a 2x one — we used to survey every screen and take
        // the max, but on macOS 26 that goes through NSArray's `count`
        // selector whose signed-vs-unsigned signature guard trips
        // objc2 0.2 with a runtime panic.  Seed off mainScreen
        // instead; the first `backingScaleFactor()` lookup on the
        // live NSWindow (in `resized`) replaces it with the screen
        // the window actually landed on, so a startup-on-1x-then-
        // dragged-to-2x scenario self-corrects on the first resize.
        let main_thread = MainThreadMarker::new()
            .expect("Marspot must be created on the main thread");
        let max_scale = NSScreen::mainScreen(main_thread)
            .map(|s| s.backingScaleFactor() as f32)
            .unwrap_or(2.0);
        let scale: f32 = std::env::var("MARSPOT_SCALE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(max_scale);

        let nsview = ctx.ns_view();
        let renderer = MetalRenderer::new(nsview, scale).expect("metal renderer init");
        self.renderer = Some(renderer);
        // run_app delivers an explicit Resized after resumed; that does
        // the renderer.resize + layout build + initial render.
    }

    fn user_event(&mut self, ctx: &MarspotAppCtx) {
        self.prof.user_events += 1;
        let drain_t0 = std::time::Instant::now();
        let total_bytes = if self.tmux.is_some() {
            self.pump_tmux_session()
        } else {
            let mut total = 0;
            for (i, p) in self.panes.iter_mut().enumerate() {
                let feed_t0 = std::time::Instant::now();
                let n = p.pump();
                self.prof.feed_total_ns += feed_t0.elapsed().as_nanos() as u64;
                total += n;
                // PTY just touched this pane's grid.  Two distinct
                // failure modes can desynchronise the selection from
                // what's actually on screen, so we handle them in
                // tandem:
                //
                //  1. `scroll_up` rolled N rows into scrollback —
                //     bump selection.abs by N so the highlight stays
                //     pinned to the original content as it migrates
                //     up out of the live grid.
                //  2. The pane's contents changed (TUIs like
                //     Claude Code repaint in place via CUP/EL — no
                //     scroll, so the bump above isn't enough), and
                //     the dragging session isn't the one being
                //     edited.  Drop the selection so we don't leave
                //     a stale highlight on top of rewritten cells.
                //     The user can re-select if they wanted it; this
                //     is the same contract typing already enforces
                //     in `key_event`.
                let pushed = p.drain_scroll_push_delta();
                let has_bytes = n > 0;
                if let Some(sel) = self.selection.as_mut() {
                    if sel.session_idx == i {
                        if pushed > 0 {
                            let bump = pushed as u32;
                            sel.anchor.1 = sel.anchor.1.saturating_add(bump);
                            sel.focus.1 = sel.focus.1.saturating_add(bump);
                        }
                        // After the abs bump: if there were any
                        // non-scroll byte writes (push_count alone
                        // can't cover in-place repaint), and the
                        // user isn't actively dragging out the
                        // selection, drop it.  `pushed` rows already
                        // moved the highlight out of the way of new
                        // appended lines; what's left to defend
                        // against is the in-place case.
                        if has_bytes && !self.selection_dragging {
                            self.selection = None;
                        }
                    }
                }
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
        // RSS profile sample (1 Hz throttle inside).  Catches the
        // active-soak's leak shape — `user_event` fires hundreds of
        // times per second under load, so the 1 Hz gate is what
        // actually rate-limits us.
        self.maybe_dump_rss();
        if self.panes.iter().all(|p| p.is_exited()) {
            for p in &mut self.panes {
                p.pump();
            }
            ctx.exit();
        }
    }

    fn key_event(&mut self, ctx: &MarspotAppCtx, event: MarspotKeyEvent, modifiers: MarspotModifiers) {
        use marspot::input::{KeyState, LogicalKey, NamedKey};

        // Cmd-C: copy current text selection to the macOS clipboard.
        // Must run before the title-edit fall-through so a selection
        // captured before opening the title editor can still be
        // copied without losing focus to the editor.  No-op when
        // there's no selection — falls through to the PTY mapper
        // which discards Cmd-* anyway.
        if event.state == KeyState::Pressed
            && modifiers.super_key()
            && matches!(event.logical, LogicalKey::Char(c) if c.eq_ignore_ascii_case(&'c'))
        {
            if self.copy_selection_to_clipboard() {
                return;
            }
        }

        // Cmd-B: toggle the sidebar.  Collapsed sidebar reclaims its
        // width for the cell grid (VSCode / Cursor convention).  Cmd-*
        // is swallowed by the app layer here — it never reaches the
        // PTY mapper — so there's no risk of conflicting with a shell
        // binding.  Sidebar-only buttons ([+] add-session, [×] close)
        // are unreachable while collapsed; the user re-opens with the
        // same Cmd-B before using them.
        if event.state == KeyState::Pressed
            && modifiers.super_key()
            && matches!(event.logical, LogicalKey::Char(c) if c.eq_ignore_ascii_case(&'b'))
        {
            self.sidebar_collapsed = !self.sidebar_collapsed;
            self.rebuild_layout(ctx);
            ctx.request_redraw();
            return;
        }

        // Title-edit mode intercepts the keyboard before the PTY
        // mapper sees anything.  Enter commits, Esc cancels,
        // Backspace pops a char, printable text appends.  Cmd-bound
        // shortcuts (paste, etc.) still fall through to the PTY
        // path so we don't break copy/paste while editing.
        if let Some(idx) = self.editing_title {
            if event.state == KeyState::Pressed
                && !modifiers.super_key()
            {
                match &event.logical {
                    LogicalKey::Named(NamedKey::Enter) => {
                        self.commit_title_edit();
                        ctx.request_redraw();
                        return;
                    }
                    LogicalKey::Named(NamedKey::Escape) => {
                        self.cancel_title_edit();
                        ctx.request_redraw();
                        return;
                    }
                    LogicalKey::Named(NamedKey::Backspace) => {
                        self.title_edit_buffer.pop();
                        ctx.request_redraw();
                        return;
                    }
                    _ => {
                        if let Some(t) = &event.text {
                            // Filter to printable chars — drop control
                            // bytes the IME might attach to functional
                            // keys (Tab, Arrows, etc.).
                            for ch in t.chars() {
                                if !ch.is_control() {
                                    self.title_edit_buffer.push(ch);
                                }
                            }
                            ctx.request_redraw();
                            return;
                        }
                    }
                }
            }
            // Non-pressed events / cmd combos in edit mode: silently
            // ignore (don't fall through to PTY for the cell that's
            // currently being edited).
            let _ = idx;
            return;
        }

        let term = self.panes[self.focused_idx].session().terminal();
        let app_mode = term.cursor_key_application_mode();
        let bracketed = term.bracketed_paste_mode();
        if let Some(bytes) = key_event_to_bytes(
            &event,
            modifiers,
            app_mode,
            bracketed,
            marspot::input::read_clipboard_text,
        ) {
            if self.record_latency && self.pending_keystroke_t0.is_none() {
                self.pending_keystroke_t0 = Some(std::time::Instant::now());
            }
            // Typing snaps the focused pane's view back to live.
            if self.panes[self.focused_idx].snap_to_live() {
                ctx.request_redraw();
            }
            // Typing into the PTY clears any text selection — once
            // the underlying grid is going to change, the existing
            // selection coordinates would point at moving content.
            if self.selection.is_some() {
                self.selection = None;
                self.selection_dragging = false;
                ctx.request_redraw();
            }
            let session = self.panes[self.focused_idx].session_mut();
            let _ = session.write(&bytes);
            // Local-echo: paint each printable-ASCII byte to the grid
            // immediately, ahead of the PTY round trip.
            // Terminal::predict_byte is a no-op for bytes that aren't
            // safe to predict (control chars, alt-screen mode, atlas
            // full, etc.).
            let mut predicted = false;
            for &b in bytes.as_ref() {
                if session.terminal_mut().predict_byte(b) {
                    predicted = true;
                }
            }
            if predicted {
                ctx.request_redraw();
            }
        }
    }

    fn mouse_down(&mut self, ctx: &MarspotAppCtx, x_phys: f64, y_phys: f64, modifiers: marspot::input::Modifiers) {
        // Layout-button + picker-overlay + close-[×] dispatch: a top-
        // level intercept that fires before any cell/sidebar handling.
        // Done in a tight borrow scope so the immutable borrow on
        // `self.layout` ends before we mutate `self`.
        let (
            layout_btn_hit,
            sidebar_btn_hit,
            picker_option_hit,
            picker_panel_hit,
            close_session_hit,
        ) = {
            let Some(layout) = &self.layout else { return };
            (
                layout.hit_test_layout_button(x_phys, y_phys),
                layout.hit_test_sidebar_button(x_phys, y_phys),
                layout.hit_test_picker_option(x_phys, y_phys),
                layout.hit_test_picker_panel(x_phys, y_phys),
                layout.hit_test_close_session(x_phys, y_phys),
            )
        };
        // Sidebar toggle: highest-priority chrome action so a click
        // on the chip never falls through to the cell underneath.
        // Mirrors the Cmd-B keyboard path.
        if sidebar_btn_hit {
            self.sidebar_collapsed = !self.sidebar_collapsed;
            self.rebuild_layout(ctx);
            ctx.request_redraw();
            return;
        }
        if self.layout_picker_open {
            if let Some(opt_idx) = picker_option_hit {
                self.layout_mode = PICKER_LAYOUTS[opt_idx];
                self.layout_picker_open = false;
                self.rebuild_layout(ctx);
                ctx.request_redraw();
                return;
            }
            if layout_btn_hit || picker_panel_hit {
                // Re-click button or click panel BG (not on an option):
                // close picker without other side effects.
                self.layout_picker_open = false;
                self.rebuild_layout(ctx);
                ctx.request_redraw();
                return;
            }
            // Click landed outside the picker entirely — close picker
            // and let the click fall through so the user doesn't have
            // to click twice (close, then act) when they meant to go
            // straight to a cell or sidebar row.
            self.layout_picker_open = false;
            self.rebuild_layout(ctx);
            // No early return — fall through to cell/sidebar dispatch.
        } else if layout_btn_hit {
            self.layout_picker_open = true;
            self.rebuild_layout(ctx);
            ctx.request_redraw();
            return;
        }

        // Sidebar close-[×] click: terminate `sessions[idx]`, but
        // refuse to close the last remaining session (marspot without a
        // session is a confusing dead-end UI; the user can spawn
        // again first via the [+] button).
        if let Some(idx) = close_session_hit {
            if self.panes.len() > 1 && idx < self.panes.len() {
                self.close_session(idx);
                self.rebuild_layout(ctx);
                ctx.request_redraw();
            }
            return;
        }

        // Sidebar [+] add-session click: spawn a new session up to
        // the SESSION_COUNT_HARD_CAP of 9.  Reuses the same Session
        // spawn path as startup.
        let add_session_hit = self
            .layout
            .as_ref()
            .map(|l| l.hit_test_add_session_button(x_phys, y_phys))
            .unwrap_or(false);
        if add_session_hit {
            if self.panes.len() < SESSION_COUNT_HARD_CAP {
                self.spawn_session();
                self.rebuild_layout(ctx);
                ctx.request_redraw();
            }
            return;
        }

        let Some(layout) = &self.layout else { return };
        // Sidebar row geometry now lives on Layout (so render*.rs
        // and hit-tests stay in lockstep).  Previous code computed
        // `SIDEBAR_TOP_PAD_PT * scale` (= 80 phys at 2x) even though
        // the renderer drew row 0 at top_inset + 14 phys — clicks
        // were misaligned with what was visually rendered.  Reading
        // from `layout.sidebar_top_pad_phys` (set by Layout::build
        // to reserve the [+] header band) fixes that.
        let row_phys = marspot::layout::SIDEBAR_ROW_H_PHYS;
        let top_pad_phys = layout.top_inset + layout.sidebar_top_pad_phys;

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

        // Resolve all hit-tests up front so the immutable borrow on
        // `layout` is dropped before we mutate self below.
        let title_hit = layout.hit_test_cell_title(x_phys, y_phys);
        let sidebar_hit = layout.hit_test_sidebar_row(
            x_phys,
            y_phys,
            top_pad_phys,
            row_phys,
            self.panes.len(),
        );
        let cell_hit = layout.hit_test(x_phys, y_phys);
        let cell_pos_hit = self
            .renderer
            .as_ref()
            .map(|r| r.cell_dims())
            .and_then(|(cw, ch)| {
                layout.hit_test_cell_pos(x_phys, y_phys, cw, ch)
            });

        // Title-strip click → enter edit mode for that cell.  Also
        // moves focus to it so the visual highlight + cursor block
        // line up with what the user is editing.
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
                ctx.request_redraw();
                return;
            }
        }

        // Click outside the title strip while editing commits the
        // edit before doing anything else.
        if self.editing_title.is_some() {
            self.commit_title_edit();
            ctx.request_redraw();
        }

        // Click in cell body → start a fresh text selection at that
        // cell coord, AND focus that cell.  Drag continues the
        // selection; release commits it (still selected; clipboard
        // copy is bound to Cmd-C).  Click in cell body that's not
        // covered by hit_test_cell_pos (e.g. clicked the padding
        // band) just clears any existing selection.
        let prior_selection = self.selection;
        self.selection = None;
        self.selection_dragging = false;
        if let Some((idx, col, row)) = cell_pos_hit {
            // Convert viewport-local row to abs (rows up from the
            // pane's current live bottom), so the selection holds its
            // grip on content even after scrolling.  Note: clicking a
            // cell does NOT snap-to-live here — when the user clicks
            // inside scrolled-back content they're explicitly
            // choosing to act on that content, and snapping would
            // shift the visible area out from under their cursor.
            let pane = &self.panes[idx];
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
            ctx.request_redraw();
            return;
        }
        if prior_selection.is_some() {
            ctx.request_redraw();
        }

        let new_focus = sidebar_hit.or(cell_hit);
        if let Some(idx) = new_focus {
            if idx < self.panes.len() && idx != self.focused_idx {
                self.focused_idx = idx;
                let _ = self.panes[self.focused_idx].snap_to_live();
                ctx.request_redraw();
            }
        }
    }

    fn mouse_drag(&mut self, ctx: &MarspotAppCtx, x_phys: f64, y_phys: f64) {
        if !self.selection_dragging {
            return;
        }
        let Some(layout) = &self.layout else { return };
        let cell_dims = self.renderer.as_ref().map(|r| r.cell_dims());
        let Some((cw, ch)) = cell_dims else { return };
        let target_idx = match self.selection.as_ref() {
            Some(s) => s.session_idx,
            None => return,
        };
        let cell = match layout.cells.get(target_idx) {
            Some(c) => c.clone(),
            None => return,
        };
        let inner_x = cell.x + layout.padding;
        let inner_y = cell.y_top + layout.cell_title_h + layout.padding;

        // Past-edge auto-scroll, NSTextView-style: dragging above the
        // cell's top reveals older scrollback (one row per drag
        // event), dragging below the cell's bottom advances toward
        // live (when there's slack to give back).  Without a separate
        // timer this only fires while the mouse is moving — holding
        // still off-edge won't keep scrolling.  Good enough for v1;
        // matches the rate-limit behaviour macOS uses for
        // pre-timer-autoscroll views.
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

        // Re-read view_offset AFTER any auto-scroll above so the abs
        // we record reflects the post-scroll viewport.
        let pane = &self.panes[target_idx];
        let rows = pane.session().terminal().grid().rows() as u32;
        let vo = pane.view_offset() as u32;
        let abs = vo + rows.saturating_sub(1).saturating_sub(row as u32);

        let Some(sel) = self.selection.as_mut() else { return };
        sel.focus = (col as u16, abs);
        ctx.request_redraw();
    }

    fn mouse_up(&mut self, _ctx: &MarspotAppCtx, _x_phys: f64, _y_phys: f64) {
        // A click without movement leaves anchor == focus → treat
        // as "no selection" so a stray single-click doesn't ghost
        // a single-cell highlight.
        if self.selection_dragging {
            self.selection_dragging = false;
            if let Some(sel) = self.selection {
                if sel.anchor == sel.focus {
                    self.selection = None;
                }
            }
        }
    }

    fn scroll(&mut self, ctx: &MarspotAppCtx, _dx_phys: f64, dy_phys: f64, precise: bool) {
        let cell_h = self
            .renderer
            .as_ref()
            .map(|r| r.cell_dims().1)
            .unwrap_or(15.0);
        // Direction / factor semantics live in `ui::scroll_lines`
        // (shared with marspot-core so both front-ends feel
        // identical).  Pane handles the clamp + view_offset mutation.
        let lines = scroll_lines(dy_phys, precise, cell_h);
        if lines == 0 {
            return;
        }
        if self.panes[self.focused_idx].apply_scroll_lines(lines) {
            ctx.request_redraw();
        }
    }

    fn resized(&mut self, ctx: &MarspotAppCtx, phys_w: f64, phys_h: f64) {
        if let Some(r) = self.renderer.as_mut() {
            r.resize(phys_w, phys_h);
        }
        self.rebuild_layout_at(ctx, phys_w, phys_h);
        // Sync render so the next CA commit lands a fresh frame at
        // the new size — avoids the live-resize flicker.
        self.render_now(ctx);
    }

    fn focused(&mut self, ctx: &MarspotAppCtx, focused: bool) {
        if let Some(r) = self.renderer.as_mut() {
            r.set_window_focused(focused);
            ctx.request_redraw();
        }
    }

    fn close_requested(&mut self, ctx: &MarspotAppCtx) {
        ctx.exit();
    }

    fn ime_preedit_changed(&mut self, ctx: &MarspotAppCtx, text: &str) {
        // Only redraw when the string actually changed so a long
        // composition doesn't churn redraws on every IME tick.
        if self.ime_preedit != text {
            self.ime_preedit.clear();
            self.ime_preedit.push_str(text);
            ctx.request_redraw();
        }
    }

    fn redraw(&mut self, ctx: &MarspotAppCtx) {
        self.prof.redraw_requested_calls += 1;
        let render_t0 = std::time::Instant::now();
        self.render_now(ctx);
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
        // RSS profile sample.  Mirrors the user_event hook — quiet
        // periods where redraw doesn't fire still get covered by the
        // user_event path; under active load both fire and the 1 Hz
        // throttle inside coalesces them into one sample per second.
        self.maybe_dump_rss();
    }
}

impl Marspot {
    /// Rebuild the cached `Layout` at the given window physical
    /// dims, honouring the current `layout_mode` + picker open
    /// state.  Called from `resized()` (with fresh dims) and from
    /// `rebuild_layout()` (read dims back from the cached layout —
    /// for layout-mode / picker-state changes that don't resize the
    /// window).  Resizes any session whose cell rect changed shape.
    fn rebuild_layout_at(&mut self, ctx: &MarspotAppCtx, phys_w: f64, phys_h: f64) {
        let Some(r) = self.renderer.as_ref() else { return };
        let (cell_w, cell_h) = r.cell_dims();
        let scale = ctx.scale();
        let sidebar_phys = if self.sidebar_collapsed {
            0.0
        } else {
            SIDEBAR_W_LOGICAL * scale
        };
        let (lc, lr) = self.layout_mode.dims();
        let header_phys = HEADER_PT * scale;
        let title_phys = CELL_TITLE_PT * scale;
        let layout = Layout::build(
            phys_w, phys_h, sidebar_phys, header_phys, title_phys,
            lc, lr, cell_w, cell_h,
        )
        .with_chrome(
            scale,
            self.layout_picker_open,
            self.panes.len(),
        );
        for (i, p) in self.panes.iter_mut().enumerate() {
            if let Some(rect) = layout.cells.get(i) {
                p.resize(rect.cols, rect.rows);
            }
        }
        self.layout = Some(layout);
    }

    /// Rebuild layout using the current cached window dims.  Used
    /// after layout-mode / picker-open changes that don't come from
    /// the AppKit Resized event (e.g. clicking the [layout] button).
    fn rebuild_layout(&mut self, ctx: &MarspotAppCtx) {
        let dims = self.layout.as_ref().map(|l| (l.window_w, l.window_h));
        if let Some((w, h)) = dims {
            self.rebuild_layout_at(ctx, w, h);
        }
    }

    /// Spawn a fresh session and append it to `self.panes` /
    /// `self.custom_titles`.  Reuses the wake-via-EventProxy path
    /// startup uses; safe to call from any `MarspotApp` callback.
    /// Refuses past `SESSION_COUNT_HARD_CAP`.  No-op in tmux mode
    /// (the single tmux -CC session is created at startup; runtime
    /// spawn would attach a second client and confuse the parser).
    fn spawn_session(&mut self) {
        if self.tmux.is_some() {
            return;
        }
        if self.panes.len() >= SESSION_COUNT_HARD_CAP {
            return;
        }
        let Some(client) = self.shelld.as_ref() else {
            lx_error!("gui.spawn.no_shelld_client", "shelld client missing, cannot spawn");
            return;
        };
        match client.new_session(INITIAL_COLS, INITIAL_ROWS, "") {
            Ok(s) => {
                self.panes.push(marspot::pane::Pane::new_shelld(s));
                self.custom_titles.push(None);
            }
            Err(e) => {
                lx_error!("gui.spawn.shelld_failed", &format!("{e}"));
            }
        }
    }

    /// Terminate `sessions[idx]` and keep all parallel state in
    /// sync.  Caller is responsible for refusing the call when this
    /// would leave marspot with zero sessions.  Drop on `Session`
    /// triggers `Pty::Drop` (SIGHUP → SIGKILL fallback → close fd
    /// → reader thread sees EOF and exits).
    fn close_session(&mut self, idx: usize) {
        if idx >= self.panes.len() {
            return;
        }
        // shelld-backed pane: ask shelld to terminate the session +
        // delete its bytelog BEFORE we drop the local pane. Otherwise
        // shelld keeps the session alive (the GUI closing the local
        // handle just detaches a subscriber) and the next time the
        // user opens a new pane, list_sessions sees this id as still
        // alive and re-attaches it — the entire bytelog replays into
        // the new pane and the "closed" content comes back.
        // Local in-process panes are handled entirely by `panes.remove`
        // below (their PTY torn down by Session/Pty Drop).
        if let (Some(id), Some(client)) = (self.panes[idx].shelld_session_id(), self.shelld.as_ref()) {
            if let Err(e) = client.kill_session(id) {
                lx_warn!(
                    "gui.close_session.kill_failed",
                    &format!("{e}"),
                    id = id,
                    pane_idx = idx
                );
            }
        }
        // Drop the session — this fires Session/Pty teardown.
        self.panes.remove(idx);
        // Parallel-array state must shrink in lockstep so the
        // post-close indices line up with what's left.
        if idx < self.custom_titles.len() {
            self.custom_titles.remove(idx);
        }
        // focused_idx: clamp into the new range.  If we just closed
        // the focused session, walk back one (or stay at 0 if it was
        // the leftmost); otherwise nudge down by one for any session
        // that lived to the right of the closed slot.
        if !self.panes.is_empty() {
            if self.focused_idx == idx {
                self.focused_idx = idx.min(self.panes.len() - 1);
            } else if self.focused_idx > idx {
                self.focused_idx -= 1;
            }
        } else {
            self.focused_idx = 0;
        }
        // Selection: clear if it lived in the closed session;
        // shift down if it lived to the right.
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
        // Title-edit: cancel if editing the closed cell, else
        // shift the index for cells to its right.
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

    /// MARSPOT_PROFILE_RSS sampler.  When the env-derived path is unset
    /// this is a single Option-check; when set, throttled to 1 Hz, it
    /// appends a TSV row so Phase 1.3's analyze-rss-dump.py can do
    /// per-subsystem slope analysis.  Columns:
    ///
    ///   `elapsed_s  total_b  grid_b  scrollback_b  atlas_b  fontcache_b  mtl_b  other_b`
    ///
    /// `other = total - sum(named)` so a leak in any unmodelled
    /// subsystem (CTFont's heap, MTL drawables, anonymous mmap pages
    /// the kernel hasn't evicted, …) surfaces there.  IO failures are
    /// silently swallowed — instrumentation must never crash marspot
    /// during a 30-min soak.
    fn maybe_dump_rss(&mut self) {
        use std::io::Write;
        let path = match &self.profile_rss_path {
            Some(p) => p.clone(),
            None => return,
        };
        let now = std::time::Instant::now();
        if let Some(last) = self.last_rss_dump {
            if now.duration_since(last).as_secs() < 1 {
                return;
            }
        }
        let started = *self.rss_dump_started_at.get_or_insert(now);
        self.last_rss_dump = Some(now);
        let elapsed_s = now.duration_since(started).as_secs();

        let total_b = read_self_rss_kib() * 1024;
        let mut grid_b: usize = 0;
        let mut scrollback_b: usize = 0;
        for p in &self.panes {
            let g = p.session().terminal().grid();
            grid_b += g.approx_bytes();
            scrollback_b += g.scrollback_approx_bytes();
        }
        let (atlas_b, fontcache_b, mtl_b) = match &self.renderer {
            Some(r) => (
                r.atlas_approx_bytes(),
                r.fontcache_approx_bytes(),
                r.metal_buffers_approx_bytes(),
            ),
            None => (0, 0, 0),
        };
        let known = grid_b + scrollback_b + atlas_b + fontcache_b + mtl_b;
        let other_b = total_b.saturating_sub(known);

        let line = format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            elapsed_s, total_b, grid_b, scrollback_b, atlas_b, fontcache_b, mtl_b, other_b,
        );
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(line.as_bytes()));
    }

    /// Commit the current title edit (if any).  Empty buffer clears
    /// any custom title for that cell, which falls back to the
    /// default session label.  Idempotent if not editing.
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

    /// Cancel a title edit without committing.  Idempotent.
    fn cancel_title_edit(&mut self) {
        self.editing_title = None;
        self.title_edit_buffer.clear();
    }

    /// Serialise the current text selection (if any) and write it
    /// to the macOS general pasteboard.  Returns true on success
    /// (something was actually copied), false otherwise — the
    /// caller can fall through to the no-selection / no-clipboard
    /// path.  Trailing whitespace on each row is dropped so a
    /// selection that overshoots the line's text doesn't carry a
    /// run of spaces; multi-row selections join with `\n`.
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

    /// In tmux mode: drain the single session's raw bytes, run them
    /// through `tmux::Parser`, and route extracted pane Output back
    /// to the terminal.  Window-state events update `tmux.windows`
    /// so the sidebar re-renders with the new list on the next
    /// redraw.  Returns the total bytes fed to the terminal.
    fn pump_tmux_session(&mut self) -> usize {
        let raw = self.panes[0].session_mut().drain_raw();
        if raw.is_empty() {
            return 0;
        }
        let events = self.tmux.as_mut().unwrap().parser.feed(&raw);
        let mut bytes_fed = 0;
        let mut got_signal_from_tmux = false;
        for ev in events {
            got_signal_from_tmux = true;
            if std::env::var("MARSPOT_TMUX_DEBUG").is_ok() {
                lx_debug!("gui.tmux.event", "tmux parser event", ev = format!("{:?}", ev));
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
                    self.panes[0].session_mut().feed_terminal(&bytes);
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
                    // Force a reconcile so closed windows actually
                    // drop out of `tmux.windows` instead of
                    // accumulating until something else (focus
                    // change, new-window) triggers a list-windows.
                    self.queue_list_windows();
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
                    // path close marspot naturally.
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
                let _ = self.panes[0].session_mut().write(b"refresh-client\n");
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
        let _ = self.panes[0].session_mut().write(b"list-windows -F \"#{window_id} #{window_name}\"\n");
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
        let _ = self.panes[0].session_mut().write(cmd.as_bytes());
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
    fn render_now(&mut self, ctx: &MarspotAppCtx) {
        if self.layout.is_none() || self.renderer.is_none() {
            return;
        }
        let focused = self.focused_idx;
        // view_offset is now per-Pane (read at SessionView construction
        // below) — there's no longer a single window-wide offset.

        // Sidebar source-of-truth depends on mode: in tmux mode, list
        // tmux windows; otherwise list sessions by ordinal number.
        const PER_WINDOW_ACTIVE_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);
        if let Some(t) = &self.tmux {
            if std::env::var("MARSPOT_TMUX_DEBUG").is_ok() {
                lx_debug!(
                    "gui.render.tmux_windows",
                    "tmux windows count",
                    len = t.windows.len()
                );
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
                (1..=self.panes.len()).map(|n| n.to_string()).collect(),
                self.panes.iter().map(|p| p.session().state()).collect(),
                focused,
            )
        };

        // Resolved label per cell (used for the cell title strip
        // AND the sidebar row, so both stay in sync).  Edit-mode
        // buffer → user-set custom title → default label.
        // tmux mode short-circuits to the tmux window name (already
        // in `labels`) and skips the custom-title machinery —
        // window names are managed by tmux itself.
        let in_tmux = self.tmux.is_some();
        let resolved_labels: Vec<String> = (0..self.panes.len()
            .max(labels.len()))
            .map(|i| {
                if !in_tmux && self.editing_title == Some(i) {
                    self.title_edit_buffer.clone()
                } else if !in_tmux {
                    if let Some(Some(custom)) = self.custom_titles.get(i)
                    {
                        custom.clone()
                    } else {
                        labels.get(i).cloned().unwrap_or_default()
                    }
                } else {
                    labels.get(i).cloned().unwrap_or_default()
                }
            })
            .collect();

        // Cell-title strings include a caret marker (▏) when the
        // cell is being edited; sidebar copies the same text but
        // without the caret + truncated with an ellipsis when it
        // overflows the sidebar's narrow column.
        let titles: Vec<String> = (0..self.panes.len())
            .map(|i| {
                let mut s = resolved_labels
                    .get(i)
                    .cloned()
                    .unwrap_or_default();
                if !in_tmux && self.editing_title == Some(i) {
                    s.push('▏');
                }
                s
            })
            .collect();
        let sidebar_labels: Vec<String> = resolved_labels
            .iter()
            .map(|s| truncate_for_sidebar(s, MAX_SIDEBAR_LABEL_CHARS))
            .collect();

        // Cap views to the active layout's cell count so we don't
        // build SessionViews for sessions that won't fit on screen
        // (e.g. 9 sessions in a Quad layout — only sessions[0..4]
        // get rendered, the rest stay alive in the sidebar).  The
        // renderer paints any extra cells (layout.cells[N..]) as
        // empty placeholders.
        let cell_count = self.layout.as_ref().map(|l| l.cells.len()).unwrap_or(0);
        let views: Vec<SessionView> = self
            .panes
            .iter()
            .take(cell_count)
            .enumerate()
            .map(|(i, p)| {
                let mut v = p.view(i == focused, titles.get(i).map(|s| s.as_str()).unwrap_or(""));
                // Preedit only applies to the focused, live pane —
                // scrolled-back views don't have a live cursor to
                // anchor it to.  Empty string skips the overlay.
                if i == focused && p.view_offset() == 0 {
                    v.ime_preedit = self.ime_preedit.as_str();
                }
                // Selection projection (abs → viewport, clip to the
                // visible band) is shared with marspot-core via
                // `ui::selection_view_for_pane`.
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
        let layout = self.layout.as_ref().unwrap();
        let renderer = self.renderer.as_mut().unwrap();
        let (cell_w, cell_h) = renderer.cell_dims();
        renderer.render_layout(layout, &views, &entries, sidebar_focus);

        // Publish the focused-pane caret rect (view-local physical
        // pixels, top-left origin) so the IME candidate window
        // anchors under the caret.  Shared geometry lives in
        // `Layout::caret_view_phys_rect`; mcli routes through the
        // renderer equivalent so both binaries stay in sync.
        let caret = self.panes.get(focused).and_then(|pane| {
            let term = pane.session().terminal();
            if !term.cursor_visible() {
                return None;
            }
            let (col, row) = term.grid().cursor();
            layout.caret_view_phys_rect(focused, col, row, cell_w, cell_h)
        });
        ctx.set_caret_rect_phys(caret);
    }
}

fn main() {
    marspot::logx::init("gui");
    let args: Vec<String> = std::env::args().collect();
    if let Some(path) = parse_named_arg(&args, "--snapshot") {
        run_snapshot(&path);
        return;
    }
    if let Some(spec) = parse_named_arg(&args, "--bench") {
        run_bench(&spec);
        return;
    }

    // Session::spawn handles ZDOTDIR shim install internally now
    // (moved to lib so mcli and any future binary get it too).

    // Every session's reader thread calls the same closure on chunk +
    // EOF; we forward to the AppKit run loop as user_event.
    let proxy = EventProxy::new();

    let tmux_mode = args.iter().any(|a| a == "--tmux");
    let initial_layout = if tmux_mode {
        LayoutMode::Single
    } else {
        LayoutMode::Nine
    };
    let n_sessions = initial_layout.cells();

    // Bring up the shelld connection up front for non-tmux mode.
    // marspot doesn't fork shells itself any more; shelld owns them
    // so a marspot restart never SIGHUPs a running session.  tmux-CC
    // mode keeps the local Session::spawn_with path because shelld
    // doesn't speak the tmux control protocol yet.
    let shelld_client: Option<std::sync::Arc<marspot::shelld_client::ShelldClient>> = if tmux_mode {
        None
    } else {
        let proxy_clone = proxy.clone();
        let wake = move || {
            proxy_clone.wake();
        };
        let socket = marspot::paths::shelld_socket();
        let client = marspot::shelld_client::ShelldClient::connect(&socket, wake)
            .unwrap_or_else(|e| {
                lx_error!(
                    "gui.shelld.connect_failed",
                    &format!("{e}"),
                    sock = socket.display(),
                    hint = "run `bin/install-shelld.sh` once to launchctl-load it"
                );
                std::process::exit(1);
            });
        Some(std::sync::Arc::new(client))
    };

    let panes: Vec<marspot::pane::Pane> = if tmux_mode {
        let proxy_clone = proxy.clone();
        let wake = move || {
            proxy_clone.wake();
        };
        let s = marspot::session::Session::spawn_with(
            "tmux",
            &["-CC", "new-session", "-A", "-s", "marspot"],
            INITIAL_COLS,
            INITIAL_ROWS,
            wake,
        )
        .expect("spawn tmux -CC session");
        vec![marspot::pane::Pane::new(s)]
    } else {
        let client = shelld_client.as_ref().unwrap();
        // List existing sessions and reattach if shelld already has
        // some (this is what makes "marspot restart preserves shells"
        // work — surviving sessions show their full bytelog history
        // on attach via shelld's REPLAY).  Fill any remaining slots
        // with brand-new sessions to reach the layout's cell count.
        let existing: Vec<marspot::shelld_proto::SessionInfo> = client
            .list_sessions()
            .unwrap_or_else(|e| {
                lx_warn!(
                    "gui.shelld.list_sessions_failed",
                    "starting fresh",
                    err = format!("{e}")
                );
                Vec::new()
            })
            .into_iter()
            .filter(|s| s.alive)
            .collect();
        let mut panes = Vec::with_capacity(n_sessions);
        for info in existing.iter().take(n_sessions) {
            match client.attach(info.session_id, INITIAL_COLS, INITIAL_ROWS) {
                Ok(s) => panes.push(marspot::pane::Pane::new_shelld(s)),
                Err(e) => {
                    lx_error!(
                        "gui.shelld.attach_failed",
                        &format!("{e}"),
                        session = info.session_id
                    );
                }
            }
        }
        while panes.len() < n_sessions {
            match client.new_session(INITIAL_COLS, INITIAL_ROWS, "") {
                Ok(s) => panes.push(marspot::pane::Pane::new_shelld(s)),
                Err(e) => {
                    lx_error!("gui.shelld.new_session_failed", &format!("{e}"));
                    break;
                }
            }
        }
        panes
    };

    let latency_out_path = std::env::var("MARSPOT_LATENCY").ok();
    let record_latency = latency_out_path.is_some();
    let profile_out_path = std::env::var("MARSPOT_PROFILE").ok();
    let profile_rss_path = std::env::var("MARSPOT_PROFILE_RSS")
        .ok()
        .map(std::path::PathBuf::from);

    let n_sessions = panes.len();
    let app = Marspot {
        renderer: None,
        layout: None,
        tmux: if tmux_mode { Some(TmuxState::new()) } else { None },
        panes,
        focused_idx: 0,
        pending_keystroke_t0: None,
        latency_samples: Vec::new(),
        record_latency,
        latency_out_path,
        prof: ProfileCounters::default(),
        profile_out_path,
        custom_titles: vec![None; n_sessions],
        editing_title: None,
        title_edit_buffer: String::new(),
        selection: None,
        selection_dragging: false,
        layout_mode: initial_layout,
        layout_picker_open: false,
        sidebar_collapsed: true,
        ime_preedit: String::new(),
        profile_rss_path,
        rss_dump_started_at: None,
        last_rss_dump: None,
        event_proxy: proxy.clone(),
        shelld: shelld_client,
    };

    let attrs = WindowAttrs {
        title: format!("Marspot v{} ({})", VERSION, GIT_SHA),
        width_logical: DEFAULT_WIN_W,
        height_logical: DEFAULT_WIN_H,
        frame_pt: None,
        bg: (0.022, 0.028, 0.042), // chrome panel tone (BG_PANEL)
    };
    run_app(app, proxy, attrs);
}

impl Drop for Marspot {
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
                lx_error!(
                    "gui.bench.latency_write_failed",
                    &format!("{e}"),
                    path = path
                );
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
                lx_error!(
                    "gui.bench.profile_write_failed",
                    &format!("{e}"),
                    path = path
                );
            }
        }
    }
}

// Note: ZDOTDIR shim install has moved to `Session::spawn` (lib) so
// every binary that spawns a session — marspot, mcli, future — picks
// up the same shell sanitisation without each having to remember to
// call it at main(). Kept this comment as a breadcrumb for greps.

/// Read scroll behaviour overrides from env once.  See `MarspotApp::scroll`
/// for the default mapping.  Returns `(invert, factor)`.
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
    // The AppKit Renderer (and its CGBitmapContext-based `snapshot`
    // method) was removed when mcli switched to Metal. A Metal-based
    // snapshot path needs Managed-storage MTLTexture + getBytes
    // readback + BGRA → RGBA conversion; not implemented yet.
    let _ = path;
    eprintln!(
        "--snapshot is temporarily disabled — the AppKit offscreen \
         renderer was removed; a Metal-based snapshot path will be \
         added if/when needed (raise an issue)."
    );
    std::process::exit(2);
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
///                  nanoseconds.  Picks up `MARSPOT_DISK_SCROLLBACK` so
///                  the same harness can probe both storage variants.
///
/// All modes write a single line of JSON to stdout so harness scripts
/// can grep / parse without depending on prose formatting.
/// Read this process's resident set size in KiB via Mach
/// `task_info(MACH_TASK_BASIC_INFO)`.  ~10 µs per call on Apple
/// Silicon — fine for the 1 Hz `MARSPOT_PROFILE_RSS` sampler.  Returns
/// 0 if the syscall fails (we never want instrumentation to crash a
/// soak).
// libc 0.2 deprecates its mach bindings (both `mach_task_self()` the
// function and `mach_task_self_` the static) in favour of the `mach2`
// crate.  Adding a new FFI dep just to silence the warning would
// violate CLAUDE.md's self-build principle for a static that is
// still fully functional — accept the deprecation locally instead.
#[allow(deprecated)]
fn read_self_rss_kib() -> usize {
    unsafe {
        let mut info: libc::mach_task_basic_info = std::mem::zeroed();
        let mut count = (std::mem::size_of::<libc::mach_task_basic_info>()
            / std::mem::size_of::<libc::natural_t>())
            as libc::mach_msg_type_number_t;
        let result = libc::task_info(
            libc::mach_task_self_,
            libc::MACH_TASK_BASIC_INFO,
            &mut info as *mut _ as libc::task_info_t,
            &mut count,
        );
        if result != libc::KERN_SUCCESS {
            return 0;
        }
        (info.resident_size / 1024) as usize
    }
}

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
        // `render` and `metal-render` are now aliases — the AppKit
        // CGImage renderer was removed, Metal is the only live path.
        "render" | "metal-render" => bench_metal_render(arg),
        "scroll" => bench_scroll(arg, /* cold */ false),
        "scroll-cold" => bench_scroll(arg, /* cold */ true),
        "scrollaccess" => bench_scrollaccess(arg),
        "glyphraster" => bench_glyphraster(arg),
        "rss-format-dump" => bench_rss_format_dump(arg),
        other => {
            eprintln!("unknown bench mode: {other}");
            std::process::exit(2);
        }
    }
}

/// `--bench scrollaccess:<lines>` — scrollback ACCESS latency at depth
/// (perf-attack D re-scope).  Builds a `<lines>`-deep disk-backed
/// scrollback, then measures the **cold** (post-MADV_DONTNEED) latency to
/// read one viewport-worth of cells at depths spanning orders of magnitude
/// (0, 1k, 10k, 100k, 1M, near-max).  This is the structural edge over
/// Terminal.app / iTerm2 (bounded history — they can't access deep at
/// all): marspot's anon-mmap ring + line index makes a deep read an O(1)
/// page-fault at the target line regardless of depth.  Reads via
/// `scrollback_cell(idx: usize, …)` — the O(1) ring primitive — NOT
/// `cell_at_view`, whose `view_offset: u16` caps the *viewport scroll* at
/// 65 535 lines (a UI-scroll limit; the data underneath is addressable to
/// the full `scrollback_len()`).  Requires `MARSPOT_DISK_SCROLLBACK=1` to
/// exceed the in-memory ring cap.  Reports per-depth cold ns + resident
/// RSS so the gate can assert flatness (max/min small) + bounded memory.
fn bench_scrollaccess(arg: &str) {
    let target_lines: usize = arg.parse().unwrap_or(2_000_000).max(GRID_ROWS as usize + 1);

    let mut terminal = Terminal::new(GRID_COLS, GRID_ROWS);
    // Feed target_lines short (~64 B) lines in batches — avoids a giant
    // single Vec while still exercising wrap / index logic per line.
    let batch_lines = 20_000usize;
    let mut buf: Vec<u8> = Vec::with_capacity(batch_lines * 70);
    let mut written = 0usize;
    while written < target_lines {
        let n = batch_lines.min(target_lines - written);
        buf.clear();
        for i in 0..n {
            let idx = written + i;
            buf.extend_from_slice(
                format!("line {idx:08}: lorem ipsum dolor sit amet consectetur adipi\r\n").as_bytes(),
            );
        }
        terminal.feed(&buf);
        written += n;
    }

    let sb_len = terminal.grid().scrollback_len();
    let max_idx = sb_len.saturating_sub(1);
    // Depths (lines back from the newest scrollback line) to probe.
    let mut depths: Vec<usize> = [0usize, 1_000, 10_000, 100_000, 1_000_000, max_idx]
        .iter()
        .copied()
        .filter(|&d| d <= max_idx)
        .collect();
    depths.sort_unstable();
    depths.dedup();

    let rss_kib = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<i64>().ok())
        .unwrap_or(-1);

    let cols = GRID_COLS;
    let rows = GRID_ROWS as usize;
    let mut sink: u64 = 0;
    let mut parts: Vec<String> = Vec::new();
    for &d in &depths {
        // Newest scrollback line is index sb_len-1; depth d sits d lines
        // older. Read a viewport-worth of lines going older from there.
        let top = max_idx.saturating_sub(d);
        // Cold: drop the ring's resident pages so the read faults from disk
        // — the realistic "scroll back hours later" case.
        terminal.grid().evict_disk_scrollback_pages_for_bench();
        let t0 = std::time::Instant::now();
        for r in 0..rows {
            let idx = top.saturating_sub(r);
            for c in 0..cols {
                if let Some(cell) = terminal.grid().scrollback_cell(idx, c) {
                    sink = sink.wrapping_add(cell.ch as u64);
                }
            }
        }
        let ns = t0.elapsed().as_nanos() as u64;
        parts.push(format!(r#"{{"depth":{d},"cold_ns":{ns}}}"#));
    }
    std::hint::black_box(sink);

    println!(
        r#"{{"mode":"scrollaccess","target_lines":{},"sb_len":{},"rss_kib":{},"depths":[{}]}}"#,
        target_lines,
        sb_len,
        rss_kib,
        parts.join(",")
    );
}

/// `--bench glyphraster:<N>` — headless glyph-rasterisation throughput
/// (perf-attack B3/B4 root-cause confirmation).  For each script class
/// (ascii / cjk / emoji) it rasterises up to N DISTINCT glyphs through
/// the real cache-miss path on a fresh (cold-atlas) renderer and reports
/// ns/glyph + glyphs/s.  This is the cost the core's render thread pays
/// when a CJK/emoji firehose first shows each glyph — the hypothesised
/// hot spot is the per-glyph `CGBitmapContextCreate` + buffer alloc +
/// context-property setup in `glyph_atlas::rasterise_glyph`.  No GPU
/// draw, no parse: the number isolates rasterisation alone.  Headless
/// (Metal device only needed for the atlas texture), so it runs in the
/// gate.  NOTE: this measures the *render* half; in the L3 architecture
/// glyph render is decoupled from cat throughput (the core reads the
/// latest shm snapshot and never back-pressures L3 unless pokes pile
/// up), so a slow number here shows as frame latency under churn, not as
/// lower cat MiB/s.
fn bench_glyphraster(arg: &str) {
    let n: usize = arg.parse().unwrap_or(1000).max(1);

    // Distinct chars per class. ascii printable is only 94 wide; cjk
    // walks the Unified Ideographs block; emoji chains the common emoji
    // blocks (unassigned codepoints resolve to glyph 0 and are skipped
    // by resolve_cell_glyph — a cheap lookup, negligible over N).
    let ascii: Vec<char> = (0x21u32..=0x7E).filter_map(char::from_u32).collect();
    let cjk: Vec<char> = (0x4E00u32..)
        .filter_map(char::from_u32)
        .take(n)
        .collect();
    let emoji: Vec<char> = (0x1F300u32..=0x1FAFF)
        .filter_map(char::from_u32)
        .take(n)
        .collect();

    let classes: [(&str, &[char]); 3] = [
        ("ascii", &ascii),
        ("cjk", &cjk),
        ("emoji", &emoji),
    ];

    let mut parts: Vec<String> = Vec::new();
    for (name, chars) in classes {
        // Fresh renderer per class → cold atlas, so every char is a miss.
        let mut renderer = MetalRenderer::new_headless().expect("headless metal renderer");
        let total_ns = renderer.bench_rasterize(chars);
        let count = chars.len();
        let ns_per = if count > 0 { total_ns / count as u64 } else { 0 };
        let per_sec = if total_ns > 0 {
            (count as f64) * 1e9 / (total_ns as f64)
        } else {
            0.0
        };
        parts.push(format!(
            r#""{}":{{"glyphs":{},"total_ns":{},"ns_per_glyph":{},"glyphs_per_sec":{:.0}}}"#,
            name, count, total_ns, ns_per, per_sec
        ));
    }
    println!(r#"{{"mode":"glyphraster",{}}}"#, parts.join(","));
}

/// `--bench rss-format-dump:<seconds>` — headless driver for the
/// MARSPOT_PROFILE_RSS dump format contract test (Phase 1.1).  Builds
/// a minimal Marspot (no renderer, no sessions, no GUI) and pumps
/// `maybe_dump_rss` for the requested number of seconds; the 1 Hz
/// throttle inside lays down N+1 rows so the format checker has
/// enough samples.  Renderer-bucket columns read 0 in this mode —
/// real numbers come from running marspot proper under a soak with the
/// same env var (Phase 1.2).
fn bench_rss_format_dump(arg: &str) {
    let secs: u64 = arg.parse().unwrap_or_else(|_| {
        eprintln!("--bench rss-format-dump expects an integer second count");
        std::process::exit(2);
    });
    let profile_rss_path = std::env::var("MARSPOT_PROFILE_RSS")
        .ok()
        .map(std::path::PathBuf::from);
    if profile_rss_path.is_none() {
        eprintln!(
            "--bench rss-format-dump requires MARSPOT_PROFILE_RSS env to point to an output path"
        );
        std::process::exit(2);
    }
    let mut app = Marspot {
        renderer: None,
        layout: None,
        tmux: None,
        panes: Vec::new(),
        focused_idx: 0,
        pending_keystroke_t0: None,
        latency_samples: Vec::new(),
        record_latency: false,
        latency_out_path: None,
        prof: ProfileCounters::default(),
        profile_out_path: None,
        custom_titles: Vec::new(),
        editing_title: None,
        title_edit_buffer: String::new(),
        selection: None,
        selection_dragging: false,
        layout_mode: LayoutMode::Nine,
        layout_picker_open: false,
        sidebar_collapsed: true,
        ime_preedit: String::new(),
        profile_rss_path,
        rss_dump_started_at: None,
        last_rss_dump: None,
        event_proxy: EventProxy::new(),
        shelld: None,
    };
    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_millis(secs * 1000 + 500);
    while std::time::Instant::now() < deadline {
        app.maybe_dump_rss();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn bench_parse(path: &str) {
    let bytes = std::fs::read(path).unwrap_or_else(|e| {
        eprintln!("bench: read {path}: {e}");
        std::process::exit(2);
    });
    // Use the same grid dimensions as a typical marspot window (auto-fit
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
        0.0,
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
        title: "",
        selection: None,
        ime_preedit: "",
        update_pending: false,
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
///   1. Construct `Terminal` (honours `MARSPOT_DISK_SCROLLBACK` so the
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


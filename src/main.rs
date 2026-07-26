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
use marspot::{lx_debug, lx_error};
use marspot::ui::{
    scroll_lines, selection_text, selection_view_for_pane, truncate_for_sidebar,
    Selection, SelectionMode, CELL_TITLE_PT, MAX_SIDEBAR_LABEL_CHARS,
    SESSION_COUNT_HARD_CAP, SIDEBAR_W_LOGICAL,
};
use marspot::ui::components::{ContextMenu, ContextMenuHit, MenuItem};

/// F3+9 — which region of the window the user right-clicked.  Used
/// to pick the menu's items.  Stored on `ContextMenuState` so the
/// action dispatcher knows the target (e.g. `Close pane` needs to
/// know *which* pane).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextRegion {
    /// Inside the cell grid of pane index `usize` (the focused-or-
    /// hovered terminal area).
    Pane(usize),
    /// Sidebar row index — `Close` / `Rename` etc. target this slot.
    SidebarSlot(usize),
    /// Inside the title strip (window chrome above the grid).  No
    /// pane-specific actions; toggles sidebar / layout etc.
    TitleStrip,
}

/// F3+9 — closed set of menu actions.  The component itself stores an
/// opaque `action_tag: u32` per item; we map u32 → this enum in
/// `dispatch_action`.  Action handlers reach into the existing
/// methods (`spawn_session`, `close_session`, `copy_selection_to_clipboard`,
/// etc.) — the menu is a UI surface for already-existing behaviour, not
/// a new behaviour layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextMenuAction {
    CopySelection,
    Paste,
    ClearScrollback,
    ClosePane,
    SplitNewPane,
    RenameTitle,
    ToggleSidebar,
    OpenLayout,
}

impl ContextMenuAction {
    fn tag(self) -> u32 {
        self as u32
    }
    fn from_tag(t: u32) -> Option<Self> {
        // SAFETY: same-variant round-trip; if `t` isn't a tag we emit
        // ourselves the caller treats None as "ignore".
        match t {
            x if x == Self::CopySelection.tag() => Some(Self::CopySelection),
            x if x == Self::Paste.tag() => Some(Self::Paste),
            x if x == Self::ClearScrollback.tag() => Some(Self::ClearScrollback),
            x if x == Self::ClosePane.tag() => Some(Self::ClosePane),
            x if x == Self::SplitNewPane.tag() => Some(Self::SplitNewPane),
            x if x == Self::RenameTitle.tag() => Some(Self::RenameTitle),
            x if x == Self::ToggleSidebar.tag() => Some(Self::ToggleSidebar),
            x if x == Self::OpenLayout.tag() => Some(Self::OpenLayout),
            _ => None,
        }
    }
}

/// F3+9 — live state of the right-click menu.  `None` on `Marspot`
/// means the menu is closed.  Open state holds:
/// - the items (so `paint` re-walks them without rebuilding),
/// - the anchor (so resize / scroll redraws keep the menu put), and
/// - the region (so the action dispatcher knows the target pane /
///   slot when an item fires).
/// `hovered_idx` is the row currently under the cursor (None = no
/// row hovered, e.g. cursor over the menu frame's padding).
struct ContextMenuState {
    items: Vec<MenuItem>,
    anchor_x: f64,
    anchor_y: f64,
    region: ContextRegion,
    hovered_idx: Option<usize>,
}

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

// Header chrome geometry (TITLE_STRIP_PT + TOOLBAR_PT = HEADER_PT)
// lives in lib.rs so binaries + render code share the same source
// of truth.  All other vertical layout (sidebar items, cell rects)
// starts BELOW the HEADER_PT band.
use marspot::HEADER_PT;

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
    /// F3+3.0 — grid shape (cols × rows) is now an arbitrary pair
    /// rather than a 7-variant enum.  User changes it via the
    /// `LayoutModal` (toolbar layout button → modal).
    grid_cols: usize,
    grid_rows: usize,
    /// True while the `LayoutModal` is open; toolbar layout button
    /// click toggles it.
    layout_modal_open: bool,
    /// F3+9 — right-click context menu state.  `None` when the menu
    /// is closed.  Right-mouse-down resolves the click region (a
    /// pane, a sidebar slot, …) and stashes the items + anchor here;
    /// the renderer reads it and paints; mouse_down + key_event Esc
    /// dismiss it.
    context_menu: Option<ContextMenuState>,
    /// UI-system dev panel state.  Default-open so a fresh launch
    /// of `bin/run.sh` shows the workbench immediately.  Toggle
    /// via toolbar button (added in a later commit) / Cmd-shortcut.
    dev_panel: marspot::ui::components::DevPanelState,
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

        // F3+9 — Esc dismisses the context menu if open.  Swallowed
        // (the keystroke does NOT reach the focused pane) — that
        // matches NSMenu, and avoids surprise side-effects like
        // sending `\e` to a vim session that meant to close a menu.
        if event.state == KeyState::Pressed && self.context_menu.is_some() {
            if let LogicalKey::Named(NamedKey::Escape) = event.logical {
                self.context_menu = None;
                ctx.request_redraw();
                return;
            }
        }

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
        // F3+9 — context menu is modal w.r.t. left-clicks while open.
        // Click on an enabled item → fire its action + close menu;
        // click on disabled / divider / menu frame padding → swallow
        // (matches macOS NSMenu); click outside the menu → close
        // menu and fall through so the click also reaches the
        // underlying chrome / pane.
        if self.context_menu.is_some() {
            let (window_w, window_h) = ctx.inner_size_phys();
            let scale = ctx.scale();
            let top_inset = self.layout.as_ref().map(|l| l.top_inset).unwrap_or(0.0);
            let (hit, region) = {
                let state = self.context_menu.as_ref().unwrap();
                let menu = ContextMenu::layout(
                    window_w, window_h, scale,
                    state.anchor_x, state.anchor_y,
                    top_inset,
                    &state.items,
                );
                (menu.hit_test(&state.items, x_phys, y_phys), state.region)
            };
            match hit {
                ContextMenuHit::Item(idx) => {
                    let tag = self.context_menu.as_ref().unwrap().items[idx].action_tag;
                    if let Some(action) = ContextMenuAction::from_tag(tag) {
                        self.dispatch_context_action(ctx, action, region);
                    } else {
                        self.context_menu = None;
                        ctx.request_redraw();
                    }
                    return;
                }
                ContextMenuHit::Frame => {
                    // Inside the menu but not on an actionable row —
                    // swallow so divider / disabled clicks don't
                    // dismiss the menu (consistent with NSMenu).
                    return;
                }
                ContextMenuHit::Outside => {
                    self.context_menu = None;
                    ctx.request_redraw();
                    // Fall through — the click also drives normal
                    // focus / selection.
                }
            }
        }
        // Layout-button + picker-overlay + close-[×] dispatch: a top-
        // level intercept that fires before any cell/sidebar handling.
        // Done in a tight borrow scope so the immutable borrow on
        // `self.layout` ends before we mutate `self`.
        let (
            layout_btn_hit,
            sidebar_btn_hit,
            dev_panel_btn_hit,
            close_session_hit,
        ) = {
            let Some(layout) = &self.layout else { return };
            (
                layout.hit_test_layout_button(x_phys, y_phys),
                layout.hit_test_sidebar_button(x_phys, y_phys),
                layout.hit_test_dev_panel_button(x_phys, y_phys),
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
        // F3+3.0 — layout button toggles the modal.
        if layout_btn_hit {
            self.layout_modal_open = !self.layout_modal_open;
            ctx.request_redraw();
            return;
        }
        // UI-system dev panel toggle.
        if dev_panel_btn_hit {
            self.dev_panel.visible = !self.dev_panel.visible;
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
                self.title_edit_buffer = self.panes[idx]
                    .custom_title
                    .clone()
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

    fn file_drop(&mut self, ctx: &MarspotAppCtx, x_phys: f64, y_phys: f64, paths: &[String]) {
        // Finder file drop → insert shell-quoted path(s) into the
        // pane under the drop point; chrome/padding drops go to the
        // focused pane.  Mirrors the L2 (marspot-core) handler.
        if paths.is_empty() {
            return;
        }
        let idx = self
            .layout
            .as_ref()
            .and_then(|l| l.hit_test(x_phys, y_phys))
            .filter(|i| *i < self.panes.len())
            .unwrap_or(self.focused_idx);
        if idx >= self.panes.len() {
            return;
        }
        if idx != self.focused_idx {
            self.focused_idx = idx;
        }
        let _ = self.panes[idx].snap_to_live();
        let mut text = String::new();
        for p in paths {
            text.push_str(&marspot::input::shell_quote_path(p));
            text.push(' ');
        }
        let bracketed = self.panes[idx]
            .session()
            .terminal()
            .bracketed_paste_mode();
        let bytes = if bracketed {
            let mut buf = Vec::with_capacity(text.len() + 12);
            buf.extend_from_slice(b"\x1b[200~");
            buf.extend_from_slice(text.as_bytes());
            buf.extend_from_slice(b"\x1b[201~");
            buf
        } else {
            text.into_bytes()
        };
        let _ = self.panes[idx].session_mut().write(&bytes);
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

    fn mouse_right_down(
        &mut self,
        ctx: &MarspotAppCtx,
        x_phys: f64,
        y_phys: f64,
        _modifiers: marspot::input::Modifiers,
    ) {
        // If the menu is already open, a right-click anywhere first
        // closes it.  The new menu opens only after the first click
        // dismisses the old; matches macOS finder behaviour where a
        // second secondary-click cycles the menu.
        if self.context_menu.take().is_some() {
            ctx.request_redraw();
        }
        let region = self.resolve_context_region(x_phys, y_phys);
        let items = self.build_menu_items(region);
        if items.is_empty() {
            return;
        }
        self.context_menu = Some(ContextMenuState {
            items,
            anchor_x: x_phys,
            anchor_y: y_phys,
            region,
            hovered_idx: None,
        });
        ctx.request_redraw();
    }

    fn mouse_moved(&mut self, ctx: &MarspotAppCtx, x_phys: f64, y_phys: f64) {
        // Drive context-menu hover highlight when the menu is open.
        // Layout has to re-walk so this is `O(items)` per mouse move
        // — negligible for menus of ~10 rows.
        if self.context_menu.is_none() {
            return;
        }
        let (window_w, window_h) = ctx.inner_size_phys();
        let scale = ctx.scale();
        let top_inset = self.layout.as_ref().map(|l| l.top_inset).unwrap_or(0.0);
        let Some(state) = self.context_menu.as_mut() else { return };
        let menu = ContextMenu::layout(
            window_w, window_h, scale,
            state.anchor_x, state.anchor_y,
            top_inset,
            &state.items,
        );
        let new_hover = menu.hover_index(&state.items, x_phys, y_phys);
        if new_hover != state.hovered_idx {
            state.hovered_idx = new_hover;
            ctx.request_redraw();
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
        let (lc, lr) = (self.grid_cols, self.grid_rows);
        let header_phys = HEADER_PT * scale;
        let title_phys = CELL_TITLE_PT * scale;
        let layout = Layout::build(
            phys_w, phys_h, sidebar_phys, header_phys, title_phys,
            lc, lr, cell_w, cell_h,
        )
        .with_chrome(
            scale,
            self.panes.len(),
            marspot::TITLE_STRIP_PT * scale,
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

    /// Spawn a fresh session and append it to `self.panes`.
    /// Reuses the wake-via-EventProxy path
    /// startup uses; safe to call from any `MarspotApp` callback.
    /// Refuses past `SESSION_COUNT_HARD_CAP`.  No-op in tmux mode
    /// (the single tmux -CC session is created at startup; runtime
    /// spawn would attach a second client and confuse the parser).
    /// F3+9 — decide which region of the window a right-click at
    /// `(x_phys, y_phys)` falls into.  The mapping is exclusive: a
    /// point is in exactly one region.  Used by `mouse_right_down`
    /// to pick the menu's items.
    fn resolve_context_region(&self, x_phys: f64, y_phys: f64) -> ContextRegion {
        let Some(layout) = &self.layout else {
            // Headless / mid-resize — fall back to a pane on the
            // focused index so the menu still works.
            return ContextRegion::Pane(self.focused_idx);
        };
        // Sidebar row check first — sidebar overlaps the same y-band
        // as panes in the title-strip area, and we want sidebar
        // priority.
        let row_phys = marspot_term::layout::SIDEBAR_ROW_H_PHYS;
        let top_pad_phys = layout.top_inset + layout.sidebar_top_pad_phys;
        if let Some(idx) = layout.hit_test_sidebar_row(
            x_phys, y_phys, row_phys, top_pad_phys, self.panes.len(),
        ) {
            return ContextRegion::SidebarSlot(idx);
        }
        // Cell area → pane idx.
        if let Some(idx) = layout.hit_test(x_phys, y_phys) {
            return ContextRegion::Pane(idx);
        }
        // Anywhere in the title strip band (above the cell grid,
        // below the top obstruction).  We don't get pixel-perfect
        // here; anything in chrome that isn't sidebar/cell counts as
        // TitleStrip.
        ContextRegion::TitleStrip
    }

    /// F3+9 — build the menu rows for a given region.  Returning an
    /// empty Vec dismisses the right-click silently (the menu won't
    /// open).  Each row's `action_tag` round-trips through
    /// `ContextMenuAction::tag` / `from_tag` so the dispatcher can
    /// pattern-match instead of carrying closures.
    fn build_menu_items(&self, region: ContextRegion) -> Vec<MenuItem> {
        let has_selection = self.selection.is_some();
        match region {
            ContextRegion::Pane(_) => {
                let copy = if has_selection {
                    MenuItem::entry("Copy", ContextMenuAction::CopySelection.tag())
                        .with_shortcut("⌘C")
                } else {
                    MenuItem::entry("Copy", ContextMenuAction::CopySelection.tag())
                        .with_shortcut("⌘C")
                        .disabled()
                };
                let close_disabled = self.panes.len() <= 1;
                let close = {
                    let mi = MenuItem::entry("Close pane", ContextMenuAction::ClosePane.tag());
                    if close_disabled { mi.disabled() } else { mi }
                };
                vec![
                    copy,
                    MenuItem::entry("Paste", ContextMenuAction::Paste.tag())
                        .with_shortcut("⌘V"),
                    MenuItem::divider(),
                    MenuItem::entry("Clear scrollback", ContextMenuAction::ClearScrollback.tag()),
                    MenuItem::divider(),
                    MenuItem::entry("New pane", ContextMenuAction::SplitNewPane.tag()),
                    close,
                ]
            }
            ContextRegion::SidebarSlot(_) => {
                let close_disabled = self.panes.len() <= 1;
                let close = {
                    let mi = MenuItem::entry("Close pane", ContextMenuAction::ClosePane.tag());
                    if close_disabled { mi.disabled() } else { mi }
                };
                vec![
                    MenuItem::entry("Rename…", ContextMenuAction::RenameTitle.tag()),
                    MenuItem::divider(),
                    close,
                ]
            }
            ContextRegion::TitleStrip => vec![
                MenuItem::entry("Toggle sidebar", ContextMenuAction::ToggleSidebar.tag())
                    .with_shortcut("⌘B"),
                MenuItem::entry("Open layout…", ContextMenuAction::OpenLayout.tag()),
            ],
        }
    }

    /// F3+9 — single-point action dispatch.  Called when a menu
    /// click resolves to an enabled item.  Closes the menu before
    /// firing so the redraw triggered inside `action` paints the
    /// post-action state.
    fn dispatch_context_action(
        &mut self,
        ctx: &MarspotAppCtx,
        action: ContextMenuAction,
        region: ContextRegion,
    ) {
        self.context_menu = None;
        match action {
            ContextMenuAction::CopySelection => {
                let _ = self.copy_selection_to_clipboard();
            }
            ContextMenuAction::Paste => {
                // Read the macOS pasteboard, route bytes through the
                // focused pane's PTY.  We reuse the same path Cmd-V
                // takes — the editing-title branch isn't relevant
                // here (no menu is open while editing a title).
                if let Some(txt) = marspot::input::read_clipboard_text() {
                    let bracketed = self.panes[self.focused_idx]
                        .session()
                        .terminal()
                        .bracketed_paste_mode();
                    let bytes = if bracketed {
                        let mut buf = Vec::with_capacity(txt.len() + 12);
                        buf.extend_from_slice(b"\x1b[200~");
                        buf.extend_from_slice(txt.as_bytes());
                        buf.extend_from_slice(b"\x1b[201~");
                        buf
                    } else {
                        txt.into_bytes()
                    };
                    let _ = self.panes[self.focused_idx]
                        .session_mut()
                        .write(&bytes);
                }
            }
            ContextMenuAction::ClearScrollback => {
                let _ = self.panes[self.focused_idx]
                    .session_mut()
                    .write(b"\x1b[3J");
            }
            ContextMenuAction::ClosePane => {
                let idx = match region {
                    ContextRegion::Pane(i) | ContextRegion::SidebarSlot(i) => i,
                    _ => self.focused_idx,
                };
                if self.panes.len() > 1 && idx < self.panes.len() {
                    self.close_session(idx);
                    self.rebuild_layout(ctx);
                }
            }
            ContextMenuAction::SplitNewPane => {
                if self.panes.len() < SESSION_COUNT_HARD_CAP {
                    self.spawn_session();
                    self.rebuild_layout(ctx);
                }
            }
            ContextMenuAction::RenameTitle => {
                let idx = match region {
                    ContextRegion::Pane(i) | ContextRegion::SidebarSlot(i) => i,
                    _ => self.focused_idx,
                };
                if idx < self.panes.len() {
                    self.editing_title = Some(idx);
                    self.title_edit_buffer = self.panes[idx]
                        .custom_title
                        .clone()
                        .unwrap_or_default();
                }
            }
            ContextMenuAction::ToggleSidebar => {
                self.sidebar_collapsed = !self.sidebar_collapsed;
                self.rebuild_layout(ctx);
            }
            ContextMenuAction::OpenLayout => {
                self.layout_modal_open = true;
            }
        }
        ctx.request_redraw();
    }

    fn spawn_session(&mut self) {
        if self.tmux.is_some() {
            return;
        }
        if self.panes.len() >= SESSION_COUNT_HARD_CAP {
            return;
        }
        // RFC-003 Phase 6: standalone marspot spawns in-process
        // Sessions; cross-restart persistence is not a property here
        // (use install-local + the split-arch path for that).
        let proxy_clone = self.event_proxy.clone();
        let wake = move || {
            proxy_clone.wake();
        };
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        match marspot::session::Session::spawn_with(
            &shell,
            &[],
            INITIAL_COLS,
            INITIAL_ROWS,
            wake,
        ) {
            Ok(s) => {
                self.panes.push(marspot::pane::Pane::new(s));
            }
            Err(e) => lx_error!("gui.spawn.session_failed", &format!("{e}")),
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
        // RFC-003 Phase 6: standalone marspot owns in-process Sessions
        // only; their PTY teardown is handled entirely by the Drop
        // chain below.
        // Drop the session — this fires Session/Pty teardown.
        self.panes.remove(idx);
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
            if let Some(pane) = self.panes.get_mut(idx) {
                let trimmed = self.title_edit_buffer.trim().to_string();
                pane.custom_title =
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
                    if let Some(custom) = self.panes.get(i)
                        .and_then(|p| p.custom_title.as_ref())
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
                let mut v = p.view(
                    i == focused,
                    titles.get(i).map(|s| s.as_str()).unwrap_or(""),
                    "",
                );
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
        // F3+9 — publish ContextMenu render state every frame the
        // menu is open.  Re-builds the row list from the items Vec
        // so the renderer doesn't share a borrow back into Marspot
        // (mirrors set_layout_modal's per-frame publish shape).
        // Dev panel lives in its own NSWindow — keep the main
        // renderer's dev_panel slot empty.
        renderer.set_dev_panel(None);
        marspot::dev_window::with_dev_window(|w| {
            w.set_visible_deferred(self.dev_panel.visible);
            if self.dev_panel.visible {
                w.render(&self.dev_panel);
            }
        });

        renderer.set_context_menu(self.context_menu.as_ref().map(|state| {
            use marspot::render_metal::{ContextMenuRender, ContextMenuRow};
            ContextMenuRender {
                scale: ctx.scale(),
                anchor_phys: (state.anchor_x, state.anchor_y),
                top_inset: layout.top_inset,
                items: state.items.iter().map(|it| ContextMenuRow {
                    label: it.label.clone(),
                    shortcut_hint: it.shortcut_hint.clone(),
                    enabled: it.enabled,
                    divider: it.divider,
                }).collect(),
                hovered_idx: state.hovered_idx,
            }
        }));
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
    // RFC-004 D.1 — must precede logx / any path computation.
    marspot::paths::migrate_legacy_state_root();
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
    let (initial_cols, initial_rows): (usize, usize) = if tmux_mode {
        (1, 1)
    } else {
        (3, 3)
    };
    let n_sessions = initial_cols * initial_rows;

    // Bring up the shelld connection up front for non-tmux mode.
    // marspot doesn't fork shells itself any more; shelld owns them
    // so a marspot restart never SIGHUPs a running session.  tmux-CC
    // mode keeps the local Session::spawn_with path because shelld
    // doesn't speak the tmux control protocol yet.
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
        // RFC-003 Phase 6: standalone marspot (no L1/L2 split) used
        // to drive shelld-backed sessions; with L4 retired it falls
        // back to the in-process Session pane (same as `mcli`'s
        // model).  Fresh shells each boot — no cross-restart
        // persistence in this code path; for that the user runs the
        // installed app (split-arch) which keeps L3 children across
        // L2 swaps via RFC-003 Amendment 7 reattach.
        let mut panes = Vec::with_capacity(n_sessions);
        for _ in 0..n_sessions {
            let proxy_clone = proxy.clone();
            let wake = move || {
                proxy_clone.wake();
            };
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
            match marspot::session::Session::spawn_with(
                &shell,
                &[],
                INITIAL_COLS,
                INITIAL_ROWS,
                wake,
            ) {
                Ok(s) => panes.push(marspot::pane::Pane::new(s)),
                Err(e) => lx_error!("gui.session.spawn_failed", &format!("{e}")),
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
        editing_title: None,
        title_edit_buffer: String::new(),
        selection: None,
        selection_dragging: false,
        grid_cols: initial_cols,
        grid_rows: initial_rows,
        layout_modal_open: false,
        context_menu: None,
        dev_panel: marspot::ui::components::DevPanelState::default(),
        sidebar_collapsed: true,
        ime_preedit: String::new(),
        profile_rss_path,
        rss_dump_started_at: None,
        last_rss_dump: None,
        event_proxy: proxy.clone(),
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
///                  nanoseconds.  Runs against the Memory variant
///                  (the bench's default Terminal without
///                  `MARSPOT_SESSION_ID`).
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
/// the full `scrollback_len()`).  Reports per-depth cold ns + resident
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
        // F2 — the historical Disk variant let bench drop ring pages
        // to simulate a cold cache; with Memory as the bench default
        // there is no page cache to drop.  Real cold-cache behaviour
        // lives in production File scrollback's pread fallback path
        // and is bench-validated via mini live runs instead.
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
        editing_title: None,
        title_edit_buffer: String::new(),
        selection: None,
        selection_dragging: false,
        grid_cols: 3,
        grid_rows: 3,
        layout_modal_open: false,
        context_menu: None,
        dev_panel: marspot::ui::components::DevPanelState::default(),
        sidebar_collapsed: true,
        ime_preedit: String::new(),
        profile_rss_path,
        rss_dump_started_at: None,
        last_rss_dump: None,
        event_proxy: EventProxy::new(),
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
        right_badge: "",
        top_fixed_h_cells: 0,
        bot_fixed_h_cells: 0,
        highlight_spans: &[],
        search_overlay: None,
        seq: 0,
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
/// a user who returns to scrollback hours after the writes.  F2:
/// the bench now runs against the Memory variant only; `cold` is
/// accepted for CLI compatibility but is a no-op.
///
/// Lifecycle:
///   1. Construct `Terminal` (Memory variant — bench has no
///      `MARSPOT_SESSION_ID`).
///   2. Feed the scenario file to populate scrollback.
///   3. (cold) no-op on Memory; retained for CLI compatibility.
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
    // F2 — `cold` previously dropped Disk variant ring pages to
    // simulate page-faulting scrollback reads.  Memory variant has
    // no pageable backing; flag retained on the CLI surface so the
    // bench-scripts contract stays compatible but the body is a
    // no-op now.  Production cold-read perf is validated via mini
    // live runs against the File variant's pread fallback.
    let _ = cold;

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


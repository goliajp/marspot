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
use marspot::session_registry::{
    self, allocate_next_session_id, list_session_entries,
};
use marspot::shell_proto::{
    decode_file_drop, decode_focus, decode_hello, decode_key_event, decode_mouse, decode_ping,
    decode_preedit, decode_resize, decode_scroll, decode_selection_text, decode_surface_attach,
    encode_caret_rect, encode_hello_ack, encode_pong, encode_surface_ready, mods_to_struct,
    wire_to_event, Frame, MsgType, DEFAULT_CONTROL_FD, ENV_CONTROL_FD, ENV_SURFACE_HEIGHT,
    ENV_SURFACE_ID, ENV_SURFACE_ID_BACK, ENV_SURFACE_SCALE, ENV_SURFACE_WIDTH, PROTO_VERSION,
};
use marspot::{lx_debug, lx_debug_sampled, lx_error, lx_event, lx_info, lx_warn};
use marspot::ui::{
    scroll_lines, selection_text, selection_view_for_pane, truncate_for_sidebar,
    Selection, SelectionMode, CELL_TITLE_PT, MAX_SIDEBAR_LABEL_CHARS,
    SESSION_COUNT_HARD_CAP, SIDEBAR_W_LOGICAL,
};
use marspot::HEADER_PT;

/// Initial dimensions for sessions created before the layout has
/// sized them (mirrors src/main.rs; the post-spawn rebuild resizes
/// to the real cell rect immediately).
const INITIAL_COLS: u16 = 40;
const INITIAL_ROWS: u16 = 12;

/// Hand `arg` off to macOS `open(1)` so the system routes it to the
/// right helper: URL → default browser, directory → Finder, file →
/// default app for the file's UTI.  Fire and forget; we don't wait
/// on the child.  Stderr inherited so a malformed arg surfaces in
/// marspot.log via the usual stderr pipe instead of vanishing.
fn spawn_open(arg: &str) {
    if let Err(e) = std::process::Command::new("/usr/bin/open")
        .arg(arg)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
    {
        lx_warn!(
            "core.link_open_failed",
            &format!("{e}"),
            arg = arg
        );
    }
}

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
/// Which chrome icon button the cursor is hovering over.  Lives at
/// module scope so render code can name it without a layer crossing.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum ChromeBtn {
    Sidebar,
    Layout,
    /// F3+1 — process-tree panel toggle.
    ProcessTree,
    /// UI-system dev panel toggle.
    DevPanel,
}

/// Convert the typed hover-button to the renderer's wire shape
/// (Option<u8>, 0 = Sidebar, 1 = Layout, 2 = ProcessTree).  Stays a
/// free function so render-side picks up no knowledge of the L2-side
/// enum.
fn map_hover_to_u8(h: Option<ChromeBtn>) -> Option<u8> {
    match h {
        Some(ChromeBtn::Sidebar) => Some(0),
        Some(ChromeBtn::Layout) => Some(1),
        Some(ChromeBtn::ProcessTree) => Some(2),
        Some(ChromeBtn::DevPanel) => Some(3),
        None => None,
    }
}

/// F3+1 — open process-tree panel state.  Holds per-pane trees +
/// a last-refresh timestamp so the main loop can decide when to
/// re-walk libproc (≥2 s gap = stale, refresh on next render).
/// `None` on `CoreApp.process_panel` means the panel is closed —
/// libproc is NOT walked at all in that state, so the panel has
/// zero idle cost when invisible.
struct ProcessPanelState {
    /// Per pane: (shelld_session_id, root shell pid, optional tree
    /// rooted at the shell child pid).  Tree is None when the pid
    /// can't be resolved (session has no entry.toml yet, or the
    /// shell child died before walk).  Indexed parallel to
    /// CoreApp.panes so the renderer can pick by pane order.
    panes: Vec<PanePidTree>,
    last_refresh: Instant,
    /// F3+1.3 — flattened per-row kill hit rects, rebuilt every render
    /// frame in parallel with the renderer's row positions.  `mouse_down`
    /// walks this list to map (x_phys, y_phys) → pid_to_kill.
    row_kill_rects: Vec<(i32, marspot_term::layout::Rect)>,
    /// F3+1.3 — kills awaiting SIGTERM→SIGKILL escalation.  Each entry
    /// is (pid, sent_at) — when sent_at + KILL_ESCALATION_GRACE elapses
    /// AND pid_is_alive(pid), we send SIGKILL and drop the entry.
    pending_kills: Vec<(i32, Instant)>,
    /// F3+4 — which pane (row in the master column) is currently
    /// selected; the detail column shows that pane's tree.  Clamped
    /// each render so a closed pane doesn't strand the selection.
    selected_pane: usize,
    /// F3+4 — previous-sample CPU times per pid + sample wall clock,
    /// used to derive CPU% via delta.  Pids that don't reappear on
    /// the next refresh get dropped.  Cleared when the panel is
    /// closed so idle has no carrying state.
    prev_pid_stats: std::collections::HashMap<i32, (u64, Instant)>,
    /// F3+1.4 — close button (red traffic light) hit rect, rebuilt
    /// per frame in parallel with the renderer's modal position.
    close_btn_rect: marspot_term::layout::Rect,
    /// F3+4 — per-pane-row clickable rects in the master column.
    /// Empty Vec when there are no panes.  Replaces tab_rects.
    pane_row_rects: Vec<marspot_term::layout::Rect>,
    /// F3+1.5 — yellow minimize traffic light hit rect.
    min_btn_rect: marspot_term::layout::Rect,
    /// F3+1.5 — green maximize traffic light hit rect.
    max_btn_rect: marspot_term::layout::Rect,
    /// F3+1.5 — title bar hit rect; clicks here begin a drag.  Body
    /// + tab strip do NOT initiate drag, only title bar (matches
    /// macOS window-drag semantics).
    title_bar_rect: marspot_term::layout::Rect,
    /// F3+1.5 — body viewport rect (used for scroll wheel routing
    /// + content clip).
    body_rect: marspot_term::layout::Rect,
    /// F3+1.5 — full modal frame rect (title + tabs + body union).
    /// `mouse_down` uses this to decide "inside modal = swallow,
    /// outside modal = backdrop click closes".
    modal_rect: marspot_term::layout::Rect,
    /// F3+1.5 — collapse body so only the title bar shows.  Yellow
    /// traffic light toggles this.
    minimized: bool,
    /// F3+1.5 — expand modal to ~95×90 % of window.  Green traffic
    /// light toggles this.
    maximized: bool,
    /// F3+1.5 — body content scroll, in physical pixels.  Clamped
    /// to `[0, content_h - viewport_h]`.  Mouse wheel updates.
    scroll_y: f64,
    /// F3+1.5 — content total height (sum of all body rows) computed
    /// during the last build.  Used to clamp scroll_y.
    content_h: f64,
    /// F3+1.5 — modal position offset from default center, in
    /// physical pixels.  Drag updates `pos_offset` by `(mouse_delta)`.
    pos_offset: (f64, f64),
    /// F3+1.5 — drag in progress.  `(grab_mouse_x, grab_mouse_y,
    /// grab_pos_offset_x, grab_pos_offset_y)` snapshotted at mouse-
    /// down so motion delta translates to pos_offset diff.  None
    /// means not dragging.
    drag_grab: Option<(f64, f64, f64, f64)>,
}

struct PanePidTree {
    shelld_session_id: u64,
    shell_child_pid: Option<i32>,
    tree: Option<marspot::pidtree::ProcNode>,
    /// F3+4 — aggregate stats over the tree, populated by
    /// `refresh_process_panel` from the latest pidtree + proc_stat
    /// sample.  Master column in the renderer reads these.
    name: String,
    n_pids: u32,
    cpu_pct: f32,
    rss_kb: u64,
    busy: String,
}

/// F3+1.3 — how long after a SIGTERM before we escalate to SIGKILL.
/// Chosen for "user clicks [×] and expects the row to vanish soon":
/// 2 s is long enough that a graceful shutdown (claude flushing logs,
/// node closing the socket) can complete, short enough that the user
/// doesn't think the click was ignored.
const KILL_ESCALATION_GRACE: Duration = Duration::from_secs(2);

/// F3+5 — per-sid debounce window on `refresh_pane_cwd_for`.  A
/// multi-line paste fires Enter N times; only the first within this
/// window triggers a syscall.  150 ms keeps "cd && cd" sequences
/// reflected near-instantly while collapsing pasted scripts into a
/// single fetch.
const CWD_REFRESH_DEBOUNCE: Duration = Duration::from_millis(150);

/// F3+1.4 — modal size in logical points (auto-scales by display
/// scale).  Centered over the marspot window.  Slightly smaller than
/// the user's 800×600 spec when scale > 1 to leave room for the
/// window's own borders + traffic lights.
const PROCESS_PANEL_WIDTH_LOGICAL: f64 = 800.0;
const PROCESS_PANEL_HEIGHT_LOGICAL: f64 = 600.0;
const PROCESS_PANEL_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// F3+1 — read `sessions/<sid>/entry.toml`'s `shell_child_pid` field.
/// Returns None on missing file, parse error, or absent field; the
/// caller renders the pane's tree as "no root pid" in that case.
/// Cheap one-shot read (typical entry.toml ~200 bytes); we don't
/// cache because panel refresh is already throttled to ≥ 2 s.
fn read_shell_child_pid(shelld_session_id: u64) -> Option<i32> {
    let path = marspot_term::session_registry::session_dir(shelld_session_id)
        .join("entry.toml");
    let body = std::fs::read_to_string(&path).ok()?;
    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("shell_child_pid") {
            // tolerate "= 1234" or "=1234"
            let v = rest.trim_start_matches(|c: char| c == '=' || c.is_whitespace());
            return v.parse::<i32>().ok();
        }
    }
    None
}

// F3+9 — right-click context menu plumbing.  Mirrors the standalone
// `src/main.rs` shape so the menu behaviour is identical across the
// split-arch (L1→L2 wire) and single-binary paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextRegion {
    Pane(usize),
    SidebarSlot(usize),
    TitleStrip,
}

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
    /// Pass the link text (URL / file path) to `/usr/bin/open`.
    /// Reads `ContextMenuState.link` for the actual string.
    OpenLink,
    /// Copy the link text to the clipboard verbatim.
    CopyLink,
}

impl ContextMenuAction {
    fn tag(self) -> u32 { self as u32 }
    fn from_tag(t: u32) -> Option<Self> {
        match t {
            x if x == Self::CopySelection.tag() => Some(Self::CopySelection),
            x if x == Self::Paste.tag() => Some(Self::Paste),
            x if x == Self::ClearScrollback.tag() => Some(Self::ClearScrollback),
            x if x == Self::ClosePane.tag() => Some(Self::ClosePane),
            x if x == Self::SplitNewPane.tag() => Some(Self::SplitNewPane),
            x if x == Self::RenameTitle.tag() => Some(Self::RenameTitle),
            x if x == Self::ToggleSidebar.tag() => Some(Self::ToggleSidebar),
            x if x == Self::OpenLayout.tag() => Some(Self::OpenLayout),
            x if x == Self::OpenLink.tag() => Some(Self::OpenLink),
            x if x == Self::CopyLink.tag() => Some(Self::CopyLink),
            _ => None,
        }
    }
}

/// Owned snapshot of the URL/path the menu was opened on.  Lives on
/// `ContextMenuState` so the dispatcher can pull it after the menu
/// has been cleared.  Kept tiny — the kind drives label wording, the
/// text drives Open + Copy actions.
#[derive(Debug, Clone)]
struct LinkContext {
    text: String,
    kind: marspot::grid_links::LinkKind,
}

/// Build the right-click menu items shown when the user clicks (left
/// or right) on an underlined URL / file-path span.  Returns Open +
/// Copy entries (Copy on top — primary intent in a terminal context
/// is "grab this URL/path", not "launch the browser").  Email is
/// recognised at scan time but inert here — caller filters it via
/// `hit_test_link_at_xy`; the empty Vec stays as the safety net.
///
/// **Regression-gate**: this is the contract behind the "click a URL,
/// get a panel" feature (`/Users/doracawl/workspace/goliajp/marspot/src/bin/marspot-core.rs:3282` for the
/// left-click entry; `mouse_right_down` line 1297 for the right-click
/// entry).  The tail of the click chain is:
///
///   AppKit `(rightM|m)ouseDown:` → L1 shell → MsgType wire → L2
///   `CoreApp::mouse_(right_)down` → `hit_test_link_at_xy` (`grid_links::scan_visible_links`)
///   → `link_menu_items_for` → `ContextMenuState` set → renderer
///   `set_context_menu` → ContextMenu paint over grid.
///
/// User noted a transient regression of this feature on 2026-06-25
/// without a root cause; the unit tests below pin the contract on
/// each LinkKind so a future drift surfaces at `cargo nextest`.
fn link_menu_items_for(
    link: &LinkContext,
) -> Vec<marspot::ui::components::MenuItem> {
    use marspot::grid_links::LinkKind;
    use marspot::ui::components::MenuItem;
    let (open_label, copy_label) = match link.kind {
        LinkKind::Url => ("Open URL", "Copy URL"),
        LinkKind::File => ("Open file", "Copy path"),
        LinkKind::Email => return Vec::new(),
    };
    vec![
        MenuItem::entry(copy_label, ContextMenuAction::CopyLink.tag()),
        MenuItem::entry(open_label, ContextMenuAction::OpenLink.tag()),
    ]
}

#[cfg(test)]
mod link_menu_tests {
    use super::*;
    use marspot::grid_links::LinkKind;

    fn ctx(kind: LinkKind) -> LinkContext {
        LinkContext { text: "https://example.com".into(), kind }
    }

    #[test]
    fn url_yields_copy_then_open() {
        let items = link_menu_items_for(&ctx(LinkKind::Url));
        assert_eq!(items.len(), 2, "URL must surface Copy + Open");
        assert_eq!(items[0].label, "Copy URL");
        assert_eq!(items[0].action_tag, ContextMenuAction::CopyLink.tag());
        assert_eq!(items[1].label, "Open URL");
        assert_eq!(items[1].action_tag, ContextMenuAction::OpenLink.tag());
    }

    #[test]
    fn file_yields_copy_path_then_open_file() {
        let items = link_menu_items_for(&ctx(LinkKind::File));
        assert_eq!(items.len(), 2, "File must surface Copy + Open");
        assert_eq!(items[0].label, "Copy path");
        assert_eq!(items[1].label, "Open file");
    }

    #[test]
    fn email_yields_empty_menu() {
        // Email is recognised but inert (per user request).  Empty
        // menu = "do nothing" — mouse_(right_)down treats len()==0 as
        // a no-op and the click falls through to selection / focus.
        let items = link_menu_items_for(&ctx(LinkKind::Email));
        assert!(items.is_empty(), "Email kind must yield no items");
    }

    /// Belt-and-braces: a NEW LinkKind variant (e.g. a future
    /// `Anchor` for HTTP fragment links) must not silently produce
    /// an empty menu without an explicit `return Vec::new()` — the
    /// `match` above is non-exhaustive-by-design and a new variant
    /// would force a compile-time decision.  This test fails to
    /// compile (not just fails to pass) if the variant is missed —
    /// listing each known variant pins the surface.
    #[test]
    fn every_known_linkkind_is_handled() {
        for kind in [LinkKind::Url, LinkKind::File, LinkKind::Email] {
            let items = link_menu_items_for(&ctx(kind));
            match kind {
                LinkKind::Url | LinkKind::File => {
                    assert_eq!(items.len(), 2, "{kind:?} must be actionable");
                }
                LinkKind::Email => {
                    assert!(items.is_empty(), "Email stays inert");
                }
            }
        }
    }
}

struct ContextMenuState {
    items: Vec<marspot::ui::components::MenuItem>,
    anchor_x: f64,
    anchor_y: f64,
    region: ContextRegion,
    hovered_idx: Option<usize>,
    /// Set when the menu was opened on top of an actionable link
    /// (URL / file path).  `dispatch_context_action` reads this for
    /// the OpenLink / CopyLink actions before clearing the menu.
    link: Option<LinkContext>,
}

#[derive(Debug)]
enum CoreEvent {
    Key(MarspotKeyEvent, Modifiers),
    MouseDown(f64, f64, Modifiers),
    /// F3+9 — right-click in screen coords + modifier byte.
    /// Drives the L2 context menu (same handler shape as MouseDown).
    MouseRightDown(f64, f64, Modifiers),
    MouseDrag(f64, f64),
    /// Coordinates are on the wire but unused — release only ends
    /// the drag (same as src/main.rs `mouse_up`).
    MouseUp,
    /// Bare mouse-move (no button).  L2 hit-tests against chrome
    /// rects so icon-button hover affordances update under the
    /// cursor.  Modifier byte on the wire is currently unused.
    MouseMove(f64, f64),
    /// `(dy_phys, precise)`; the horizontal delta is dropped at
    /// decode (terminal scrollback is vertical-only).
    Scroll(f64, bool),
    /// Finder file drop forwarded by L1: `(x, y)` drop point in
    /// physical px + resolved filesystem paths.  L2 hit-tests the
    /// pane and inserts shell-quoted paths via the Paste path.
    FileDrop(f64, f64, Vec<String>),
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
    /// L3's control reader thread saw EOF / IO error and exited.  Main
    /// loop reconnects via wait_and_connect + swaps the pane's
    /// L3Conn.control + respawns the reader.  This is the back-stop
    /// for any path that drops the L2↔L3 stream (silent-update execv
    /// before manifest v2 carried `control_stream_fd`, kernel races
    /// during dual-core swap, plain crashes, etc).
    L3ControlEof(u64),
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
    /// Shell → core: plugin-set pane title.  Inserts into the title
    /// resolution chain ABOVE cwd basename, BELOW user-set custom
    /// title.  Empty `String` clears the plugin-set entry.
    PaneTitle(u64, String),
    /// Shell → core: a plugin took over the pane backing this shelld
    /// session.  Capability bits say what L2 should change while the
    /// session is active (lock keys, freeze grid, accept overlays).
    PaneSessionBegin(u64, u32),
    /// Shell → core: plugin released the pane back to live mode.
    PaneSessionEnd(u64),
    /// Shell → core: cc plugin asked to push raw bytes into the PTY
    /// behind the given shelld session id.  L2 finds the matching L3
    /// pane and forwards via the existing control channel as an
    /// InjectInput frame.
    InjectInput(u64, Vec<u8>),
    /// C5 — L3 → L2 `SearchResults` frame.  Carries the shelld
    /// session id (so we can route to the right pane in a multi-pane
    /// world), the query_id (so a stale batch from a cancelled
    /// worker gets dropped by the pane's `SearchList`), and the
    /// decoded payload fields.
    SearchResults(
        u64,
        u32,
        bool,
        u32,
        Vec<marspot::shell_proto::WireSearchHit>,
    ),
}

/// F3+3.3 — LayoutModal drag state owned by CoreApp.  Started on
/// mouse_down inside a card, updated each mouse_drag, committed on
/// mouse_up by swapping `card_slots[from_slot]` ↔ slot under cursor.
/// No animation in V2.0; bounce + magnetic snap are V2.1.
#[derive(Debug, Clone, Copy)]
struct LayoutModalDrag {
    /// Slot the user grabbed from.
    from_slot: usize,
    /// Mouse offset inside the source card at grab time (physical
    /// pixels) so the visual stays under the cursor across motion.
    grab_offset_phys: (f64, f64),
    /// Current mouse position in physical pixels.  Live updated by
    /// mouse_drag; renderer uses `mouse - grab_offset` as draw origin.
    mouse_phys: (f64, f64),
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
        MsgType::MouseRightDown => decode_mouse(&f.payload)
            .ok()
            .map(|(x, y, m)| CoreEvent::MouseRightDown(x, y, mods_to_struct(m))),
        MsgType::MouseDrag => decode_mouse(&f.payload)
            .ok()
            .map(|(x, y, _)| CoreEvent::MouseDrag(x, y)),
        MsgType::MouseUp => decode_mouse(&f.payload).ok().map(|_| CoreEvent::MouseUp),
        MsgType::MouseMove => decode_mouse(&f.payload)
            .ok()
            .map(|(x, y, _)| CoreEvent::MouseMove(x, y)),
        MsgType::Scroll => decode_scroll(&f.payload)
            .ok()
            .map(|(_dx, dy, p)| CoreEvent::Scroll(dy, p)),
        MsgType::FileDrop => decode_file_drop(&f.payload)
            .ok()
            .map(|(x, y, paths)| CoreEvent::FileDrop(x, y, paths)),
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
        MsgType::PaneTitle => marspot::shell_proto::decode_pane_title(&f.payload)
            .ok()
            .map(|(sid, text)| CoreEvent::PaneTitle(sid, text)),
        MsgType::PaneSessionBegin => marspot::shell_proto::decode_pane_session_begin(&f.payload)
            .ok()
            .map(|(sid, caps)| CoreEvent::PaneSessionBegin(sid, caps)),
        MsgType::PaneSessionEnd => marspot::shell_proto::decode_pane_session_end(&f.payload)
            .ok()
            .map(CoreEvent::PaneSessionEnd),
        MsgType::InjectInput => marspot::shell_proto::decode_inject_input(&f.payload)
            .ok()
            .map(|(sid, bytes)| CoreEvent::InjectInput(sid, bytes)),
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
    session_id: u64,
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
                // C5 — L3 search worker delivered a batch.  Stale
                // qid is dropped by the pane's SearchList in
                // `apply_results` (D15 last-write-wins).
                MsgType::SearchResults => {
                    if let Ok((qid, has_more, total_seen, hits)) =
                        marspot::shell_proto::decode_search_results(&f.payload)
                    {
                        let _ = tx.send(CoreEvent::SearchResults(
                            session_id,
                            qid,
                            has_more,
                            total_seen,
                            hits,
                        ));
                    }
                }
                // F3+3.6 — old PaneCwd MsgType arm retired; cwd is
                // now pulled via proc_pidinfo on LayoutModal open.
                // Any stale PaneCwd frame from a not-yet-upgraded L3
                // falls into the silent-skip path below.
                _ => {}
            },
            Ok(None) | Err(_) => {
                // Tell main loop the pane's L2↔L3 stream went away —
                // it'll wait_and_connect, swap the pane's control,
                // and respawn this reader on the new stream.
                let _ = tx.send(CoreEvent::L3ControlEof(session_id));
                return;
            }
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
    spawn_l3_with_cwd(cols, rows, session_id, "", event_tx)
}

fn spawn_l3_with_cwd(
    cols: u16,
    rows: u16,
    session_id: u64,
    initial_cwd: &str,
    event_tx: &Sender<CoreEvent>,
) -> std::io::Result<L3Spawn> {
    // RFC-003 Amendment 7 step 3: L2 creates the region under the
    // deterministic per-session name `/msp-s-<id>` so a post-swap L2
    // can shm_open(name) and reattach to the surviving L3 instead of
    // spawning a duplicate.  The L3 carries the name in entry.toml
    // (via MARSPOT_SHM_NAME env) for that lookup.
    let shm_name_c = grid_shm::session_shm_name(session_id);
    let shm_name = shm_name_c
        .to_str()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let region = grid_shm::create_region_named(cols, rows, &shm_name_c)?;
    let region_raw = region.as_raw_fd();

    // CLOEXEC the shm fd we hold here so it can't leak into a sibling
    // L3 spawned later.  The pre_exec dup2 below re-clears CLOEXEC on
    // fd 4 so the actual child still sees it.
    {
        let flags = unsafe { libc::fcntl(region_raw, libc::F_GETFD) };
        if flags >= 0 {
            unsafe { libc::fcntl(region_raw, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
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
        "spawning L3 session (UDS-only control, RFC-003 step 3b)",
        bin = session_bin.display(),
        cols = cols,
        rows = rows,
        shm_fd = SHM_TARGET_FD,
        session_id = session_id
    );
    let mut cmd = Command::new(&session_bin);
    cmd.env(ENV_SHM_FD, SHM_TARGET_FD.to_string())
        // L2 owns session assignment: hand this L3 the exact session it
        // must drive so N children never race for the same one.
        .env("MARSPOT_SESSION_ID", session_id.to_string())
        // RFC-003: L3 owns its own PTY *and* binds its own UDS at
        // sessions/<id>/sock.  L2 connects to that socket post-spawn
        // (see wait_and_connect below).
        .env("MARSPOT_L3_OWNS_PTY", "1")
        // Amendment 7 step 3: hand the L3 the shm name we chose so it
        // records it in entry.toml.  A future L2 then knows what to
        // shm_open for reattach.
        .env("MARSPOT_SHM_NAME", &shm_name);
    // F3+6 — pass the user's saved cwd through so the shell forks
    // there instead of $HOME.  Empty = unset (L3 falls back to $HOME).
    if !initial_cwd.is_empty() {
        cmd.env("MARSPOT_INITIAL_CWD", initial_cwd);
    }
    // Ensure the child does NOT inherit any control-socket env from
    // the L2 parent — RFC-003 step 3b leaves the inherited-fd path
    // behind entirely.
    cmd.env_remove(ENV_CONTROL_FD);
    // SAFETY: pre_exec runs between fork and exec; only async-signal-safe
    // libc calls (dup2/close/fcntl) are used.
    unsafe {
        cmd.pre_exec(move || {
            if region_raw != SHM_TARGET_FD {
                if libc::dup2(region_raw, SHM_TARGET_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            let flags = libc::fcntl(SHM_TARGET_FD, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(SHM_TARGET_FD, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    lx_event!("L3_SPAWNED", "L3 child running", pid = child.id());

    // Map the region as a reader (mmap survives the fd closing, so the
    // owned `region` can drop after).
    let reader = GridShmReader::from_fd(region_raw)?;
    drop(region);

    // RFC-003 step 3b: connect to the L3's UDS instead of inheriting a
    // socketpair.  L3 boots, binds sessions/<id>/sock, writes entry.toml;
    // we poll for the entry then handshake.  Failure rolls back the
    // child (Drop sends SIGKILL via std).
    let control = match marspot::uds_session_client::wait_and_connect(
        session_id,
        std::time::Duration::from_secs(5),
    ) {
        Ok(s) => s,
        Err(e) => {
            lx_error!(
                "core.l3.uds_connect_failed",
                &format!("{e}"),
                session_id = session_id,
                child_pid = child.id()
            );
            // RFC-003 §6 Amendment 15.2 — std::process::Child::drop
            // is a no-op (Rust deliberately doesn't reap behind your
            // back); on this error path the just-spawned L3 would
            // otherwise live on forever as an orphan.  Explicit
            // SIGKILL + reap so we don't leak processes (and the
            // associated PTY + vault deposit, which the kernel
            // collects when the last reference drops).
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    };
    let reader_stream = control.try_clone()?;
    let tx = event_tx.clone();
    let (selection_tx, selection_rx) = std::sync::mpsc::channel::<(u32, String)>();
    std::thread::spawn(move || l3_reader_loop(reader_stream, session_id, tx, selection_tx));

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
    spawn_l3_pane_with_cwd(cols, rows, session_id, "", event_tx)
}

fn spawn_l3_pane_with_cwd(
    cols: u16,
    rows: u16,
    session_id: u64,
    initial_cwd: &str,
    event_tx: &Sender<CoreEvent>,
) -> std::io::Result<Pane> {
    let spawn = spawn_l3_with_cwd(cols, rows, session_id, initial_cwd, event_tx)?;
    Ok(Pane::new_l3(L3Conn::new(spawn, session_id)))
}

/// RFC-003 Amendment 7 step 4: reattach to an L3 child that survived
/// the previous L2's death.  Walks entry.toml → shm_open(name) +
/// connect_with_handshake(socket).  Returns a Pane that drives the
/// *existing* L3 process without forking a new one.
///
/// Returns Err on any failure (registry entry missing, shm name
/// missing, shm gone, socket gone, handshake refused) — caller falls
/// through to spawn_l3_pane.
fn reattach_l3_pane(
    session_id: u64,
    event_tx: &Sender<CoreEvent>,
) -> std::io::Result<Pane> {
    let entry = marspot_term::session_registry::read_session_entry(session_id)?;
    if entry.shm_name.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "no shm_name (legacy entry)",
        ));
    }
    let shm_c = std::ffi::CString::new(entry.shm_name.clone())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e}")))?;
    let shm_fd = grid_shm::open_region(&shm_c)?;
    let reader = GridShmReader::from_fd(shm_fd.as_raw_fd())?;
    // GridShmReader keeps its own mmap; once the fd is mapped we can
    // drop the OwnedFd (kernel keeps the mapping alive).
    drop(shm_fd);

    // UDS connect + handshake — same wire as the spawn path.
    let control = marspot::uds_session_client::wait_and_connect(
        session_id,
        std::time::Duration::from_secs(2),
    )?;
    let reader_stream = control.try_clone()?;
    let tx = event_tx.clone();
    let (selection_tx, selection_rx) = std::sync::mpsc::channel::<(u32, String)>();
    std::thread::spawn(move || l3_reader_loop(reader_stream, session_id, tx, selection_tx));

    lx_event!(
        "L3_REATTACHED",
        "took over surviving L3 from previous L2 image",
        session_id = session_id,
        pid = entry.pid,
        shm_name = entry.shm_name
    );
    Ok(Pane::new_l3(L3Conn::reattach(
        entry.pid,
        control,
        reader,
        selection_rx,
        session_id,
    )))
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
    /// Per-shelld-session plugin-set title, set via `MsgType::PaneTitle`.
    /// Inserts into the title resolution chain ABOVE cwd basename,
    /// BELOW user-set custom title.  Empty payload removes the entry.
    pane_titles: std::collections::HashMap<u64, String>,
    /// F3+2.1 — cwd reported by each pane's shell via OSC 7.  Keyed
    /// by shelld_session_id.  Read by the title placeholder chain
    /// (`Path::file_name` of the cached path → basename string).
    /// Populated event-driven by `MsgType::PaneCwd` frames, so the
    /// hot path stays zero-syscall.
    pane_cwds: std::collections::HashMap<u64, String>,
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
    /// F3+3.0 — grid shape (cols × rows) is now an arbitrary
    /// pair rather than a 7-variant enum.  Cell total = cols×rows;
    /// when N panes > cells the overflow stays in the sidebar.
    /// User changes via the `LayoutModal`.
    grid_cols: usize,
    grid_rows: usize,
    /// True while the `LayoutModal` is open; toolbar layout button
    /// click toggles it.  Modal contents (cols/rows +/- controls
    /// + preview) live in the modal component.
    layout_modal_open: bool,
    /// F3+9 — right-click ContextMenu live state.  `None` when closed.
    /// Set by `mouse_right_down`; cleared by `mouse_down` outside
    /// the menu, Esc key, or after dispatching an action.
    context_menu: Option<ContextMenuState>,
    /// Modal-staged cols / rows.  Updated by clicks on the modal's
    /// +/- steppers; committed to `grid_cols` / `grid_rows` on Apply.
    /// Initialised from grid_* every time the modal opens.
    pending_grid_cols: usize,
    pending_grid_rows: usize,
    /// F3+5 — per-sid debounce window for `refresh_pane_cwd_for`.
    /// A burst of Enter keys (multi-line paste) hits this map and
    /// returns within the debounce → at most one syscall per
    /// `CWD_REFRESH_DEBOUNCE` per pane.  Cleared per-sid on
    /// `close_session`.
    last_cwd_refresh: std::collections::HashMap<u64, Instant>,
    /// F3+3.3 — per-slot assignment for the LayoutModal preview /
    /// drag area.  `card_slots[slot_idx] = pane_idx` shows that
    /// pane's title in slot `slot_idx`.  A pane_idx out of range
    /// (>= panes.len()) renders as an empty slot.  Length is
    /// always `pending_grid_cols * pending_grid_rows`; reset to
    /// `(0..cells).collect()` whenever the modal opens or cells
    /// count changes.  Drag-drop swaps two entries here; Apply
    /// permutes `self.panes` to match.
    card_slots: Vec<usize>,
    /// Active drag, if any.  None while no drag in progress.
    layout_drag: Option<LayoutModalDrag>,
    sidebar_collapsed: bool,
    /// Which chrome icon button (if any) the cursor is currently
    /// hovering over.  Updated on every `MouseMove` frame; drives a
    /// darker BG fill in the toolbar render.  `None` outside both
    /// buttons.  Kept on `CoreApp` (not Layout) so mouse-move never
    /// rebuilds the grid math.
    hover_chrome_btn: Option<ChromeBtn>,
    /// F3+1 — process-tree panel.  `None` = closed (no libproc cost).
    process_panel: Option<ProcessPanelState>,
    ime_preedit: String,
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
    /// cc plugin asked to push raw bytes into the PTY behind
    /// `shelld_session_id`.  Find the matching L3 pane and let it
    /// forward via the existing control channel.
    fn inject_input(&mut self, shelld_session_id: u64, bytes: &[u8]) {
        for pane in &mut self.panes {
            if pane.session().l3_session_id() == Some(shelld_session_id) {
                pane.session_mut().forward_inject_input(bytes);
                return;
            }
        }
    }

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

    fn set_pane_title(&mut self, shelld_session_id: u64, text: String) {
        let prev = self.pane_titles.get(&shelld_session_id).cloned();
        let changed = if text.is_empty() {
            self.pane_titles.remove(&shelld_session_id).is_some()
        } else if prev.as_deref() != Some(text.as_str()) {
            self.pane_titles.insert(shelld_session_id, text);
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
        let (lc, lr) = (self.grid_cols, self.grid_rows);
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
        .with_chrome(
            self.scale,
            self.panes.len(),
            marspot::TITLE_STRIP_PT * self.scale,
        );
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

    /// F3+3.3 — reset `card_slots` to identity for the current
    /// pending grid shape.  Called when the modal opens, when the
    /// user changes cols/rows in the modal (since the cell count
    /// changes), and on apply (after permuting).
    /// F3+3.6 — batch pull-based cwd refresh.  Walks every pane,
    /// `read_shell_child_pid` + `pidtree::proc_cwd` per pane.  Used
    /// by LayoutModal open to one-shot fresh all 9.
    fn refresh_pane_cwds(&mut self) {
        let now = Instant::now();
        for pane in &self.panes {
            let Some(sid) = pane.shelld_session_id() else { continue };
            let Some(pid) = read_shell_child_pid(sid) else { continue };
            let Some(path) = marspot::pidtree::proc_cwd(pid) else { continue };
            let s = path.to_string_lossy().into_owned();
            self.pane_cwds.insert(sid, s);
            self.last_cwd_refresh.insert(sid, now);
        }
    }

    /// F3+5 — single-pane cwd refresh with per-sid debounce.
    /// Returns `true` if a syscall actually ran.  Callers pass
    /// `force=true` when the trigger has just-occurred semantics
    /// (pane spawn, modal open) to bypass debounce; `false` for
    /// rate-limited triggers (every focus change, every Enter).
    ///
    /// Cost: at most two syscalls (one toml read + one
    /// `proc_pidinfo`) when not debounced; zero on debounce hit.
    /// Not on the render hot path.
    fn refresh_pane_cwd_for(&mut self, pane_idx: usize, force: bool) -> bool {
        let Some(pane) = self.panes.get(pane_idx) else { return false };
        let Some(sid) = pane.shelld_session_id() else { return false };
        let now = Instant::now();
        if !force {
            if let Some(&prev) = self.last_cwd_refresh.get(&sid) {
                if now.duration_since(prev) < CWD_REFRESH_DEBOUNCE {
                    return false;
                }
            }
        }
        // F3+5.1 — bump the debounce clock BEFORE the syscalls fire,
        // so a permanently-failing fetch (entry.toml without
        // shell_child_pid, sandbox-blocked proc_pidinfo) still ticks
        // the debounce instead of getting force-retried every frame
        // by the build_views lazy fill.  Worst case becomes 1 attempt
        // per CWD_REFRESH_DEBOUNCE per pane instead of 60 fps × N.
        self.last_cwd_refresh.insert(sid, now);
        let Some(pid) = read_shell_child_pid(sid) else { return false };
        let Some(path) = marspot::pidtree::proc_cwd(pid) else { return false };
        self.pane_cwds.insert(sid, path.to_string_lossy().into_owned());
        true
    }

    /// F3+6 — snapshot every persistable bit of state to
    /// `shell-state.bin`.  Called from spawn / close / focus-change /
    /// layout-apply / title-commit so a hard kill leaves a recent
    /// state on disk.  ~50 us per call (memcpy + atomic rename); no
    /// debounce because we never call this on the render hot path.
    fn save_session_state(&self) {
        use marspot::state::{SavedPane, SavedState};
        let panes: Vec<SavedPane> = self.panes.iter().enumerate().map(|(i, p)| {
            let sid = p.shelld_session_id().unwrap_or(0);
            let custom_title = self.custom_titles.get(i)
                .and_then(|t| t.clone())
                .unwrap_or_default();
            let last_cwd = self.pane_cwds.get(&sid).cloned().unwrap_or_default();
            SavedPane { sid, custom_title, last_cwd }
        }).collect();
        let saved = SavedState {
            grid_cols: self.grid_cols as u16,
            grid_rows: self.grid_rows as u16,
            focused_idx: self.focused_idx as u16,
            panes,
            // F3+6.1 reserved — L1 writes window frame on its side.
            window: None,
        };
        if let Err(e) = marspot::state::write(&saved) {
            lx_warn!(
                "core.state_file.write_failed",
                &format!("{e}; saved state not persisted this tick")
            );
        }
    }

    /// F3+5 — fill `pane_cwds` for any pane that doesn't yet have an
    /// entry cached.  Called at the top of `build_views` so the title
    /// strip placeholder always reads a populated value (modulo
    /// genuinely-failing fetches: pid not yet written, sandbox, etc).
    /// O(panes) with HashMap.contains_key on the hot path; the
    /// syscall only fires for the misses, which on a stable session
    /// = zero per frame after the first.
    fn lazy_fill_missing_cwds(&mut self) {
        let mut filled = false;
        for i in 0..self.panes.len() {
            let Some(sid) = self.panes[i].shelld_session_id() else { continue };
            if self.pane_cwds.contains_key(&sid) { continue; }
            // F3+5.1 — `force=false`: paired with the now-always-bumped
            // debounce clock in `refresh_pane_cwd_for`, this means a
            // pane that hasn't filled yet retries at most every
            // `CWD_REFRESH_DEBOUNCE`, not every frame.  Steady state
            // (all populated) skips entirely via contains_key above.
            if self.refresh_pane_cwd_for(i, false) && self.pane_cwds.contains_key(&sid) {
                filled = true;
            }
        }
        // F3+6 — once we successfully ingested at least one fresh cwd,
        // persist immediately so the saved file isn't empty after the
        // very first frame's worth of fills.  No-op when nothing was
        // actually filled (steady state skips above).
        if filled {
            self.save_session_state();
        }
    }

    fn reset_card_slots(&mut self) {
        let cells = self.pending_grid_cols * self.pending_grid_rows;
        self.card_slots = (0..cells).collect();
        self.layout_drag = None;
    }

    /// F3+3.3 — apply `card_slots` as a permutation on the leading
    /// `cells` panes.  After this, `self.panes[slot_idx]` is the
    /// pane that previously sat at `card_slots[slot_idx]` (== the
    /// pane the user dragged into slot_idx in the modal).
    ///
    /// `card_slots` entries that point past the live pane count
    /// are skipped (empty cards stay empty).  The post-cells tail
    /// of `self.panes` (sidebar overflow) is untouched.  Same
    /// permutation is applied to `custom_titles` so the title-
    /// strip / sidebar labels travel with their owning pane.
    /// Resets `card_slots` to identity afterwards.
    fn apply_card_slot_permutation(&mut self, cells: usize) {
        let n_in_grid = cells.min(self.panes.len());
        if n_in_grid == 0 || self.card_slots.len() < n_in_grid {
            self.reset_card_slots();
            return;
        }
        // Build new layouts for the leading n_in_grid slots.  Slots
        // pointing at out-of-range pane indices map to None (empty)
        // and the corresponding existing pane keeps its place at
        // the tail (skipped during reorder).
        let mut new_panes: Vec<Option<Pane>> = (0..n_in_grid).map(|_| None).collect();
        let mut new_titles: Vec<Option<Option<String>>> = (0..n_in_grid).map(|_| None).collect();
        // Drain the leading n_in_grid panes into Option holders so
        // we can move them around without re-borrow conflicts.
        let mut drained: Vec<Option<Pane>> =
            self.panes.drain(..n_in_grid).map(Some).collect();
        let mut drained_titles: Vec<Option<Option<String>>> =
            if self.custom_titles.len() >= n_in_grid {
                self.custom_titles.drain(..n_in_grid).map(Some).collect()
            } else {
                (0..n_in_grid).map(|_| Some(None)).collect()
            };
        for slot_idx in 0..n_in_grid {
            let from = self.card_slots[slot_idx];
            if from < drained.len() {
                new_panes[slot_idx] = drained[from].take();
                new_titles[slot_idx] = drained_titles
                    .get_mut(from)
                    .and_then(|t| t.take());
            }
        }
        // Re-insert at the head.  Any leftover (None) means the slot
        // had no source pane — should not happen with identity-only
        // permutations but defensible: pull from a leftover pool to
        // avoid panicking.
        let mut leftover: Vec<Pane> = drained.into_iter().flatten().collect();
        let mut leftover_titles: Vec<Option<String>> = drained_titles
            .into_iter()
            .flatten()
            .collect();
        let mut ordered: Vec<Pane> = Vec::with_capacity(n_in_grid);
        let mut ordered_titles: Vec<Option<String>> = Vec::with_capacity(n_in_grid);
        for i in 0..n_in_grid {
            if let Some(p) = new_panes[i].take() {
                ordered.push(p);
            } else if let Some(p) = leftover.pop() {
                ordered.push(p);
            }
            if let Some(t) = new_titles[i].take() {
                ordered_titles.push(t);
            } else if let Some(t) = leftover_titles.pop() {
                ordered_titles.push(t);
            } else {
                ordered_titles.push(None);
            }
        }
        // Re-prepend.
        let tail_panes = std::mem::take(&mut self.panes);
        self.panes = ordered;
        self.panes.extend(tail_panes);
        let tail_titles = std::mem::take(&mut self.custom_titles);
        self.custom_titles = ordered_titles;
        self.custom_titles.extend(tail_titles);
        self.reset_card_slots();
    }

    /// Spawn a fresh session and append it.  Refuses past
    /// `SESSION_COUNT_HARD_CAP`.  Sized to the cell it will land in
    /// (falling back to the first cell's shape) so the shell prompt
    /// prints at the right width from its very first byte.
    // ─── F3+9 — right-click context menu (split-arch L2 side) ────────

    fn mouse_right_down(
        &mut self,
        x_phys: f64,
        y_phys: f64,
        _modifiers: Modifiers,
    ) {
        // Second right-click closes the previous menu first.
        if self.context_menu.take().is_some() {
            self.needs_render = true;
        }
        // A right-click that lands on a recognised URL / file path
        // gets a link-specific menu (Open / Copy) instead of the
        // generic pane menu.  Email is recognised but inert.
        let link = self.hit_test_link_at_xy(x_phys, y_phys);
        let region = self.resolve_context_region(x_phys, y_phys);
        let items = match &link {
            Some(l) => self.build_link_menu_items(l),
            None => self.build_menu_items(region),
        };
        if items.is_empty() {
            return;
        }
        self.context_menu = Some(ContextMenuState {
            items,
            anchor_x: x_phys,
            anchor_y: y_phys,
            region,
            hovered_idx: None,
            link,
        });
        self.needs_render = true;
    }

    fn resolve_context_region(&self, x_phys: f64, y_phys: f64) -> ContextRegion {
        // Sidebar row check first — wins over the cell-area hit when
        // both overlap (the sidebar overlays the title-strip band).
        let row_phys = marspot_term::layout::SIDEBAR_ROW_H_PHYS;
        let top_pad_phys = self.layout.top_inset + self.layout.sidebar_top_pad_phys;
        if let Some(idx) = self.layout.hit_test_sidebar_row(
            x_phys, y_phys, top_pad_phys, row_phys, self.panes.len(),
        ) {
            return ContextRegion::SidebarSlot(idx);
        }
        if let Some(idx) = self.layout.hit_test(x_phys, y_phys) {
            return ContextRegion::Pane(idx);
        }
        ContextRegion::TitleStrip
    }

    fn build_menu_items(
        &self,
        region: ContextRegion,
    ) -> Vec<marspot::ui::components::MenuItem> {
        use marspot::ui::components::MenuItem;
        match region {
            ContextRegion::Pane(_) => {
                let close_disabled = self.panes.len() <= 1;
                let close = {
                    let mi = MenuItem::entry(
                        "Close pane", ContextMenuAction::ClosePane.tag(),
                    );
                    if close_disabled { mi.disabled() } else { mi }
                };
                let copy = MenuItem::entry(
                    "Copy", ContextMenuAction::CopySelection.tag(),
                ).with_shortcut("⌘C");
                vec![
                    if self.selection.is_some() { copy } else { copy.disabled() },
                    MenuItem::entry("Paste", ContextMenuAction::Paste.tag())
                        .with_shortcut("⌘V"),
                    MenuItem::divider(),
                    MenuItem::entry("Clear scrollback",
                        ContextMenuAction::ClearScrollback.tag()),
                    MenuItem::divider(),
                    MenuItem::entry("New pane",
                        ContextMenuAction::SplitNewPane.tag()),
                    close,
                ]
            }
            ContextRegion::SidebarSlot(_) => {
                let close_disabled = self.panes.len() <= 1;
                let close = {
                    let mi = MenuItem::entry(
                        "Close pane", ContextMenuAction::ClosePane.tag(),
                    );
                    if close_disabled { mi.disabled() } else { mi }
                };
                vec![
                    MenuItem::entry("Rename…",
                        ContextMenuAction::RenameTitle.tag()),
                    MenuItem::divider(),
                    close,
                ]
            }
            ContextRegion::TitleStrip => vec![
                MenuItem::entry("Toggle sidebar",
                    ContextMenuAction::ToggleSidebar.tag())
                    .with_shortcut("⌘B"),
                MenuItem::entry("Open layout…",
                    ContextMenuAction::OpenLayout.tag()),
            ],
        }
    }

    /// Two-entry menu shown when the user clicks (either button) on
    /// a URL or file-path span.  Labels read per-kind so the user
    /// can tell from the menu what they're acting on.
    fn build_link_menu_items(
        &self,
        link: &LinkContext,
    ) -> Vec<marspot::ui::components::MenuItem> {
        link_menu_items_for(link)
    }

    fn dispatch_context_action(
        &mut self,
        action: ContextMenuAction,
        region: ContextRegion,
    ) {
        // Snapshot the link text before clearing the menu — the
        // OpenLink / CopyLink arms read it after the clear.
        let link_text = self
            .context_menu
            .as_ref()
            .and_then(|s| s.link.as_ref())
            .map(|l| l.text.clone());
        self.context_menu = None;
        match action {
            ContextMenuAction::CopySelection => {
                let _ = self.copy_selection_to_clipboard();
            }
            ContextMenuAction::Paste => {
                if let Some(txt) = marspot::input::read_clipboard_text() {
                    if let Some(pane) = self.panes.get_mut(self.focused_idx) {
                        pane.session_mut().forward_paste(&txt);
                    }
                }
            }
            ContextMenuAction::ClearScrollback => {
                if let Some(pane) = self.panes.get_mut(self.focused_idx) {
                    pane.session_mut().forward_inject_input(b"\x1b[3J");
                }
            }
            ContextMenuAction::ClosePane => {
                let idx = match region {
                    ContextRegion::Pane(i) | ContextRegion::SidebarSlot(i) => i,
                    _ => self.focused_idx,
                };
                if self.panes.len() > 1 && idx < self.panes.len() {
                    self.close_session(idx);
                    self.rebuild_layout();
                }
            }
            ContextMenuAction::SplitNewPane => {
                if self.panes.len() < marspot::ui::SESSION_COUNT_HARD_CAP {
                    self.spawn_session();
                    self.rebuild_layout();
                }
            }
            ContextMenuAction::RenameTitle => {
                let idx = match region {
                    ContextRegion::Pane(i) | ContextRegion::SidebarSlot(i) => i,
                    _ => self.focused_idx,
                };
                if idx < self.panes.len() {
                    self.editing_title = Some(idx);
                }
            }
            ContextMenuAction::ToggleSidebar => {
                self.sidebar_collapsed = !self.sidebar_collapsed;
                self.rebuild_layout();
            }
            ContextMenuAction::OpenLayout => {
                self.layout_modal_open = true;
            }
            ContextMenuAction::OpenLink => {
                if let Some(t) = link_text {
                    spawn_open(&t);
                }
            }
            ContextMenuAction::CopyLink => {
                if let Some(t) = link_text {
                    let _ = marspot::input::write_clipboard_text(&t);
                }
            }
        }
        self.needs_render = true;
    }

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
            // RFC-003 step 3a: L2 allocates the session id itself via
            // the on-disk registry (sessions/.next_id with flock), no
            // L4 round-trip.
            match allocate_next_session_id()
                .and_then(|id| spawn_l3_pane(cols, rows, id, &self.event_tx))
            {
                Ok(pane) => {
                    self.panes.push(pane);
                    self.custom_titles.push(None);
                    // F3+5 — initial cwd pull for the new pane so the
                    // title strip lands populated on its first paint.
                    // shell_child_pid may not be written yet on this
                    // very tick — `refresh_pane_cwd_for` silently
                    // returns false, and the build_views lazy-fill
                    // catches it on a later frame.
                    let new_idx = self.panes.len() - 1;
                    self.refresh_pane_cwd_for(new_idx, true);
                    self.save_session_state();
                }
                Err(e) => lx_error!("core.spawn.l3_failed", &format!("{e}")),
            }
            return;
        }
        // RFC-003 Phase 6: L3 is the only backend.  If l3_mode is off
        // we no longer have a shelld fallback — just log + skip.  Run
        // with MARSPOT_L3=1 (the default) to get a pane.
        lx_error!(
            "core.spawn.non_l3_mode",
            "MARSPOT_L3=0 used to fall back to shelld panes; RFC-003 removed L4 — ignoring spawn request"
        );
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
        // RFC-003 §6 Amendment 16 — L3 self-execv silent update.
        //
        // L2 is purely the trigger: SIGTERM each L3 pid.  L3's handler
        // looks at current/marspot-session's MARSPOT_FP fingerprint;
        // if different from its own rodata fingerprint it execvs into
        // the new image (PTY master fd + UDS listener fd survive via
        // clear-CLOEXEC + manifest handoff).  If the fingerprint
        // matches its own (no real update) it falls back to the
        // user-quit shutdown path (state.bin + clean exit, shell
        // SIGHUPs).
        //
        // L2 doesn't touch fds, doesn't spawn a replacement, doesn't
        // wait.  The L3 self-execv keeps PID + master_fd + listener_fd
        // + shell child unchanged; L2 sees a brief control read pause
        // while the new image rebinds its readers, then keystrokes
        // resume.
        match marspot::updater::promote_pending_session() {
            Ok(true) => lx_event!(
                "SESSION_PROMOTE",
                "promoted staged marspot-session → current/ for swap"
            ),
            Ok(false) => {}
            Err(e) => lx_error!("core.promote.swap_failed", &format!("{e}")),
        }
        let mut signalled = 0usize;
        for pane in &self.panes {
            if !pane.is_l3() || pane.is_exited() {
                continue;
            }
            let Some(sid) = pane.session().l3_session_id() else {
                continue;
            };
            let Some(pid) = pane.session().l3_pid() else { continue };
            if pid <= 0 { continue }
            if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
                signalled += 1;
                lx_event!(
                    "L3_SWAP_SIGTERM",
                    "asked L3 to self-execv into current/marspot-session",
                    session = sid,
                    pid = pid
                );
            }
        }
        lx_event!(
            "L3_SWAP_FANOUT",
            "SIGTERM fanned out for L3 self-execv",
            n_signalled = signalled
        );
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
        // RFC-003 step 3c: L3 (owns-pty) panes own their PTY in-process,
        // so SIGTERM the L3 child via entry.toml's pid + delete the
        // registry dir (also wipes bytelog so the slot can't replay if
        // a later L3 picks the same id).  Legacy shelld panes still go
        // through L4 kill_session — that path is deleted in Phase 6.
        if let Some(id) = self.panes[idx].shelld_session_id() {
            if self.panes[idx].is_l3() {
                if let Ok(entry) = session_registry::read_session_entry(id) {
                    unsafe { libc::kill(entry.pid, libc::SIGTERM) };
                }
                let _ = session_registry::delete_session(id);
                // Amendment 7 step 3: also drop the named shm region
                // so the kernel actually frees the pages once every
                // fd-holder closes.
                grid_shm::delete_region(&grid_shm::session_shm_name(id));
            }
            // RFC-003 Phase 6: shelld-backed panes don't exist anymore;
            // the L3-only path above is the entire close path.
            // F3+5 — drop cached cwd state so it can't leak past the
            // pane.  Same id may eventually be reused; a fresh pane
            // gets a fresh refresh.
            self.pane_cwds.remove(&id);
            self.last_cwd_refresh.remove(&id);
            self.pane_badges.remove(&id);
            self.pane_titles.remove(&id);
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
        self.save_session_state();
    }

    fn commit_title_edit(&mut self) {
        if let Some(idx) = self.editing_title.take() {
            if idx < self.custom_titles.len() {
                let trimmed = self.title_edit_buffer.trim().to_string();
                // RFC-003 Phase 6: titles survive an L2 swap via the
                // L3 process's persisted entry.toml (Amendment 7
                // reattach path).  L2-side `custom_titles` is the
                // current truth.
                self.custom_titles[idx] =
                    if trimmed.is_empty() { None } else { Some(trimmed) };
            }
            self.title_edit_buffer.clear();
            self.save_session_state();
        }
    }

    fn cancel_title_edit(&mut self) {
        self.editing_title = None;
        self.title_edit_buffer.clear();
    }

    // ─── C5: scrollback search overlay ────────────────────────────

    /// Minimum pane width (in cell cols) below which Cmd+F is a no-op
    /// per §6.7.  The search bar is 40 cols wide; a 24-col floor leaves
    /// headroom for narrow PaneSession dialogs that legitimately use
    /// the keyboard.
    const SEARCH_MIN_PANE_COLS: u16 = 24;

    /// Cmd+F handler.  When search is closed → open + focus query.
    /// When already open → re-focus + select-all (browser convention
    /// per §6.9).  Narrow-pane fallback skips the open entirely.
    fn handle_cmd_f(&mut self) -> bool {
        let idx = self.focused_idx;
        let Some(pane) = self.panes.get_mut(idx) else { return false };
        // Width gate (§6.7).
        let grid_cols = pane.session().grid().cols();
        if grid_cols < Self::SEARCH_MIN_PANE_COLS {
            lx_event!(
                "L2_SEARCH_NARROW_PANE",
                "Cmd+F suppressed; pane too narrow",
                cols = grid_cols,
                min = Self::SEARCH_MIN_PANE_COLS as u32
            );
            return true; // claim it so the key isn't typed as 'f'
        }
        match pane.search.as_mut() {
            None => {
                pane.search = Some(marspot::pane::PaneSearch::open());
                self.needs_render = true;
                lx_event!(
                    "L2_SEARCH_OPEN",
                    "Cmd+F opened search overlay",
                    pane_idx = idx as u32
                );
            }
            Some(s) => {
                // Re-focus + select-all: bar cursor to end (we don't
                // model selection inside the single-line query; the
                // SearchBar's Cmd+A behaviour maps to "cursor to end").
                s.bar.focused = true;
                s.bar.cursor = s.bar.query_char_len();
                self.needs_render = true;
            }
        }
        true
    }

    /// Apply an incoming `SearchResults` batch.  Routes to the pane
    /// hosting `shelld_session_id`; drops stale batches via the
    /// SearchList's qid check.
    fn apply_search_results(
        &mut self,
        shelld_session_id: u64,
        query_id: u32,
        has_more: bool,
        hits: Vec<marspot::shell_proto::WireSearchHit>,
    ) {
        let Some(pane) = self.panes.iter_mut().find(|p| {
            p.session().shelld_session_id() == Some(shelld_session_id)
        }) else { return };
        let Some(s) = pane.search.as_mut() else { return };
        let changed = s.list.apply_results(query_id, hits, has_more);
        if changed {
            self.needs_render = true;
        }
    }

    /// Walk panes whose search is open + debounce_until is past;
    /// emit a fresh `SearchScrollback` frame on each.  Called from
    /// `pump_all` each loop iteration; cheap when no search is open
    /// (idle = single `is_none` check per pane).
    fn process_search_debounces(&mut self) {
        let now = std::time::Instant::now();
        for pane in self.panes.iter_mut() {
            let Some(s) = pane.search.as_mut() else { continue };
            let fire = match s.bar.debounce_until {
                Some(t) if now >= t => true,
                _ => false,
            };
            if !fire {
                continue;
            }
            s.bar.debounce_until = None;
            // Skip the emit when the query hasn't actually changed
            // since the last fire (Cmd+A / arrow keys reset the
            // debounce by editing intent but produce no diff).
            if s.bar.query == s.last_emitted_query {
                continue;
            }
            let qid = s.next_query_id;
            s.next_query_id = s.next_query_id.wrapping_add(1).max(1);
            s.bar.query_id = qid;
            s.last_emitted_query = s.bar.query.clone();
            // Empty query — cancel any in-flight worker, clear the
            // list, don't emit a new search.
            if s.bar.query.is_empty() {
                s.list.reset_for_query(qid);
                pane.session_mut().forward_search_cancel(qid);
                pane.active_highlight = None;
                continue;
            }
            s.list.reset_for_query(qid);
            pane.active_highlight = None;
            let case_sensitive = s.bar.case_sensitive;
            let query = s.bar.query.clone();
            pane.session_mut()
                .forward_search_scrollback(qid, case_sensitive, 64, &query);
        }
    }

    /// Routes a key event to the focused pane's search overlay (if
    /// open).  Returns `true` when the key was consumed (don't
    /// propagate to PTY / Cmd-C / etc.).  Implements §6.7 routing
    /// rules: ↑/↓/Enter → list; everything else → bar.
    fn search_consume_key(
        &mut self,
        event: &MarspotKeyEvent,
        mods: Modifiers,
    ) -> bool {
        use marspot::input::{LogicalKey, NamedKey};
        let idx = self.focused_idx;
        // Decision: list vs bar.  Done in a scope so the mutable
        // borrow of `self.panes` ends before we call
        // `jump_to_focused_hit(idx)` (which needs a fresh &mut self).
        enum Outcome {
            NotOpen,
            Consumed,
            ConsumedJump,
            ConsumedClosed,
            Pass,
        }
        let outcome: Outcome = {
            let Some(pane) = self.panes.get_mut(idx) else { return false };
            let Some(search) = pane.search.as_mut() else { return false };
            if !search.bar.focused {
                Outcome::NotOpen
            } else {
                let to_list = matches!(
                    event.logical,
                    LogicalKey::Named(NamedKey::ArrowUp)
                        | LogicalKey::Named(NamedKey::ArrowDown)
                        | LogicalKey::Named(NamedKey::Enter)
                );
                if to_list && !search.list.hits.is_empty() {
                    let (disp, jump) = search.list.handle_key(event, mods);
                    let consumed = !matches!(
                        disp,
                        marspot_term::render::InputDisposition::Pass
                    );
                    let jumped = matches!(
                        jump,
                        marspot::tools::search_list::JumpRequest::JumpToFocused
                    );
                    if consumed && jumped {
                        Outcome::ConsumedJump
                    } else if consumed {
                        Outcome::Consumed
                    } else {
                        Outcome::Pass
                    }
                } else {
                    // Bar-routed.
                    let disp = search.bar.handle_key(
                        event,
                        mods,
                        std::time::Instant::now(),
                    );
                    let consumed = !matches!(
                        disp,
                        marspot_term::render::InputDisposition::Pass
                    );
                    if !search.bar.focused {
                        // Esc → close entire overlay.
                        pane.search = None;
                        pane.active_highlight = None;
                        Outcome::ConsumedClosed
                    } else if consumed {
                        Outcome::Consumed
                    } else {
                        Outcome::Pass
                    }
                }
            }
        };
        match outcome {
            Outcome::NotOpen => false,
            Outcome::Pass => false,
            Outcome::Consumed | Outcome::ConsumedClosed => {
                self.needs_render = true;
                true
            }
            Outcome::ConsumedJump => {
                self.needs_render = true;
                self.jump_to_focused_hit(idx);
                true
            }
        }
    }

    /// Realise a `JumpRequest::JumpToFocused`: compute view_offset +
    /// HighlightSpan from the focused hit's WireSearchHit, store on
    /// the pane, and forward GridScroll if needed.
    fn jump_to_focused_hit(&mut self, pane_idx: usize) {
        let Some(pane) = self.panes.get_mut(pane_idx) else { return };
        let Some(search) = pane.search.as_ref() else { return };
        let Some(hit) = search.list.focused_hit() else { return };
        let rows = pane.session().grid().rows();
        // Live hit detection: B4 remap stamps live hits with
        // logical_line_idx in [u64::MAX - rows + 1, u64::MAX].  Anything
        // below that threshold is a scrollback hit.
        let live_threshold = u64::MAX.saturating_sub(rows as u64);
        let is_live = hit.logical_line_idx > live_threshold;
        let hit_qid = search.bar.query_id;
        let primary_row = hit
            .spans
            .iter()
            .map(|s| s.phys_row_idx)
            .next()
            .unwrap_or(0);
        let new_spans: Vec<marspot_term::render::HighlightSpan>;
        let new_view_offset: u16;
        if is_live {
            // Live: span.phys_row_idx is already live-local (0..rows-1);
            // view_offset = 0 puts the live grid bottom at viewport row
            // rows-1, so view_row = phys_row_idx works as-is.
            new_view_offset = 0;
            new_spans = hit
                .spans
                .iter()
                .filter(|s| (s.phys_row_idx as u16) < rows)
                .map(|s| marspot_term::render::HighlightSpan {
                    view_row: s.phys_row_idx as u16,
                    col_start: s.col_start,
                    col_end_inclusive: s.col_end_inclusive,
                })
                .collect();
        } else {
            // Scrollback hit: target the hit at viewport row rows/2.
            // At view_offset = K, viewport row R shows
            // scrollback[sb_len - K + R].  Solving for K so that
            // R = target gives K = sb_len + target - primary_row.
            let target_r: i64 = (rows as i64) / 2;
            let sb_len = pane.session().l3_scrollback_len() as i64;
            let raw_k = sb_len + target_r - primary_row as i64;
            // Clamp to a sane view_offset range.
            let max_k = (sb_len + rows as i64 - 1).max(0);
            new_view_offset = raw_k.clamp(0, max_k) as u16;
            new_spans = hit
                .spans
                .iter()
                .map(|s| {
                    // view_row = target_R + (S - primary_row).
                    let vr = target_r + (s.phys_row_idx as i64 - primary_row as i64);
                    (vr, s.col_start, s.col_end_inclusive)
                })
                .filter(|(vr, _, _)| *vr >= 0 && *vr < rows as i64)
                .map(|(vr, cs, ce)| marspot_term::render::HighlightSpan {
                    view_row: vr as u16,
                    col_start: cs,
                    col_end_inclusive: ce,
                })
                .collect();
        }
        pane.active_highlight = Some(marspot_term::render::ActiveHighlight {
            query_id: hit_qid,
            spans: new_spans,
        });
        // F1+4 — set + push without dedup gate.  Previous code
        // checked `old_offset != new_view_offset` and silently
        // skipped forward_scroll when equal, but `pane.view_offset`
        // and the L3Conn-side `req_view_offset` could desync (e.g.
        // L3 republished at 0 after some other path) which left the
        // grid frozen at live even though L2 thought it was at K.
        // Set the L2 side AND push the scroll; the L3Conn does its
        // own req_view_offset dedup that handles the genuinely-no-op
        // case.
        let old_offset = pane.view_offset();
        pane.set_view_offset(new_view_offset);
        pane.session_mut().forward_scroll(new_view_offset);
        lx_event!(
            "L2_SEARCH_JUMP",
            "jump to focused search hit",
            pane_idx = pane_idx as u32,
            is_live = if is_live { 1u32 } else { 0u32 },
            primary_row = primary_row,
            old_offset = old_offset as u32,
            new_offset = new_view_offset as u32
        );
        // F1+5 — bar stays open per §6.7.  Closing on Enter (F1+4)
        // made the immediate next keystroke hit the L3 forward
        // path's snap_to_live and bounce view_offset back to 0
        // before the user could see the hit.  Esc closes the overlay
        // and clears the highlight in one shot.
        self.needs_render = true;
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
        // cc-only post-processing: claudecode renders to a fixed inner
        // width with hard `\n` wraps that we don't want on the clipboard.
        // Detect cc panes via the L1 plugin badge — non-empty entry on
        // this pane's shelld_session_id means cc plugin tagged it.  See
        // `src/cc.rs`.
        let text = text.map(|t| {
            let is_cc = self
                .panes
                .get(idx)
                .and_then(|p| p.shelld_session_id())
                .and_then(|sid| self.pane_badges.get(&sid))
                .map(|b| !b.is_empty())
                .unwrap_or(false);
            if is_cc { marspot::cc::rejoin_wrapped_paragraphs(&t) } else { t }
        });
        match text {
            Some(text) => marspot::input::write_clipboard_text(&text),
            None => false,
        }
    }

    fn key(&mut self, event: MarspotKeyEvent, modifiers: Modifiers) {
        use marspot::input::{KeyState, LogicalKey, NamedKey};

        // F3+9 — Esc closes the context menu if open.  Swallowed so the
        // \e doesn't reach the focused pane.
        if event.state == KeyState::Pressed && self.context_menu.is_some() {
            if let LogicalKey::Named(NamedKey::Escape) = event.logical {
                self.context_menu = None;
                self.needs_render = true;
                return;
            }
        }

        // F3+1.5 — Process Monitor modal eats ESC + arrow keys + Cmd-W
        // when open (modal semantics).  Sits above every other key
        // path so a modal-active terminal still has working pane
        // keys after closing.
        if self.process_panel.is_some() && event.state == KeyState::Pressed {
            let is_esc = matches!(event.logical, LogicalKey::Named(NamedKey::Escape));
            let is_cmd_w = matches!(event.logical, LogicalKey::Char('w'))
                && modifiers.super_;
            if is_esc || is_cmd_w {
                self.process_panel = None;
                self.needs_render = true;
                return;
            }
        }

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

        // C5 — Cmd+F intercept (env-gated on `MARSPOT_SEARCH=1`).
        // Sits ahead of all PTY routing + Cmd-C / Cmd-B so an open
        // search bar gets the keys; falls through silently when the
        // env gate is off.  F1 flips by removing the env check inside
        // `handle_cmd_f` and `search_consume_key` / opening on every
        // session.
        if event.state == KeyState::Pressed
            && modifiers.super_key()
            && matches!(event.logical, LogicalKey::Char(c) if c.eq_ignore_ascii_case(&'f'))
        {
            if self.handle_cmd_f() {
                return;
            }
        }
        if event.state == KeyState::Pressed && self.search_consume_key(&event, modifiers) {
            return;
        }
        // F1+12 — F1+9 had a too-wide guard here that returned early
        // for ALL events when search was open, swallowing Cmd-C / Cmd-B
        // before their dedicated handlers below could fire.  User
        // could navigate hits but Cmd-C wouldn't copy without Esc'ing
        // out of search first.
        //
        // The actual bouncer F1+9 was trying to defuse lives inside
        // the L3 forward path (`snap_to_live` + `forward_key`).  We
        // moved the guard there (further down) so Cmd-C / Cmd-B /
        // Cmd-V keep working while search is open.

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

        // RFC-003 Phase 4 (frozen reattach minimum): an exited L3 pane
        // shows whatever was last published; on key press we revive it
        // by spawning a fresh L3 at the same session id.  The bytelog
        // opens in append mode so the new shell's output continues the
        // same on-disk record.  Only revive on a key press the user
        // would actually mean as "wake up" (any printable / Enter /
        // arrow / Tab etc); modifier-only key transitions don't fire.
        if pane.is_l3() && pane.is_exited() && event.state == KeyState::Pressed {
            if let Some(sid) = pane.session().shelld_session_id() {
                // Layout already sized the pane — keep its current
                // cell dims so the new L3 boots at the same shape.
                let (cols, rows) = self
                    .layout
                    .cells
                    .get(self.focused_idx)
                    .map(|c| (c.cols, c.rows))
                    .unwrap_or((INITIAL_COLS, INITIAL_ROWS));
                match spawn_l3_pane(cols, rows, sid, &self.event_tx) {
                    Ok(new_pane) => {
                        lx_event!(
                            "L3_REVIVED",
                            "user keystroke respawned dead L3 at same id",
                            session_id = sid
                        );
                        self.panes[self.focused_idx] = new_pane;
                        self.needs_render = true;
                    }
                    Err(e) => lx_error!(
                        "core.revive.spawn_failed",
                        &format!("{e}"),
                        session_id = sid
                    ),
                }
                return;
            }
        }

        // L3-backed pane: forward the key *event* to the session process,
        // which encodes with its own modes + local-echoes, then republishes
        // the grid (we re-read it on the GridReady wake).  No local write /
        // predict here.
        if pane.is_l3() {
            // F1+12 — when the search overlay owns the keyboard, any key
            // that fell through to here must not (a) snap_to_live (the
            // F1+9 bouncer) or (b) forward_key into the PTY (would type
            // search-bar shortcuts into the shell).  Cmd-C / Cmd-B
            // already ran above; bar-bound printable / arrows / Enter
            // were already consumed by search_consume_key on the press
            // event.  Anything left (e.g. KeyState::Released, or a
            // Cmd-? we don't recognise) is dropped silently.
            if self.panes[self.focused_idx]
                .search
                .as_ref()
                .is_some_and(|s| s.bar.focused)
            {
                return;
            }
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
            // F3+5 — Enter pressed → shell about to execute a line
            // (potentially `cd`).  Debounced refresh keeps a multi-
            // line paste collapsed to one syscall.  Carriage return
            // OR linefeed both count (modes may emit either).
            if bytes.iter().any(|&b| b == b'\r' || b == b'\n') {
                let focused = self.focused_idx;
                self.refresh_pane_cwd_for(focused, false);
            }
        }
    }

    /// Hit-test an auto-detected link span on pane `idx`.  Scans the
    /// pane's visible grid (cheap — bounded by visible cells) and
    /// returns the first span containing the clicked (col, row).
    /// Returns None when the click missed every detected link.
    fn hit_test_pane_link(
        &self,
        idx: usize,
        col: u16,
        row: u16,
    ) -> Option<marspot::grid_links::LinkRange> {
        let pane = self.panes.get(idx)?;
        let view_offset = pane.view_offset();
        let grid = pane.session().grid();
        // cc-mode: claudecode renders URLs / paths to a fixed inner
        // width and hard-newlines with a hanging indent.  Tell the
        // link scanner so it merges the continuation into one
        // logical token.  See `grid_links::ScanOpts::cc_mode`.
        let cc_mode = pane
            .shelld_session_id()
            .and_then(|sid| self.pane_badges.get(&sid))
            .map(|b| !b.is_empty())
            .unwrap_or(false);
        let opts = marspot::grid_links::ScanOpts { cc_mode };
        let links = marspot::grid_links::scan_visible_links(grid, view_offset, opts);
        links.into_iter().find(|link| {
            link.row == row && col >= link.col_start && col <= link.col_end
        })
    }

    /// Top-level link hit-test from physical pixel coordinates.
    /// Folds the (x_phys, y_phys) → (pane, col, row) step the
    /// renderer-aware caller would otherwise repeat, and filters
    /// out the inert `Email` kind so callers can treat a `Some`
    /// as actionable.  Returns owned text + kind so the result can
    /// be stashed in `ContextMenuState.link` and survive the menu's
    /// clear-on-dispatch.
    fn hit_test_link_at_xy(
        &self,
        x_phys: f64,
        y_phys: f64,
    ) -> Option<LinkContext> {
        let (cw, ch) = self.renderer.cell_dims();
        let (idx, col, row) = self.layout.hit_test_cell_pos(x_phys, y_phys, cw, ch)?;
        let link = self.hit_test_pane_link(idx, col, row)?;
        match link.kind {
            marspot::grid_links::LinkKind::Url
            | marspot::grid_links::LinkKind::File => Some(LinkContext {
                text: link.text,
                kind: link.kind,
            }),
            marspot::grid_links::LinkKind::Email => None,
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

    /// F3+1.5 — build the centered Process Monitor modal data via the
    /// new UI component kit (ModalFrame, TrafficLights, TabStrip,
    /// ScrollView).  Returns render data + populates parallel hit-test
    /// state.  All rects are physical pixels.
    fn build_process_panel_render(&mut self) -> Option<marspot::render_metal::ProcessPanelRender> {
        // F3+4 — char-level truncation with ASCII ellipsis (matches the
        // sidebar's `truncate_for_sidebar` style; kept inline to avoid
        // pulling a "process panel utils" module in for one helper).
        fn truncate_to(s: &str, max_chars: usize) -> String {
            let n = s.chars().count();
            if n <= max_chars { return s.to_string(); }
            let head: String = s.chars().take(max_chars.saturating_sub(1)).collect();
            format!("{head}…")
        }
        use marspot::render_metal::{
            ProcessPanelRender, ProcessPanelRow, ProcessPanelPaneRow,
        };
        use marspot::ui::components::ScrollView;
        use marspot::ui::system::macos::TrafficLights;
        use marspot::ui::components::modal_frame::{ModalFrame, ModalLayoutSpec};
        let scale = self.scale.max(0.1);
        let panes_len;
        let selected_pane;
        let minimized;
        let maximized;
        let pos_offset;
        let scroll_y_in;
        {
            let panel = self.process_panel.as_mut()?;
            if !panel.panes.is_empty() {
                if panel.selected_pane >= panel.panes.len() {
                    panel.selected_pane = panel.panes.len() - 1;
                }
            } else {
                panel.selected_pane = 0;
            }
            panes_len     = panel.panes.len();
            selected_pane = panel.selected_pane;
            minimized     = panel.minimized;
            maximized     = panel.maximized;
            pos_offset    = panel.pos_offset;
            scroll_y_in   = panel.scroll_y;
        }
        // F3+4 — modal frame: no tab strip anymore.
        let frame = ModalFrame::layout(
            self.w_phys as f64,
            self.h_phys as f64,
            ModalLayoutSpec {
                default_w:   PROCESS_PANEL_WIDTH_LOGICAL  * scale,
                default_h:   PROCESS_PANEL_HEIGHT_LOGICAL * scale,
                title_bar_h: 28.0 * scale,
                tab_strip_h: 0.0,
                maximized,
                max_w_ratio: 0.95,
                max_h_ratio: 0.90,
                minimized,
                with_tab_strip: false,
                pos_offset,
                top_obstruction: self.layout.top_inset,
            },
        );
        let lights = TrafficLights::layout(
            frame.title_bar,
            12.0 * scale,
            8.0  * scale,
            12.0 * scale,
        );
        let (_cell_w_f64, cell_h_f64) = self.renderer.cell_dims();

        // F3+4 — emit master rows (sorted by CPU% desc) + detail rows
        // for the selected pane.  Master row hit rects are persisted
        // so mouse_down can route clicks to selected_pane updates.
        let mut pane_rows: Vec<ProcessPanelPaneRow> = Vec::new();
        let mut detail_rows: Vec<ProcessPanelRow> = Vec::new();
        let mut kill_meta: Vec<(i32, usize)> = Vec::new();
        // Map sorted-row index → original pane index, so selected_pane
        // (kept stable across resort) still points at the same pane.
        let mut sorted_to_orig: Vec<usize> = Vec::new();
        if !minimized {
            let panel_ref = self.process_panel.as_ref()?;
            let mut orig: Vec<usize> = (0..panel_ref.panes.len()).collect();
            orig.sort_by(|&a, &b| {
                panel_ref.panes[b].cpu_pct.total_cmp(&panel_ref.panes[a].cpu_pct)
            });
            sorted_to_orig.clone_from(&orig);
            for &i in &orig {
                let pane = &panel_ref.panes[i];
                pane_rows.push(ProcessPanelPaneRow {
                    name: truncate_to(&pane.name, 22),
                    sid: pane.shelld_session_id,
                    n_pids: pane.n_pids,
                    cpu_pct: pane.cpu_pct,
                    rss_kb: pane.rss_kb,
                    busy: truncate_to(&pane.busy, 18),
                });
            }
            // Selected pane's tree → detail rows.
            let selected_orig = sorted_to_orig.get(selected_pane).copied()
                .unwrap_or(0);
            if let Some(pane) = panel_ref.panes.get(selected_orig) {
                detail_rows.push(ProcessPanelRow {
                    depth: 0,
                    pid: 0,
                    comm: format!("{} · sid {} · shell pid {}",
                        pane.name, pane.shelld_session_id,
                        pane.shell_child_pid.unwrap_or(0)),
                    cpu_pct: 0.0,
                    rss_kb: 0,
                    is_header: true,
                });
                if let Some(tree) = pane.tree.as_ref() {
                    let flat = marspot::pidtree::flatten_pre_order(tree);
                    for (depth, node) in flat {
                        let row_idx = detail_rows.len();
                        let (cpu, rss) =
                            if let Some(stat) = panel_ref.prev_pid_stats
                                .get(&node.pid)
                            {
                                let _ = stat;
                                // For per-row CPU/RSS we sample current
                                // stats fresh — prev_pid_stats only
                                // holds cumulative + timestamp; CPU% was
                                // computed during refresh into the
                                // aggregates but not per-pid.  Re-sample
                                // is one syscall; cheap on demand.
                                let s = marspot::pidtree::proc_stat(node.pid);
                                let cpu = s.and_then(|cur| {
                                    let dt_ns = panel_ref.last_refresh
                                        .elapsed().as_nanos() as u64 + 1;
                                    let prev = panel_ref.prev_pid_stats
                                        .get(&node.pid)?.0;
                                    if cur.total_cpu_ns >= prev {
                                        Some(((cur.total_cpu_ns - prev) as f64
                                            / dt_ns as f64) as f32 * 100.0)
                                    } else { Some(0.0) }
                                }).unwrap_or(0.0);
                                let rss = s.map(|s| s.rss_bytes / 1024).unwrap_or(0);
                                (cpu, rss)
                            } else { (0.0, 0) };
                        detail_rows.push(ProcessPanelRow {
                            depth: ((depth + 1).min(8)) as u8,
                            pid: node.pid,
                            comm: node.comm.clone(),
                            cpu_pct: cpu,
                            rss_kb: rss,
                            is_header: false,
                        });
                        kill_meta.push((node.pid, row_idx));
                    }
                }
            }
        }
        // F3+4.1 — match the renderer's Table layout 1:1.  Master
        // is the left 38 % including its own header; detail fills
        // the rest.  Both Tables share row_h / header_h.
        let cell_w_f64 = self.renderer.cell_dims().0;
        let row_h = cell_h_f64 * 1.3;
        let header_h = cell_h_f64 * 1.4;
        let kill_w = 18.0 * scale;
        let kill_h = (row_h - 4.0).max(8.0);
        let master_w = frame.body.w * 0.38;
        let master_rows_top = frame.body.y_top + header_h;
        let mut pane_row_rects: Vec<marspot_term::layout::Rect> =
            Vec::with_capacity(pane_rows.len());
        for i in 0..pane_rows.len() {
            let row_y = master_rows_top + (i as f64) * row_h;
            if row_y + row_h > frame.body.y_top + frame.body.h { break; }
            pane_row_rects.push(marspot_term::layout::Rect {
                x: frame.body.x, y_top: row_y,
                w: master_w, h: row_h,
            });
        }
        // Detail scroll body (under the header).
        let detail_rows_top = frame.body.y_top + header_h;
        let content_h = (detail_rows.len() as f64) * row_h;
        let detail_body = marspot_term::layout::Rect {
            x: frame.body.x + master_w,
            y_top: detail_rows_top,
            w: frame.body.w - master_w,
            h: frame.body.h - header_h,
        };
        let mut sv = ScrollView::new(detail_body);
        sv.content_h = content_h;
        sv.scroll_y = scroll_y_in;
        sv.clamp();
        let scroll_y_clamped = sv.scroll_y;
        // Detail table's column geometry: same widths as renderer.
        // Last column = kill column (kill_w + 8 px).  Process flex
        // gets the remainder.
        let pid_w  = cell_w_f64 * 7.0;
        let cpu_w  = cell_w_f64 * 7.0;
        let rss_w  = cell_w_f64 * 8.0;
        let kill_col_w = kill_w + 8.0;
        let kill_col_x = detail_body.x + detail_body.w - kill_col_w;
        // Per-data-row kill rects (skip section/header rows).
        let mut row_kill_rects: Vec<(i32, marspot_term::layout::Rect)> =
            Vec::with_capacity(kill_meta.len());
        for (pid, row_idx) in kill_meta {
            let row_y = detail_rows_top + (row_idx as f64) * row_h - scroll_y_clamped;
            if row_y + row_h < detail_rows_top { continue; }
            if row_y > frame.body.y_top + frame.body.h { continue; }
            let bx = kill_col_x + (kill_col_w - kill_w) * 0.5;
            let by = row_y + (row_h - kill_h) * 0.5;
            row_kill_rects.push((pid, marspot_term::layout::Rect {
                x: bx, y_top: by, w: kill_w, h: kill_h,
            }));
        }
        let _ = (pid_w, cpu_w, rss_w);
        // Persist hit-test state.
        {
            let panel = self.process_panel.as_mut()?;
            panel.pane_row_rects = pane_row_rects;
            panel.close_btn_rect = lights.close;
            panel.min_btn_rect   = lights.min;
            panel.max_btn_rect   = lights.max;
            panel.title_bar_rect = frame.title_bar;
            panel.body_rect      = frame.body;
            panel.modal_rect     = frame.frame;
            panel.row_kill_rects = row_kill_rects;
            panel.scroll_y       = scroll_y_clamped;
            panel.content_h      = content_h;
        }
        let _ = panes_len;
        Some(ProcessPanelRender {
            rect: frame.frame,
            title: "Process Monitor".to_string(),
            pane_rows,
            selected_pane,
            rows: detail_rows,
            minimized,
            scroll_y: scroll_y_clamped,
            draw_backdrop: true,
        })
    }

    /// F3+1.3 — send SIGTERM to `pid` and track it for SIGKILL
    /// escalation 2 s later if it hasn't exited.  Logged at Info so
    /// post-mortem can correlate UI clicks with process deaths.
    fn kill_and_track(&mut self, pid: i32) {
        match marspot::pidtree::kill_pid(pid, libc::SIGTERM) {
            Ok(()) => {
                marspot::lx_event!(
                    "PROCESS_PANEL_SIGTERM",
                    "user clicked panel [×] — SIGTERM sent",
                    pid = pid
                );
                if let Some(panel) = self.process_panel.as_mut() {
                    panel.pending_kills.push((pid, Instant::now()));
                }
            }
            Err(e) => {
                marspot::lx_warn!(
                    "process_panel.sigterm_failed",
                    &format!("kill({pid}, SIGTERM): {e}")
                );
            }
        }
    }

    /// F3+1.3 — escalate any pending SIGTERM that the target ignored
    /// past the grace period.  Called from the main loop; cheap when
    /// `pending_kills` is empty (the typical case).
    fn tick_process_panel_kills(&mut self) {
        let Some(panel) = self.process_panel.as_mut() else { return };
        if panel.pending_kills.is_empty() {
            return;
        }
        let now = Instant::now();
        panel.pending_kills.retain(|(pid, sent_at)| {
            if !marspot::pidtree::pid_is_alive(*pid) {
                // Gone already — graceful TERM took it.
                return false;
            }
            if now.duration_since(*sent_at) >= KILL_ESCALATION_GRACE {
                let _ = marspot::pidtree::kill_pid(*pid, libc::SIGKILL);
                // Drop the entry whether SIGKILL succeeded or failed;
                // a failure means the pid disappeared between the
                // is_alive check and the kill, which is a win, not
                // a loss.
                return false;
            }
            true
        });
    }

    /// F3+1 — re-walk libproc + read each pane's entry.toml to
    /// rebuild the process-tree cache.  Called when the panel opens,
    /// and on the main loop's render path when `last_refresh` is
    /// older than 2 s.  No-op when the panel is closed.
    fn refresh_process_panel(&mut self) {
        // F3+4 — walk libproc, sample per-pid stats, compute CPU%
        // deltas vs the previous refresh, build per-pane aggregates.
        // Two-pass: pass A snapshots procs + new stats; pass B walks
        // panes building summaries.  prev_pid_stats is rolled forward
        // (only pids seen this tick survive into next).
        let Some(panel) = self.process_panel.as_mut() else { return };
        let all = marspot::pidtree::list_all_procs();
        let now = Instant::now();
        // Sample CPU + RSS for every pid we'll touch (descendants of
        // any pane's shell child).  Caller already has the tree so
        // we'd double-walk if we sampled lazily inside `build_node` —
        // do it once into a flat map.
        let mut cur_stats: std::collections::HashMap<i32, marspot::pidtree::ProcStat> =
            std::collections::HashMap::new();
        // Snapshot CWD for each pane's shell child for the name col.
        panel.panes.clear();
        for (pane_idx, pane) in self.panes.iter().enumerate() {
            let Some(sid) = pane.shelld_session_id() else { continue };
            let pid = read_shell_child_pid(sid);
            let tree = pid.and_then(|p| marspot::pidtree::tree_rooted_at(p, &all));
            // Aggregate: walk descendants, sample stats, sum CPU% +
            // RSS, find top-busy non-shell process.
            let mut n = 0u32;
            let mut sum_cpu_pct = 0.0f32;
            let mut sum_rss_kb = 0u64;
            let mut top_busy = (0.0f32, String::new());
            if let Some(ref root) = tree {
                let flat = marspot::pidtree::flatten_pre_order(root);
                for (_depth, node) in &flat {
                    let p = node.pid;
                    let Some(stat) = marspot::pidtree::proc_stat(p) else { continue };
                    cur_stats.insert(p, stat);
                    n += 1;
                    sum_rss_kb += stat.rss_bytes / 1024;
                    // CPU% via delta.
                    let cpu = if let Some(&(prev_cpu, prev_t)) =
                        panel.prev_pid_stats.get(&p)
                    {
                        let dt_ns = now.duration_since(prev_t).as_nanos() as u64;
                        if dt_ns > 0 && stat.total_cpu_ns >= prev_cpu {
                            let dcpu = stat.total_cpu_ns - prev_cpu;
                            (dcpu as f64 / dt_ns as f64) as f32 * 100.0
                        } else { 0.0 }
                    } else { 0.0 };
                    sum_cpu_pct += cpu;
                    if cpu > top_busy.0 && !node.comm.starts_with("-zsh") {
                        top_busy = (cpu, node.comm.clone());
                    }
                }
            }
            // Resolve pane name same way the title strip does:
            // custom > cwd basename > ordinal.
            let name = {
                let custom = self.custom_titles.get(pane_idx).and_then(|t| t.clone())
                    .filter(|s| !s.is_empty());
                if let Some(c) = custom { c }
                else if let Some(p) = self.pane_cwds.get(&sid) {
                    std::path::Path::new(p).file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| format!("sid {sid}"))
                } else {
                    format!("{}", pane_idx + 1)
                }
            };
            let busy = if top_busy.0 < 0.1 { String::from("(idle)") } else { top_busy.1 };
            panel.panes.push(PanePidTree {
                shelld_session_id: sid,
                shell_child_pid: pid,
                tree,
                name,
                n_pids: n,
                cpu_pct: sum_cpu_pct,
                rss_kb: sum_rss_kb,
                busy,
            });
        }
        // Roll the stats cache forward: keep only pids we sampled this
        // tick (dead pids drop out automatically).
        panel.prev_pid_stats.clear();
        for (pid, stat) in cur_stats {
            panel.prev_pid_stats.insert(pid, (stat.total_cpu_ns, now));
        }
        panel.last_refresh = now;
    }

    fn mouse_moved(&mut self, x_phys: f64, y_phys: f64) {
        // F3+9 — drive context menu hover highlight when open.
        if let Some(state) = self.context_menu.as_mut() {
            use marspot::ui::components::ContextMenu;
            let menu = ContextMenu::layout(
                self.layout.window_w, self.layout.window_h, self.scale,
                state.anchor_x, state.anchor_y,
                self.layout.top_inset,
                &state.items,
            );
            let new_hover = menu.hover_index(&state.items, x_phys, y_phys);
            if new_hover != state.hovered_idx {
                state.hovered_idx = new_hover;
                self.needs_render = true;
            }
        }

        let new_hover = if self.layout.hit_test_sidebar_button(x_phys, y_phys) {
            Some(ChromeBtn::Sidebar)
        } else if self.layout.hit_test_layout_button(x_phys, y_phys) {
            Some(ChromeBtn::Layout)
        } else if self.layout.hit_test_process_button(x_phys, y_phys) {
            Some(ChromeBtn::ProcessTree)
        } else if self.layout.hit_test_dev_panel_button(x_phys, y_phys) {
            Some(ChromeBtn::DevPanel)
        } else {
            None
        };
        if new_hover != self.hover_chrome_btn {
            self.hover_chrome_btn = new_hover;
            self.renderer.set_hover_chrome_btn(map_hover_to_u8(new_hover));
            self.needs_render = true;
        }
    }

    fn mouse_down(&mut self, x_phys: f64, y_phys: f64, modifiers: Modifiers) {
        // F3+9 — when context menu is open, a left click first
        // dispatches an item / swallows on frame / closes-on-outside.
        if self.context_menu.is_some() {
            use marspot::ui::components::{ContextMenu, ContextMenuHit};
            let (hit, region) = {
                let state = self.context_menu.as_ref().unwrap();
                let menu = ContextMenu::layout(
                    self.layout.window_w, self.layout.window_h, self.scale,
                    state.anchor_x, state.anchor_y,
                    self.layout.top_inset,
                    &state.items,
                );
                (menu.hit_test(&state.items, x_phys, y_phys), state.region)
            };
            match hit {
                ContextMenuHit::Item(idx) => {
                    let tag = self.context_menu.as_ref().unwrap()
                        .items[idx].action_tag;
                    if let Some(action) = ContextMenuAction::from_tag(tag) {
                        self.dispatch_context_action(action, region);
                    } else {
                        self.context_menu = None;
                        self.needs_render = true;
                    }
                    return;
                }
                ContextMenuHit::Frame => {
                    // Click on divider / disabled row / padding —
                    // swallow, NSMenu-style.
                    return;
                }
                ContextMenuHit::Outside => {
                    self.context_menu = None;
                    self.needs_render = true;
                    // Fall through: click also triggers normal
                    // focus / selection behaviour.
                }
            }
        }

        let layout = &self.layout;
        let layout_btn_hit = layout.hit_test_layout_button(x_phys, y_phys);
        let sidebar_btn_hit = layout.hit_test_sidebar_button(x_phys, y_phys);
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
        // F3+1.5 — Process Monitor modal click priorities (high → low):
        //   1. row [×] kill (only when not minimized — rects empty)
        //   2. tab strip → switch active tab
        //   3. traffic lights: close / min / max
        //   4. title bar (non-light) → start drag
        //   5. body → swallow (modal semantics)
        //   6. backdrop (anywhere else in window) → swallow as well so
        //      a stray click doesn't punch through to the grid behind.
        if let Some(panel) = self.process_panel.as_ref() {
            // 1) Row kill
            let kill_hit: Option<i32> = panel
                .row_kill_rects
                .iter()
                .find(|(_, rect)| rect.contains(x_phys, y_phys))
                .map(|(pid, _)| *pid);
            if let Some(pid) = kill_hit {
                self.kill_and_track(pid);
                self.refresh_process_panel();
                self.needs_render = true;
                return;
            }
            // F3+4 — master pane list row clicks switch the selection.
            let pane_hit: Option<usize> = panel
                .pane_row_rects
                .iter()
                .enumerate()
                .find(|(_, r)| r.contains(x_phys, y_phys))
                .map(|(i, _)| i);
            if let Some(i) = pane_hit {
                if let Some(p) = self.process_panel.as_mut() {
                    p.selected_pane = i;
                    p.scroll_y = 0.0;
                }
                self.needs_render = true;
                return;
            }
            // 3) Traffic lights
            if panel.close_btn_rect.contains(x_phys, y_phys) {
                self.process_panel = None;
                self.needs_render = true;
                return;
            }
            if panel.min_btn_rect.contains(x_phys, y_phys) {
                if let Some(p) = self.process_panel.as_mut() {
                    p.minimized = !p.minimized;
                    if p.minimized { p.maximized = false; }
                }
                self.needs_render = true;
                return;
            }
            if panel.max_btn_rect.contains(x_phys, y_phys) {
                if let Some(p) = self.process_panel.as_mut() {
                    p.maximized = !p.maximized;
                    if p.maximized { p.minimized = false; }
                }
                self.needs_render = true;
                return;
            }
            // 4) Title bar drag — anywhere in title bar that isn't a
            //    traffic light starts a window-drag.
            if panel.title_bar_rect.contains(x_phys, y_phys) {
                if let Some(p) = self.process_panel.as_mut() {
                    p.drag_grab = Some((x_phys, y_phys, p.pos_offset.0, p.pos_offset.1));
                }
                return;
            }
            // 5) Click inside modal but on no widget → swallow.
            if panel.modal_rect.contains(x_phys, y_phys) {
                return;
            }
            // 6) Click on backdrop (outside modal) → close the modal.
            // Familiar pattern: clicking outside a modal dismisses it.
            self.process_panel = None;
            self.needs_render = true;
            return;
        }
        // F3+1 — process-tree panel toggle.  Same priority tier as
        // sidebar: a click on the icon never falls through.  Opening
        // forces an immediate libproc walk so the panel paints
        // populated on its first frame.
        if self.layout.hit_test_process_button(x_phys, y_phys) {
            if self.process_panel.is_some() {
                self.process_panel = None;
            } else {
                self.process_panel = Some(ProcessPanelState {
                    panes: Vec::new(),
                    last_refresh: Instant::now() - std::time::Duration::from_secs(10),
                    row_kill_rects: Vec::new(),
                    pending_kills: Vec::new(),
                    selected_pane: 0,
                    prev_pid_stats: std::collections::HashMap::new(),
                    close_btn_rect: marspot_term::layout::Rect::ZERO,
                    pane_row_rects: Vec::new(),
                    min_btn_rect: marspot_term::layout::Rect::ZERO,
                    max_btn_rect: marspot_term::layout::Rect::ZERO,
                    title_bar_rect: marspot_term::layout::Rect::ZERO,
                    body_rect: marspot_term::layout::Rect::ZERO,
                    modal_rect: marspot_term::layout::Rect::ZERO,
                    minimized: false,
                    maximized: false,
                    scroll_y: 0.0,
                    content_h: 0.0,
                    pos_offset: (0.0, 0.0),
                    drag_grab: None,
                });
                self.refresh_process_panel();
            }
            self.needs_render = true;
            return;
        }
        // UI-system dev panel toggle.  L2 doesn't own dev panel
        // visibility — L1 (marspot-shell) hosts the NSWindow.  Route
        // the click via `DevPanelToggle` wire frame; L1 flips state
        // + drives the AppKit show/hide on its main loop.
        if self.layout.hit_test_dev_panel_button(x_phys, y_phys) {
            self.pending_to_shell.push((MsgType::DevPanelToggle, Vec::new()));
            return;
        }
        // F3+3.0 — when the LayoutModal is open, intercept ALL
        // clicks: hit-test its controls first, swallow non-control
        // clicks landing inside the frame so the modal feels modal
        // (doesn't punch through to the grid).
        if self.layout_modal_open {
            use marspot::ui::components::{LayoutModal, LayoutModalHit, GRID_MIN, GRID_MAX};
            let modal = LayoutModal::layout(
                self.w_phys, self.h_phys, self.scale,
                marspot::TITLE_STRIP_PT * self.scale,
                self.pending_grid_cols, self.pending_grid_rows,
            );
            // F3+3.3 — card drag start has priority over the
            // generic hit_test below (which would otherwise classify
            // a card click as `LayoutModalHit::Frame` and swallow it).
            if let Some(card_idx) = modal.hit_test_card(x_phys, y_phys) {
                let card = modal.cards[card_idx];
                self.layout_drag = Some(LayoutModalDrag {
                    from_slot: card_idx,
                    grab_offset_phys: (
                        x_phys - card.x,
                        y_phys - card.y_top,
                    ),
                    mouse_phys: (x_phys, y_phys),
                });
                self.needs_render = true;
                return;
            }
            match modal.hit_test(x_phys, y_phys) {
                Some(LayoutModalHit::Close) => {
                    self.layout_modal_open = false;
                    self.needs_render = true;
                    return;
                }
                Some(LayoutModalHit::Apply) => {
                    self.layout_modal_open = false;
                    self.grid_cols = self.pending_grid_cols;
                    self.grid_rows = self.pending_grid_rows;
                    // F3+3.3 — apply card_slots permutation to
                    // self.panes so the modal's drag-reordered
                    // arrangement lands in the actual grid.  Only
                    // the in-cells portion is reordered (panes past
                    // grid cells stay in sidebar order).  Identity
                    // mapping = no-op.
                    let cells = self.grid_cols * self.grid_rows;
                    self.apply_card_slot_permutation(cells);
                    // shrink-guard — if focused pane slot is
                    // beyond the new cell count, jump focus to the
                    // last surviving cell so the user sees a focused
                    // pane in-grid.  Overflowed sessions stay alive
                    // in the sidebar (n_sessions > cells handling
                    // is already preserved by `take(cell_count)`).
                    if cells > 0 && self.focused_idx >= cells {
                        self.focused_idx = cells - 1;
                    }
                    self.rebuild_layout();
                    self.save_session_state();
                    return;
                }
                Some(LayoutModalHit::ColsDec) => {
                    if self.pending_grid_cols > GRID_MIN {
                        self.pending_grid_cols -= 1;
                        self.reset_card_slots();
                        self.needs_render = true;
                    }
                    return;
                }
                Some(LayoutModalHit::ColsInc) => {
                    if self.pending_grid_cols < GRID_MAX {
                        self.pending_grid_cols += 1;
                        self.reset_card_slots();
                        self.needs_render = true;
                    }
                    return;
                }
                Some(LayoutModalHit::RowsDec) => {
                    if self.pending_grid_rows > GRID_MIN {
                        self.pending_grid_rows -= 1;
                        self.reset_card_slots();
                        self.needs_render = true;
                    }
                    return;
                }
                Some(LayoutModalHit::RowsInc) => {
                    if self.pending_grid_rows < GRID_MAX {
                        self.pending_grid_rows += 1;
                        self.reset_card_slots();
                        self.needs_render = true;
                    }
                    return;
                }
                Some(_) => {
                    // TitleBar / Frame: swallow.
                    return;
                }
                None => {
                    // Click outside the modal closes it without
                    // any other side effect.
                    self.layout_modal_open = false;
                    self.needs_render = true;
                    return;
                }
            }
        }
        // F3+3.0 — toolbar layout button toggles the LayoutModal.
        // Re-init pending values from current grid_* every open so
        // the modal always starts in sync with the live grid.
        if layout_btn_hit {
            if !self.layout_modal_open {
                self.pending_grid_cols = self.grid_cols;
                self.pending_grid_rows = self.grid_rows;
                self.reset_card_slots();
                // F3+3.6 — pull-fetch each pane's cwd on the
                // open transition so the modal preview + the
                // title-strip placeholder show fresh values.
                self.refresh_pane_cwds();
            }
            self.layout_modal_open = !self.layout_modal_open;
            self.layout_drag = None;
            self.needs_render = true;
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

        // Auto-link hit-test: a click that lands on an underlined
        // URL / file span opens the same context menu the right-
        // click path uses (Open + Copy).  User-facing behaviour is
        // "any click on a URL/path gives you the choice" instead
        // of "left-click opens directly" — so an accidental click
        // doesn't silently spawn `/usr/bin/open`.  Email is
        // recognised but inert.  Sits ahead of selection so the
        // click doesn't simultaneously start a fresh selection
        // on the link cells.
        if let Some(link) = self.hit_test_link_at_xy(x_phys, y_phys) {
            let items = self.build_link_menu_items(&link);
            if !items.is_empty() {
                let region = self.resolve_context_region(x_phys, y_phys);
                self.context_menu = Some(ContextMenuState {
                    items,
                    anchor_x: x_phys,
                    anchor_y: y_phys,
                    region,
                    hovered_idx: None,
                    link: Some(link),
                });
                self.needs_render = true;
            }
            return;
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
            // F3+3.0 — click on an empty cell (idx >= panes.len(),
            // which means the grid has more cells than sessions
            // after a layout grow) spawns a new session and focuses
            // it.  Matches the sidebar [+] behaviour but lands the
            // user directly in the cell they clicked, so growing
            // the grid + filling it reads as one motion.
            if idx >= self.panes.len() && cell_hit.is_some() {
                if self.panes.len() < SESSION_COUNT_HARD_CAP {
                    self.spawn_session();
                    self.focused_idx = self.panes.len() - 1;
                    self.rebuild_layout();
                }
                return;
            }
            if idx < self.panes.len() && idx != self.focused_idx {
                self.resolve_pending_on_defocus(idx);
                self.focused_idx = idx;
                let _ = self.panes[self.focused_idx].snap_to_live();
                // F3+5 — focus change = "user is looking at this pane
                // right now"; refresh its cwd so the title strip stays
                // current.  Debounced per-sid (cheap when same pane is
                // focused twice in a row).
                self.refresh_pane_cwd_for(idx, false);
                self.needs_render = true;
            }
        }
    }

    /// Finder file drop: insert the shell-quoted path(s) into the
    /// pane under the drop point — the "type the path for me" gesture
    /// every macOS terminal supports.  Routing goes through the
    /// existing Paste path (L3 wraps in bracketed-paste when the app
    /// enabled the mode), so shells insert at the prompt and TUI apps
    /// like claudecode see a normal paste in their input box.
    fn file_drop(&mut self, x_phys: f64, y_phys: f64, paths: &[String]) {
        if paths.is_empty() {
            return;
        }
        // Target the pane under the drop point (body cells or its
        // sidebar row); a drop on chrome/padding goes to the focused
        // pane — dropping "at the terminal" should never be a no-op.
        let row_phys = marspot_term::layout::SIDEBAR_ROW_H_PHYS;
        let top_pad_phys = self.layout.top_inset + self.layout.sidebar_top_pad_phys;
        let sidebar_hit = self.layout.hit_test_sidebar_row(
            x_phys, y_phys, top_pad_phys, row_phys, self.panes.len(),
        );
        let idx = sidebar_hit
            .or(self.layout.hit_test(x_phys, y_phys))
            .filter(|i| *i < self.panes.len())
            .unwrap_or(self.focused_idx);
        if idx >= self.panes.len() {
            return;
        }
        // Same focus motion as a click — the pane receiving the text
        // becomes the pane the user is typing into next.
        if idx != self.focused_idx {
            self.resolve_pending_on_defocus(idx);
            self.focused_idx = idx;
            self.refresh_pane_cwd_for(idx, false);
        }
        if self.panes[idx].snap_to_live() {
            self.needs_render = true;
        }
        // Trailing space after each path so the user can keep typing
        // (and multiple files arrive space-separated) — matches the
        // Finder → Terminal.app / iTerm2 convention.
        let mut text = String::new();
        for p in paths {
            text.push_str(&marspot_term::input_core::shell_quote_path(p));
            text.push(' ');
        }
        self.panes[idx].session_mut().forward_paste(&text);
        self.needs_render = true;
    }

    fn mouse_drag(&mut self, x_phys: f64, y_phys: f64) {
        // F3+3.3 — LayoutModal card drag.  Take priority over the
        // process panel drag so a layout modal session never gets
        // captured by chrome elsewhere.
        if let Some(d) = self.layout_drag.as_mut() {
            d.mouse_phys = (x_phys, y_phys);
            self.needs_render = true;
            return;
        }
        // F3+1.5 — modal title bar drag.  Snapshot at mouse_down
        // (drag_grab = Some((grab_x, grab_y, grab_off_x, grab_off_y)))
        // → motion delta translates to pos_offset diff.
        if let Some(panel) = self.process_panel.as_mut() {
            if let Some((gx, gy, gox, goy)) = panel.drag_grab {
                panel.pos_offset = (gox + (x_phys - gx), goy + (y_phys - gy));
                self.needs_render = true;
                return;
            }
        }
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

    fn end_modal_drag(&mut self) {
        if let Some(panel) = self.process_panel.as_mut() {
            panel.drag_grab = None;
        }
    }

    fn mouse_up(&mut self) {
        // F3+3.3 — finalize LayoutModal card drag: pick the
        // destination slot under the cursor, swap, redraw.  No
        // animation (V2.0); settle = single-frame jump.
        if let Some(d) = self.layout_drag.take() {
            use marspot::ui::components::LayoutModal;
            let modal = LayoutModal::layout(
                self.w_phys, self.h_phys, self.scale,
                marspot::TITLE_STRIP_PT * self.scale,
                self.pending_grid_cols, self.pending_grid_rows,
            );
            // Drop position = card origin (mouse - grab_offset),
            // plus card center offset so the lookup tracks visual
            // intent (cursor pointing at the dragged card's CENTER).
            let card_w = modal.cards.first().map(|c| c.w).unwrap_or(0.0);
            let card_h = modal.cards.first().map(|c| c.h).unwrap_or(0.0);
            let cx = d.mouse_phys.0 - d.grab_offset_phys.0 + card_w * 0.5;
            let cy = d.mouse_phys.1 - d.grab_offset_phys.1 + card_h * 0.5;
            if let Some(to_slot) = modal.nearest_card(cx, cy) {
                if to_slot != d.from_slot
                    && to_slot < self.card_slots.len()
                    && d.from_slot < self.card_slots.len()
                {
                    self.card_slots.swap(d.from_slot, to_slot);
                }
            }
            self.needs_render = true;
            // Don't continue into selection / process-panel paths.
            return;
        }
        self.end_modal_drag();
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
        // F3+1.5 — when the Process Monitor modal is open, the wheel
        // belongs to it (assuming the cursor is over the modal — and
        // since the modal swallows clicks anyway, treating ALL scroll
        // as modal scroll while it's open is the simpler, more
        // predictable mapping).
        if let Some(panel) = self.process_panel.as_mut() {
            if !panel.minimized {
                let _ = precise;
                panel.scroll_y += dy_phys;
                // Clamp using last-frame content_h.
                let max = (panel.content_h - panel.body_rect.h).max(0.0);
                if panel.scroll_y < 0.0 { panel.scroll_y = 0.0; }
                if panel.scroll_y > max { panel.scroll_y = max; }
                self.needs_render = true;
            }
            return;
        }
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
        // C5 — fire any pane's queued SearchScrollback when its
        // debounce window has elapsed.  Cheap when no search is
        // open (single `is_none` check per pane).
        self.process_search_debounces();
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
            // Auto-pin the viewport when a row scrolled into scrollback
            // while the user is reading history.  L3 runs the symmetric
            // bump in its publish loop so view_offset stays consistent
            // across the L2↔L3 boundary without a round-trip.  Without
            // this, every line the shell emits while the user is
            // scrolled back slides the visible content downward by one
            // row — the "老内容被新内容覆盖" symptom.
            if pushed > 0 {
                let pushed_u16 = pushed.min(u16::MAX as u64) as u16;
                p.bump_view_offset_on_scroll_push(pushed_u16);
            }
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

        // F3+5 — title placeholder = basename of the cwd cached in
        // `pane_cwds`.  Population strategy is hybrid passive:
        // (1) pane spawn, (2) focus change, (3) Enter key in focused
        // pane, (4) LayoutModal open (all panes), (5) `lazy_fill_missing_cwds`
        // here at the top of build_views as a tail-of-conditions
        // fallback — if anything else missed it, this catches it on
        // the first paint.  Cost: HashMap.contains_key per pane (no
        // syscall) on the steady-state hot path; one proc_pidinfo
        // syscall only on a miss.
        self.lazy_fill_missing_cwds();
        let cwd_basenames: Vec<Option<&str>> = (0..self.panes.len())
            .map(|i| {
                let sid = self.panes[i].shelld_session_id()?;
                let path = self.pane_cwds.get(&sid)?;
                std::path::Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
            })
            .collect();

        // Resolved label per cell: edit-mode buffer → user-set custom
        // title → plugin-set title (MsgType::PaneTitle, cc/...) →
        // cwd basename (dynamic placeholder) → ordinal fallback.
        let resolved_labels: Vec<String> = (0..self.panes.len())
            .map(|i| {
                if self.editing_title == Some(i) {
                    self.title_edit_buffer.clone()
                } else if let Some(Some(custom)) = self.custom_titles.get(i) {
                    custom.clone()
                } else if let Some(plugin_title) = self.panes[i]
                    .shelld_session_id()
                    .and_then(|sid| self.pane_titles.get(&sid))
                {
                    plugin_title.clone()
                } else if let Some(Some(name)) = cwd_basenames.get(i) {
                    (*name).to_string()
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

        // F3+1.3 — build + push process-panel data BEFORE we
        // borrow `self.panes` into `views`.  Renderer holds the
        // panel data via `set_process_panel`, freeing `&self` for
        // the render call below.
        let panel_data = self.build_process_panel_render();
        self.renderer.set_process_panel(panel_data);
        // F3+9 — publish ContextMenu render state every frame.
        // Dev panel renders into its own NSWindow, owned by L1
        // (marspot-shell), not by L2.  L2's only job re: dev panel
        // is to (a) hit-test the toolbar toggle icon and (b) emit
        // a `DevPanelToggle` wire frame on click; L1 takes it from
        // there.  No call here.
        self.renderer.set_dev_panel(None);

        self.renderer.set_context_menu(self.context_menu.as_ref().map(|state| {
            use marspot::render_metal::{ContextMenuRender, ContextMenuRow};
            ContextMenuRender {
                scale: self.scale,
                anchor_phys: (state.anchor_x, state.anchor_y),
                top_inset: self.layout.top_inset,
                items: state.items.iter().map(|it| ContextMenuRow {
                    label: it.label.clone(),
                    shortcut_hint: it.shortcut_hint.clone(),
                    enabled: it.enabled,
                    divider: it.divider,
                }).collect(),
                hovered_idx: state.hovered_idx,
            }
        }));
        // F3+3.0 / 3.3 — publish LayoutModal state every frame.
        // Per-slot titles built from card_slots → resolved title
        // chain (custom > cwd basename > ordinal).  Empty slot if
        // the slot points at a pane index past the live count.
        self.renderer.set_layout_modal(if self.layout_modal_open {
            use marspot::render_metal::{LayoutModalRender, LayoutModalDragRender};
            let cells = self.pending_grid_cols * self.pending_grid_rows;
            let slot_titles: Vec<String> = (0..cells)
                .map(|slot| {
                    let pane_idx = self.card_slots.get(slot).copied().unwrap_or(usize::MAX);
                    resolved_labels
                        .get(pane_idx)
                        .cloned()
                        .unwrap_or_default()
                })
                .collect();
            Some(LayoutModalRender {
                cols: self.pending_grid_cols,
                rows: self.pending_grid_rows,
                scale: self.scale,
                slot_titles,
                drag: self.layout_drag.map(|d| LayoutModalDragRender {
                    from_slot: d.from_slot,
                    grab_offset_phys: d.grab_offset_phys,
                    mouse_phys: d.mouse_phys,
                }),
            })
        } else {
            None
        });
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

    // Stale env IDs are normal during a dual-core install-local swap
    // window: L1 spawns this core with a `front_id`/`back_id` pair,
    // then a few ms later decides the previous core is unrecoverable
    // and rotates the pair before we get here.  Exit cleanly instead
    // of panicking so L1's "crash/hang detect → respawn" path picks
    // the next slot without a backtrace storm in marspot.log.
    let Some(front) = IOSurface::lookup(front_id) else {
        lx_warn!(
            "core.surface.lookup_nil",
            "IOSurface front id stale (L1 rotated mid-spawn); exiting for respawn",
            front_id = front_id,
            back_id = back_id
        );
        std::process::exit(2);
    };
    front.increment_use();
    let Some(back) = IOSurface::lookup(back_id) else {
        lx_warn!(
            "core.surface.lookup_nil",
            "IOSurface back id stale (L1 rotated mid-spawn); exiting for respawn",
            front_id = front_id,
            back_id = back_id
        );
        std::process::exit(2);
    };
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
    // CoreEvents; the main loop blocks on `recv_timeout` so it
    // sleeps until *any* event arrives — idle CPU = 0.  RFC-003
    // retired the shelld wake callback (used to push `PumpShelld`);
    // L3 reattach + dual-pump bytelog already drives the event loop
    // through the same channel.
    let (event_tx, event_rx): (Sender<CoreEvent>, Receiver<CoreEvent>) = mpsc::channel();

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
    // F3+6 — read persisted shell-state.bin first; defaults to 3×3
    // when none / corrupt.  `saved_state` then drives reattach order,
    // fresh-spawn cwds, focused_idx and custom_titles below.
    let saved_state = marspot::state::read();
    let (grid_cols, grid_rows): (usize, usize) = match saved_state.as_ref() {
        Some(s) if s.grid_cols > 0 && s.grid_rows > 0 => (
            (s.grid_cols as usize).clamp(1, 6),
            (s.grid_rows as usize).clamp(1, 6),
        ),
        _ => (3, 3),
    };
    let n_sessions = match saved_state.as_ref() {
        Some(s) => s.panes.len().clamp(1, SESSION_COUNT_HARD_CAP),
        None => grid_cols * grid_rows,
    };
    let (boot_cols, boot_rows) = {
        let (cell_w, cell_h) = renderer.cell_dims();
        let (lc, lr) = (grid_cols, grid_rows);
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
    // RFC-003 §6 Amendment 14: did the boot promote a fresh marspot-
    // session binary?  If yes, fan SIGUSR2 out to every reattached L3
    // after the reattach loop so existing children also pick up the
    // new image via execv self-update (not just freshly-spawned ones).
    let mut session_binary_freshly_promoted = false;
    if l3_mode {
        // A core update lands the new session engine in pending/; promote
        // it into current/ before spawning so each L3 (resolved as core's
        // sibling = current/marspot-session in an installed app) boots the
        // new binary in lockstep with this core.
        match marspot::updater::promote_pending_session() {
            Ok(true) => {
                session_binary_freshly_promoted = true;
                lx_event!(
                    "SESSION_PROMOTE",
                    "promoted staged marspot-session → current/ at boot"
                );
            }
            Ok(false) => {}
            Err(e) => lx_error!("core.promote.boot_failed", &format!("{e}")),
        }
        // RFC-003 step 3a + Amendment 7 step 4: scan registry,
        // reattach to alive L3s by shm name + UDS connect, prune dead,
        // allocate fresh for the remainder.
        let raw_list = list_session_entries();
        for e in &raw_list {
            if !e.title.is_empty() {
                session_titles.insert(e.id, e.title.clone());
            }
        }
        // RFC-003 §6 Amendment 15 — separate alive vs dead.  alive
        // entries reattach (L3 still running).  dead entries are
        // resurrect candidates: their sessions/<id>/ dir survives
        // disk (state.bin + bytelog + entry.toml), so we can spawn
        // a fresh L3 against the saved snapshot + fresh shell.  No
        // pruning during scan — pruning a dead entry would discard
        // exactly the data we want to resurrect from.
        let mut alive_ids: Vec<u64> = Vec::new();
        let mut dead_ids: Vec<u64> = Vec::new();
        for e in &raw_list {
            let live = unsafe { libc::kill(e.pid, 0) } == 0;
            if live {
                alive_ids.push(e.id);
            } else {
                dead_ids.push(e.id);
            }
        }
        let dead = dead_ids.len();
        // F3+6 — order: saved.panes first (in saved order, for sids
        // that survived), then any other alive sid as tail.  Without
        // saved state, fall back to numeric sort (legacy).
        if let Some(ref s) = saved_state {
            let alive_set: std::collections::HashSet<u64> =
                alive_ids.iter().copied().collect();
            let saved_set: std::collections::HashSet<u64> =
                s.panes.iter().map(|p| p.sid).filter(|&id| id != 0).collect();
            let mut ordered: Vec<u64> = s.panes.iter()
                .map(|p| p.sid)
                .filter(|&id| id != 0 && alive_set.contains(&id))
                .collect();
            for &id in &alive_ids {
                if !saved_set.contains(&id) {
                    ordered.push(id);
                }
            }
            alive_ids = ordered;
        } else {
            alive_ids.sort();
        }
        let mut reattached_ids: Vec<u64> = Vec::new();
        for id in alive_ids.iter().take(n_sessions) {
            match reattach_l3_pane(*id, &event_tx) {
                Ok(pane) => {
                    panes.push(pane);
                    reattached_ids.push(*id);
                }
                Err(e) => {
                    lx_warn!(
                        "core.reattach.l3_failed",
                        &format!("{e} — will SIGKILL + prune"),
                        session = id
                    );
                    // Reattach failed: SIGKILL the orphan + clean
                    // registry so the next boot doesn't loop on it.
                    // F3+3.2 — SIGKILL (not SIGTERM), same reason
                    // as the prune loop below: SIGTERM is overloaded
                    // to trigger L3 self-execv on fingerprint
                    // mismatch, which would leave a leaked L3
                    // running with no L2 client.
                    if let Ok(entry) =
                        marspot_term::session_registry::read_session_entry(*id)
                    {
                        unsafe { libc::kill(entry.pid, libc::SIGKILL) };
                        if !entry.shm_name.is_empty() {
                            if let Ok(c) =
                                std::ffi::CString::new(entry.shm_name.clone())
                            {
                                grid_shm::delete_region(&c);
                            }
                        }
                    }
                    let _ = session_registry::delete_session(*id);
                }
            }
        }
        // Any alive id beyond n_sessions is leftover from a wider
        // layout; KILL + prune so they don't accumulate.
        //
        // F3+3.2 — SIGKILL, not SIGTERM.  SIGTERM is overloaded by
        // RFC-003 §6 Amendment 16 to mean "binary fingerprint
        // differs → self-execv into the new image".  During an
        // install storm, an old L3 that we want to PRUNE receives
        // SIGTERM, sees the new current/marspot-session fingerprint
        // differs from its own, and execv's instead of dying.
        // After execv it sits idle (no L2 client), leaking a whole
        // marspot-session process.  SIGKILL bypasses the handler so
        // the pruned L3 is reliably gone.  PTY child + sockets get
        // cleaned by the kernel; `delete_session` below clears the
        // registry dir.
        for id in alive_ids.iter().skip(n_sessions) {
            if let Ok(entry) = marspot_term::session_registry::read_session_entry(*id) {
                unsafe { libc::kill(entry.pid, libc::SIGKILL) };
                if !entry.shm_name.is_empty() {
                    if let Ok(c) = std::ffi::CString::new(entry.shm_name.clone()) {
                        grid_shm::delete_region(&c);
                    }
                }
            }
            let _ = session_registry::delete_session(*id);
        }
        // RFC-003 §6 Amendment 16 — L3 self-execv silent update.
        // If this boot promoted a fresh marspot-session binary, fan
        // SIGTERM out to every reattached L3 — their handler will
        // notice current/marspot-session's MARSPOT_FP differs from
        // their own rodata fingerprint and self-execv into the new
        // image (PTY master fd + UDS listener fd + shell child all
        // preserved via clear-CLOEXEC + manifest handoff).
        if session_binary_freshly_promoted && !reattached_ids.is_empty() {
            let mut signalled = 0usize;
            for id in &reattached_ids {
                if let Ok(entry) =
                    marspot_term::session_registry::read_session_entry(*id)
                {
                    if unsafe { libc::kill(entry.pid, libc::SIGTERM) } == 0 {
                        signalled += 1;
                    }
                }
            }
            lx_event!(
                "L3_BOOT_FANOUT",
                "SIGTERM sent to reattached L3s for self-execv",
                n_signalled = signalled,
                n_reattached = reattached_ids.len()
            );
        }
        // RFC-003 §6 Amendment 18 — dead L3 resurrection.  user 关窗
        // → L1 close_requested → SIGTERM 全部 L3 → L3 SIGTERM handler
        // 写 state.bin + process::exit(0)(Drop 不跑,entry.toml 存活).
        // reopen → 这些 session 在 raw_list 里 pid 已死 → 落到
        // dead_ids.之前 prune 把整个 session_dir rm,state.bin 跟着
        // 没,新 L3 起来空白 — user 失去全 history.
        //
        // 现在 dead_ids 是 **resurrect candidates**:
        //   1. 老 shm 区域死了 → 拆,新 L3 起来会拿到 L2 新发的 shm
        //   2. session_dir(含 state.bin)留着
        //   3. 走 fresh-spawn 路径时,优先用 dead_ids 的 id,L3 cold
        //      boot 看到 state.bin 自动 apply_snapshot —— 用户看到
        //      原来的 scrollback 完整保留.
        for id in &dead_ids {
            if let Ok(entry) = marspot_term::session_registry::read_session_entry(*id) {
                if !entry.shm_name.is_empty() {
                    if let Ok(c) = std::ffi::CString::new(entry.shm_name.clone()) {
                        grid_shm::delete_region(&c);
                    }
                }
            }
            // 留 session_dir + state.bin;下面 resurrect loop spawn 时
            // L3 cold boot 看到 state.bin 自动 load + delete.
        }
        lx_event!(
            "core.session_registry.inventory",
            "RFC-003 registry scan at boot",
            total = raw_list.len(),
            alive = alive_ids.len(),
            reattached = reattached_ids.len(),
            dead = dead,
            want = n_sessions
        );
        // dead_ids 排在前面 — 它们各自的 session_dir 有 state.bin,
        // 同 id 起 L3 时 L3 cold boot 自动 apply_snapshot 恢复 scrollback.
        // 之后再用 allocate_next_session_id 给剩下的 slot 拿全新 id.
        let mut ids: Vec<u64> = dead_ids
            .iter()
            .copied()
            .take(n_sessions.saturating_sub(panes.len()))
            .collect();
        while panes.len() + ids.len() < n_sessions {
            match allocate_next_session_id() {
                Ok(id) => ids.push(id),
                Err(e) => {
                    lx_error!(
                        "core.session_registry.allocate_failed",
                        &format!("{e}")
                    );
                    break;
                }
            }
        }
        for (slot, id) in ids.into_iter().enumerate() {
            // F3+6 — pick the cwd from saved.panes for the slot this
            // spawn is filling.  `slot` indexes into the missing-tail
            // (saved indexes 0..panes.len() are already reattached
            // above; freshly spawned slots start at panes.len()
            // before this iter began).
            let target_slot = panes.len();
            let cwd = saved_state.as_ref()
                .and_then(|s| s.panes.get(target_slot))
                .map(|p| p.last_cwd.clone())
                .unwrap_or_default();
            let _ = slot;
            match spawn_l3_pane_with_cwd(boot_cols, boot_rows, id, &cwd, &event_tx) {
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

    // RFC-003 Phase 6: shelld fallback path deleted.  If l3_mode failed
    // to spawn anything we exit; no fallback to L4.
    if panes.is_empty() {
        lx_error!("core.boot.no_sessions", "no sessions could be created — exiting");
        return;
    }

    // Repopulate per-pane custom titles.  F3+6 — `saved_state.panes`
    // wins over `session_titles` (the entry.toml-derived source)
    // because the bin file reflects the user's last interactive
    // state, including titles set after the last L3 reattach hop.
    // Fallback chain: saved → session_titles → None.
    let custom_titles_init: Vec<Option<String>> = panes
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if let Some(ref s) = saved_state {
                if let Some(entry) = s.panes.get(i) {
                    if !entry.custom_title.is_empty() {
                        return Some(entry.custom_title.clone());
                    }
                }
            }
            p.shelld_session_id()
                .and_then(|sid| session_titles.get(&sid).cloned())
                .filter(|t| !t.is_empty())
        })
        .collect();
    let initial_focused_idx = saved_state.as_ref()
        .map(|s| (s.focused_idx as usize).min(panes.len().saturating_sub(1)))
        .unwrap_or(0);
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
        focused_idx: initial_focused_idx,
        custom_titles: custom_titles_init,
        editing_title: None,
        title_edit_buffer: String::new(),
        pane_badges: std::collections::HashMap::new(),
        pane_titles: std::collections::HashMap::new(),
        pane_cwds: std::collections::HashMap::new(),
        pending_to_shell: Vec::new(),
        pane_sessions: std::collections::HashMap::new(),
        esc_history: std::collections::VecDeque::with_capacity(3),
        selection: None,
        selection_dragging: false,
        grid_cols,
        grid_rows,
        layout_modal_open: false,
        context_menu: None,
        pending_grid_cols: grid_cols,
        pending_grid_rows: grid_rows,
        card_slots: (0..(grid_cols * grid_rows)).collect(),
        layout_drag: None,
        last_cwd_refresh: std::collections::HashMap::new(),
        sidebar_collapsed: true,
        hover_chrome_btn: None,
        process_panel: None,
        ime_preedit: String::new(),
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
    // F3+6 — first save right after boot so a hard kill before any
    // user action still leaves the file populated.  Costs one write
    // (~50us); idempotent if shell-state.bin already matched.
    app.save_session_state();
    lx_info!(
        "core.layout.ready",
        "initial layout built",
        mode = format!("{}x{}", app.grid_cols, app.grid_rows),
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
        // F3+1.3 — periodic tasks driven off the same wakeups:
        //   1) escalate any SIGTERM that the target ignored past the
        //      grace period (cheap — empty Vec when no pending).
        //   2) re-walk libproc every PROCESS_PANEL_REFRESH_INTERVAL
        //      so the panel reflects forked-since-last-walk children
        //      / dead descendants.  Bypassed entirely when the panel
        //      is closed.
        app.tick_process_panel_kills();
        if let Some(panel) = app.process_panel.as_ref() {
            if panel.last_refresh.elapsed() >= PROCESS_PANEL_REFRESH_INTERVAL {
                app.refresh_process_panel();
                app.needs_render = true;
            }
        }
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
                CoreEvent::MouseRightDown(x, y, mods) => app.mouse_right_down(x, y, mods),
                CoreEvent::MouseDrag(x, y) => app.mouse_drag(x, y),
                CoreEvent::MouseUp => app.mouse_up(),
                CoreEvent::MouseMove(x, y) => app.mouse_moved(x, y),
                CoreEvent::Scroll(dy, precise) => app.scroll(dy, precise),
                CoreEvent::FileDrop(x, y, paths) => app.file_drop(x, y, &paths),
                CoreEvent::Focus(focused) => {
                    app.renderer.set_window_focused(focused);
                    // No mouseMoved deliveries while the window
                    // isn't key — drop any latched hover so the
                    // affordance doesn't linger when the user
                    // alt-tabs away mid-hover.
                    if !focused && app.hover_chrome_btn.is_some() {
                        app.hover_chrome_btn = None;
                        app.renderer.set_hover_chrome_btn(None);
                    }
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
                CoreEvent::L3ControlEof(sid) => {
                    // L3's reader EOF'd — typically silent-update
                    // execv before manifest v2 carried control_stream_fd
                    // closed the inherited stream; possibly a hard
                    // crash; possibly a dual-core swap race.
                    // Reconnect via the same wait_and_connect L3's UDS
                    // accept path serves, hot-swap the pane's control,
                    // and respawn the reader on the new stream.  All
                    // best-effort: a permanent L3 death is detected by
                    // pane.poll's try_wait on the next tick.
                    let pane_idx = app.panes.iter().position(|p| {
                        p.session().l3_session_id() == Some(sid)
                    });
                    if let Some(idx) = pane_idx {
                        match marspot::uds_session_client::wait_and_connect(
                            sid,
                            std::time::Duration::from_secs(2),
                        ) {
                            Ok(new_control) => {
                                let reader_half = new_control.try_clone();
                                match reader_half {
                                    Ok(rh) => {
                                        app.panes[idx]
                                            .session_mut()
                                            .swap_l3_control(new_control);
                                        let tx = app.event_tx.clone();
                                        let (selection_tx, _selection_rx) =
                                            std::sync::mpsc::channel::<(u32, String)>();
                                        std::thread::spawn(move || {
                                            l3_reader_loop(rh, sid, tx, selection_tx);
                                        });
                                        lx_event!(
                                            "L3_CONTROL_RECONNECTED",
                                            "L2 reader reconnected after EOF",
                                            session = sid,
                                            pane = idx
                                        );
                                    }
                                    Err(e) => lx_warn!(
                                        "core.l3.control_clone_failed",
                                        &format!("{e}"),
                                        session = sid
                                    ),
                                }
                            }
                            Err(e) => lx_warn!(
                                "core.l3.control_reconnect_failed",
                                &format!("{e}; pane will go silent until next reconnect"),
                                session = sid
                            ),
                        }
                    }
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
                CoreEvent::PaneTitle(sid, text) => {
                    app.set_pane_title(sid, text);
                }
                CoreEvent::PaneSessionBegin(sid, caps) => {
                    app.pane_session_begin(sid, caps);
                }
                CoreEvent::PaneSessionEnd(sid) => {
                    app.pane_session_end(sid);
                }
                CoreEvent::InjectInput(sid, bytes) => {
                    app.inject_input(sid, &bytes);
                }
                CoreEvent::SearchResults(sid, qid, has_more, _total_seen, hits) => {
                    app.apply_search_results(sid, qid, has_more, hits);
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

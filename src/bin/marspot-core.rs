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
    decode_surface_attach_window, decode_window_closed, decode_window_focus,
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

/// Does this raw scanned-IP text look like a bare, unbracketed IPv6
/// address that needs `[...]` wrapping before it can appear in an
/// `http://` URL?  Bracketed input (`[::1]:8080`) is already URL-safe
/// and returns false.  Bare IPv4 (`1.2.3.4`, `1.2.3.4:8080`) has at
/// most one `:` and returns false.  Bare IPv6 (`::1`, `2001:db8::1`)
/// has `::` or `≥3` colons and returns true.
fn ip_text_is_bare_ipv6(text: &str) -> bool {
    if text.starts_with('[') || !text.contains(':') {
        return false;
    }
    if text.contains("::") {
        return true;
    }
    text.chars().filter(|&c| c == ':').count() >= 3
}

#[cfg(test)]
mod ip_bracket_tests {
    use super::ip_text_is_bare_ipv6;
    #[test]
    fn bracketed_and_ipv4_do_not_need_wrapping() {
        assert!(!ip_text_is_bare_ipv6("[::1]:8080"));
        assert!(!ip_text_is_bare_ipv6("47.96.114.231"));
        assert!(!ip_text_is_bare_ipv6("192.168.1.1:8080"));
        assert!(!ip_text_is_bare_ipv6("10.0.0.5/status"));
    }
    #[test]
    fn bare_ipv6_needs_wrapping() {
        assert!(ip_text_is_bare_ipv6("::1"));
        assert!(ip_text_is_bare_ipv6("2001:db8::1"));
        assert!(ip_text_is_bare_ipv6(
            "2001:0db8:85a3:0000:0000:8a2e:0370:7334"
        ));
    }
}

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

/// `install-local.sh`'s `sup_log` shells out to
/// `marspot-core --log-event <tag> <detail>` so supervisor-side
/// decisions land in the same marspot.log as everything else.
///
/// That mechanism was added after an install-local bug took down nine
/// live sessions with no log line explaining why — and it had never
/// once worked.  Nothing parsed argv, so every call fell straight into
/// the boot path and panicked in `env_required` on the missing surface
/// id: three dead processes per install (the script tries three binary
/// paths) and not one line written.
///
/// Hence this is the first thing `main` does, before any env read.
fn parse_log_event(args: &[String]) -> Option<(&str, String)> {
    if args.first().map(String::as_str) != Some("--log-event") {
        return None;
    }
    // A call with no tag is still a call — log it under a placeholder
    // rather than falling through to the boot path and panicking,
    // which is the failure this whole function exists to end.
    let tag = args.get(1).map(String::as_str).unwrap_or("untagged");
    Some((tag, args.get(2..).unwrap_or(&[]).join(" ")))
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
    /// cc — Claude usage modal toggle.
    CcUsage,
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
        Some(ChromeBtn::CcUsage) => Some(4),
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
    /// Cursor inside `title_bar_rect` — reveals the ×/−/+ glyphs.
    title_bar_hovered: bool,
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

/// Queue one frame for L1, logging if it had to be dropped.
///
/// Six call sites used to inline this `if !send { lx_error!(…) }` block
/// verbatim, which is three past the point where CLAUDE.md says to
/// extract.  `which` discriminates them in the log — one event name to
/// grep for, a field to tell them apart, rather than six near-identical
/// names.
fn send_to_shell(
    w: &marspot_term::frame_writer::FrameWriter,
    frame: Frame,
    which: &str,
) {
    if !w.send(frame) {
        lx_error!(
            "core.shell_write_dropped",
            "L1 not draining the control socket; frame dropped",
            which = which,
            backlog = w.backlog()
        );
    }
}

/// How many times a background reconnect retries before giving the
/// pane up.  Cheap now that it is off the main loop.
const L3_RECONNECT_ATTEMPTS: u32 = 3;

/// Consecutive failures after which cwd resolution for a session is
/// abandoned until an explicit trigger.  Small: the fetch either works
/// once the shell has registered its pid, or it never will.
const CWD_MAX_FAILURES: u32 = 5;

/// Shortest L2 main-loop iteration worth reporting as a stall.
///
/// Tighter than L3's 150 ms because this loop owes a frame: anything
/// past ~80 ms has already cost the user a visible hitch, and a stall
/// here freezes every pane rather than one.
const L2_LOOP_STALL_THRESHOLD: Duration = Duration::from_millis(80);

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
    /// Menu opened on a pane-badge prefix; the items came from an L1
    /// plugin via `PaneBadgeMenu` and their tags are plugin-opaque —
    /// item dispatch sends `PaneBadgeMenuAction` back to L1 instead
    /// of mapping through `ContextMenuAction`.
    PaneBadge(u64),
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
/// or right) on an underlined URL / file-path / email span.  Returns
/// Copy + Open entries (Copy on top — primary intent in a terminal
/// context is "grab this URL/path/address", not "launch the app").
/// Email routes through `/usr/bin/open mailto:…` so the system's
/// default mail client picks it up.
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
    // UUID is copy-only (nothing sensible to `open(1)`); every other
    // kind surfaces both Copy and Open.
    if link.kind == LinkKind::Uuid {
        return vec![MenuItem::entry(
            "Copy UUID",
            ContextMenuAction::CopyLink.tag(),
        )];
    }
    let (open_label, copy_label) = match link.kind {
        LinkKind::Url => ("Open URL", "Copy URL"),
        LinkKind::File => ("Open file", "Copy path"),
        LinkKind::Email => ("Send email", "Copy email"),
        LinkKind::Ip => ("Open in browser", "Copy IP"),
        LinkKind::Uuid => unreachable!("handled above"),
    };
    vec![
        MenuItem::entry(copy_label, ContextMenuAction::CopyLink.tag()),
        MenuItem::entry(open_label, ContextMenuAction::OpenLink.tag()),
    ]
}

/// RFC-004 B.5 — boot-assembly integration tests.  Each test runs in
/// its own process (nextest) against a sandbox `MARSPOT_STATE_DIR`;
/// resurrection/spawn paths exercise the REAL `marspot-session`
/// binary (resolved as a sibling of the workspace target dir, or a
/// deliberately-broken path for the failure tests).  These pin the
/// RFC-004 invariants: slot order preserved, slots never compact,
/// sids stable, failures yield vacant placeholders, orphans adopted.
#[cfg(test)]
mod sup_log_tests {
    use super::parse_log_event;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    /// The shape `install-local.sh` actually sends.
    #[test]
    fn log_event_is_recognised_and_keeps_the_whole_detail() {
        let args = argv(&[
            "--log-event",
            "UPDATE_SWAP",
            "single-core",
            "swap",
            "complete",
        ]);
        assert_eq!(
            parse_log_event(&args),
            Some(("UPDATE_SWAP", "single-core swap complete".to_string())),
            "detail is several argv words and must be rejoined"
        );
    }

    /// Degenerate calls must still be handled here rather than falling
    /// through to the boot path, which panics on the missing surface
    /// env — that fall-through is the bug this replaced.
    #[test]
    fn malformed_log_event_calls_never_fall_through() {
        let bare = argv(&["--log-event"]);
        assert_eq!(parse_log_event(&bare), Some(("untagged", String::new())));
        let tag_only = argv(&["--log-event", "TAG"]);
        assert_eq!(parse_log_event(&tag_only), Some(("TAG", String::new())));
    }

    /// A normal boot must NOT be mistaken for a log-event call.
    #[test]
    fn boot_argv_is_not_a_log_event() {
        let empty = argv(&[]);
        assert_eq!(parse_log_event(&empty), None);
        let status = argv(&["--status"]);
        assert_eq!(parse_log_event(&status), None);
        let wrong_order = argv(&["TAG", "--log-event"]);
        assert_eq!(parse_log_event(&wrong_order), None);
    }
}

#[cfg(test)]
mod window_state_tests {
    use super::*;

    fn win(id: u32, panes: Vec<Pane>) -> WindowState {
        WindowState::new(id, panes, 0, 1, 1, 800.0, 600.0, 2.0)
    }

    /// RFC-005's load-bearing claim: a pane carries everything that is
    /// "its own" inside the `Pane` value, so moving it between windows
    /// is a `Vec` move and nothing else.  Anything that regresses to a
    /// window-side parallel array keyed by pane index breaks here.
    #[test]
    fn moving_a_pane_between_windows_carries_its_state() {
        let mut a = win(1, vec![Pane::new_vacant(7, 80, 24)]);
        let mut b = win(2, Vec::new());
        a.panes[0].custom_title = Some("keep me".into());
        a.panes[0].set_view_offset(42);

        let moved = a.panes.remove(0);
        b.panes.push(moved);

        assert!(a.panes.is_empty(), "source window must give the pane up");
        assert_eq!(b.panes[0].shelld_session_id(), Some(7));
        assert_eq!(b.panes[0].custom_title.as_deref(), Some("keep me"));
        assert_eq!(
            b.panes[0].view_offset(),
            42,
            "scroll position is pane state, not window state"
        );
    }

    /// `focused_pane_mut` must track `focused_idx`, not slot 0 — the
    /// helper exists to dodge a borrow conflict and an off-by-one here
    /// would silently route keystrokes to the wrong pane.
    #[test]
    fn focused_pane_follows_focused_idx() {
        let mut w = win(
            1,
            vec![
                Pane::new_vacant(10, 80, 24),
                Pane::new_vacant(11, 80, 24),
                Pane::new_vacant(12, 80, 24),
            ],
        );
        w.focused_idx = 2;
        assert_eq!(w.focused_pane_mut().shelld_session_id(), Some(12));
        assert_eq!(w.try_focused_pane().unwrap().shelld_session_id(), Some(12));
        w.focused_idx = 9;
        assert!(
            w.try_focused_pane().is_none(),
            "out-of-range focus must be visible, not silently slot 0"
        );
    }

    /// Each window owns its own renderer state.  The flag that says
    /// "this window still owes a full clear" must not be dischargeable
    /// by another window's resize — that would leave stale pixels in
    /// whichever window did not repaint.
    #[test]
    fn each_window_owns_its_clear_flag() {
        let mut a = win(1, Vec::new());
        let mut b = win(2, Vec::new());
        // A fresh window owes a clear; consume both so they start level.
        assert!(a.render.take_clear_required());
        assert!(b.render.take_clear_required());
        assert!(!a.render.take_clear_required());

        a.render.mark_bg_clear_required();
        assert!(
            !b.render.take_clear_required(),
            "marking window A must not discharge or set window B"
        );
        assert!(a.render.take_clear_required());
    }

    /// Point every `marspot::paths` lookup at a per-process temp dir.
    ///
    /// Idempotent and cheap, so helpers call it unconditionally rather
    /// than leaving it to each test to remember.
    fn sandbox_state_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("marspot-core-tests-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test state dir");
        // SAFETY: nextest runs one test per process; `cargo test`
        // shares one, but every test here wants the same sandbox, so
        // racing writers all write the same value.
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", &dir) };
        dir
    }

    /// The guard that would have caught the 2026-07-27 incident: no
    /// test in this module may run against the real state dir.
    #[test]
    fn tests_never_touch_the_real_state_dir() {
        sandbox_state_dir();
        let resolved = marspot::state::state_file_path();
        assert!(
            !resolved.starts_with(dirs_home().join("Library/Application Support/marspot")),
            "state path escaped the sandbox: {}",
            resolved.display()
        );
        assert!(
            resolved.starts_with(std::env::temp_dir()),
            "state path is not in a temp sandbox: {}",
            resolved.display()
        );
    }

    fn dirs_home() -> std::path::PathBuf {
        std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
    }

    /// A `CoreApp` with `windows` and nothing else going on.  Real
    /// `MetalRenderer` because the paths under test (`rebuild_layout`,
    /// texture creation) go through it.
    ///
    /// **Redirects the state dir first, always.**  A `CoreApp` reaches
    /// `save_session_state`, and `marspot::paths` resolves to the real
    /// `~/Library/Application Support/marspot` whenever
    /// `MARSPOT_STATE_DIR` is unset — so a test that touches any path
    /// which happens to save (`adopt_restored_panes`, `adopt_window`,
    /// `close_session`, `commit_title_edit`, …) silently overwrites the
    /// user's live layout with its own fixture.  That is not
    /// hypothetical: on 2026-07-27 it replaced a 4×4 / 16-pane record
    /// with this module's two 1×1 windows, and the next core boot came
    /// up with one 1×1 window and every session re-adopted as an
    /// orphan.  The sessions survived (their dirs are the real store);
    /// the layout did not.
    fn app_with(windows: Vec<WindowState>) -> CoreApp {
        sandbox_state_dir();
        let (event_tx, _rx) = mpsc::channel();
        CoreApp {
            renderer: MetalRenderer::new_headless().expect("headless renderer"),
            pane_badges: std::collections::HashMap::new(),
            pane_titles: std::collections::HashMap::new(),
            pane_cwds: std::collections::HashMap::new(),
            pending_to_shell: Vec::new(),
            pane_sessions: std::collections::HashMap::new(),
            esc_history: std::collections::HashMap::new(),
            last_cwd_refresh: std::collections::HashMap::new(),
            reconnecting: std::collections::HashSet::new(),
            cwd_unresolvable: std::collections::HashMap::new(),
            all_exited: false,
            saw_window_aware_attach: false,
            l3_mode: false,
            event_tx,
            drag_window: None,
            saved_windows: std::collections::VecDeque::new(),
            windows,
            key_window: 0,
        }
    }

    /// RFC-005 step 4d — the peer invariant on the paint side.  Every
    /// window owns its own IOSurface pair and its own buffer flip; an
    /// attach for one window must leave the others' targets exactly
    /// where they were.
    ///
    /// The regression this pins is not hypothetical: the pair used to
    /// be three locals in `main()`, so the second window's attach
    /// silently repointed the first window at a surface nobody painted
    /// into, and that window froze on its last frame.
    #[test]
    fn each_window_owns_its_paint_target() {
        let mut app = app_with(vec![win(1, Vec::new()), win(2, Vec::new())]);
        let mk = || {
            let f = IOSurface::create(64, 64).expect("front");
            let b = IOSurface::create(64, 64).expect("back");
            (f.id(), b.id(), f, b)
        };
        // Hold the originals alive for the duration of the test — the
        // ids must stay valid for `lookup`.
        let (a_f, a_b, _ka_f, _ka_b) = mk();
        let (b_f, b_b, _kb_f, _kb_b) = mk();
        let (c_f, c_b, _kc_f, _kc_b) = mk();

        assert_eq!(
            app.attach_window_surfaces(1, a_f, a_b, 800.0, 600.0, 2.0),
            Some(0)
        );
        assert_eq!(
            app.attach_window_surfaces(2, b_f, b_b, 400.0, 300.0, 2.0),
            Some(1)
        );
        let pair_of = |app: &CoreApp, wi: usize| {
            let s = app.windows[wi].surfaces.as_ref().expect("attached");
            (s.pair[0].id(), s.pair[1].id(), s.writing_idx)
        };
        assert_eq!(pair_of(&app, 0), (a_f, a_b, 0));
        assert_eq!(pair_of(&app, 1), (b_f, b_b, 0));

        // Window 2 resizes: fresh pair, and window 1 must not notice.
        assert_eq!(
            app.attach_window_surfaces(2, c_f, c_b, 500.0, 500.0, 2.0),
            Some(1)
        );
        assert_eq!(pair_of(&app, 0), (a_f, a_b, 0), "peer's pair moved");
        assert_eq!(pair_of(&app, 1), (c_f, c_b, 0));
        assert_eq!(app.windows[0].w_phys, 800.0, "peer's dims moved");
        assert_eq!(app.windows[1].w_phys, 500.0);

        // …and neither does its buffer flip.
        app.windows[1].surfaces.as_mut().unwrap().flip();
        assert_eq!(pair_of(&app, 0).2, 0, "peer's writing half flipped");
        assert_eq!(pair_of(&app, 1).2, 1);

        // An attach naming a window the core doesn't have is a normal
        // race (the window closed first), not a reason to touch anyone.
        assert_eq!(app.attach_window_surfaces(99, a_f, a_b, 10.0, 10.0, 1.0), None);
        assert_eq!(pair_of(&app, 0), (a_f, a_b, 0));
    }

    /// marspot quits when every shell is gone — "every", across all
    /// windows.  A live pane in any window keeps the app up, however
    /// long its window has been out of focus.
    #[test]
    fn the_app_exits_only_when_every_window_is_dead() {
        // Vacant = born exited; pending = a spawn in flight, i.e.
        // alive — but only for `PENDING_MAX` (30 s), so the pane is
        // built *after* the renderer (whose construction can take
        // minutes on a loaded machine) and pumped immediately.
        let dead = || Pane::new_vacant(1, 80, 24);
        let alive = || Pane::new_pending(2, 80, 24);

        let mut app = app_with(vec![win(1, Vec::new()), win(2, Vec::new())]);
        app.windows[0].panes.push(dead());
        app.windows[1].panes.push(alive());
        app.pump_all();
        assert!(
            !app.all_exited,
            "a live pane in a non-key window must keep marspot alive"
        );

        app.windows[1].panes[0] = dead();
        app.all_exited = false;
        app.pump_all();
        assert!(app.all_exited, "every pane of every window has exited");
    }

    /// RFC-005 step 6b — a window being restored is on screen with its
    /// own saved shape before any reattach has been attempted.  That is
    /// what makes the assembly off-loop worth doing: the windows
    /// already up never wait on this one's I/O.
    #[test]
    fn a_restored_window_shows_its_saved_shape_before_its_panes_arrive() {
        // Point the assembly worker at a binary that cannot start, so
        // it fails fast instead of spawning real L3s behind the test.
        // SAFETY: nextest runs one test per process.
        unsafe {
            std::env::set_var("MARSPOT_SESSION_BIN", "/nonexistent/marspot-session");
        }
        let record = marspot::state::SavedWindowLayout {
            grid_cols: 2,
            grid_rows: 2,
            focused_idx: 2,
            panes: vec![
                marspot::state::SavedPane { sid: 11, ..Default::default() },
                marspot::state::SavedPane { sid: 12, ..Default::default() },
                marspot::state::SavedPane { sid: 13, ..Default::default() },
            ],
        };
        let mut app = app_with(vec![win(1, vec![Pane::new_vacant(1, 80, 24)])]);
        app.saved_windows.push_back(record);

        app.adopt_window(2, 800.0, 600.0, 2.0);

        assert_eq!(app.windows.len(), 2, "the window exists immediately");
        let w = &app.windows[1];
        assert_eq!(w.window_id, 2);
        assert_eq!((w.grid_cols, w.grid_rows), (2, 2), "its own saved grid");
        assert_eq!(w.panes.len(), 3, "one placeholder per saved slot");
        assert_eq!(w.focused_idx, 2, "saved focus honoured");
        assert!(
            w.panes.iter().all(|p| !p.is_exited()),
            "placeholders are 'starting…', not dead slots"
        );
        assert!(
            app.saved_windows.is_empty(),
            "the record is consumed, so the next window is a fresh one"
        );
    }

    /// …and when the worker lands, its panes replace the placeholders.
    /// A window closed in the meantime is a normal race.
    #[test]
    fn assembled_panes_replace_the_placeholders() {
        let mut app = app_with(vec![
            win(1, vec![Pane::new_vacant(1, 80, 24)]),
            win(2, vec![Pane::new_pending(11, 80, 24), Pane::new_pending(12, 80, 24)]),
        ]);
        app.windows[1].focused_idx = 1;
        app.windows[1].selection = Some(Selection {
            session_idx: 1,
            anchor: (0, 0),
            focus: (1, 0),
            mode: marspot::ui::SelectionMode::Linewise,
        });
        app.windows[1].editing_title = Some(1);

        app.adopt_restored_panes(2, vec![Pane::new_vacant(11, 80, 24)]);

        let w = &app.windows[1];
        assert_eq!(w.panes.len(), 1);
        assert_eq!(w.panes[0].shelld_session_id(), Some(11));
        assert_eq!(w.focused_idx, 0, "focus clamped to the real pane count");
        assert!(w.selection.is_none(), "placeholder-era selection dropped");
        assert!(w.editing_title.is_none());
        assert_eq!(app.windows[0].panes.len(), 1, "peer untouched");

        // Empty assembly: keep the placeholders rather than blank the
        // window.  Unknown window: drop the panes, no panic.
        app.adopt_restored_panes(2, Vec::new());
        assert_eq!(app.windows[1].panes.len(), 1);
        app.adopt_restored_panes(99, vec![Pane::new_vacant(50, 80, 24)]);
        assert_eq!(app.windows.len(), 2);
    }

    /// RFC-005 step 4e — a handler acts on the window the event names,
    /// not on whichever window happens to hold the keyboard.  Typing
    /// into window 2 while window 1 is key must not put the preedit
    /// string in window 1.
    #[test]
    fn an_event_acts_on_the_window_it_names_not_the_focused_one() {
        let mut app = app_with(vec![
            win(1, vec![Pane::new_vacant(10, 80, 24)]),
            win(2, vec![Pane::new_vacant(20, 80, 24)]),
        ]);
        assert_eq!(app.key_window, 0);

        app.preedit(1, "あ".into());
        assert_eq!(app.windows[1].ime_preedit, "あ");
        assert_eq!(app.windows[0].ime_preedit, "", "key window must be untouched");
        assert!(app.windows[1].needs_render);

        // Plugin state is keyed by session, and a session names its
        // window — the badge must repaint the window that shows it.
        app.windows[0].needs_render = false;
        app.windows[1].needs_render = false;
        app.set_pane_badge(20, "busy".into());
        assert!(app.windows[1].needs_render, "the window holding sid 20");
        assert!(!app.windows[0].needs_render, "not the key window");
    }

    /// A drag belongs to the window that took the press for as long as
    /// the button is down — crossing into another window, or focus
    /// moving underneath it, must not redirect the selection.
    #[test]
    fn a_drag_stays_with_the_window_that_took_the_press() {
        let mut app = app_with(vec![win(1, Vec::new()), win(2, Vec::new())]);
        app.drag_window = Some(1); // pressed in window 1
        app.key_window = 1; // …and focus has since moved to window 2

        assert_eq!(app.drag_target(2), Some(0), "drag follows the press");
        app.drag_window = None;
        assert_eq!(app.drag_target(2), Some(1), "no press: the frame decides");
        assert_eq!(app.drag_target(99), None, "unknown window is dropped");
    }

    /// Focus moving repaints both windows: the focus ring leaves one
    /// and arrives in the other.  A frame for a window that already
    /// closed changes nothing.
    #[test]
    fn focusing_a_window_repaints_the_one_it_left() {
        let mut app = app_with(vec![win(1, Vec::new()), win(2, Vec::new())]);
        app.windows[0].needs_render = false;
        app.windows[1].needs_render = false;

        assert_eq!(app.focus_window(2), Some(1));
        assert_eq!(app.key_window, 1);
        assert!(app.windows[0].needs_render, "the window that lost focus");
        assert!(app.windows[1].needs_render, "the window that gained it");

        app.windows[0].needs_render = false;
        app.windows[1].needs_render = false;
        assert_eq!(app.focus_window(99), None, "unknown window");
        assert_eq!(app.key_window, 1);
        assert!(!app.windows[0].needs_render);
        assert!(!app.windows[1].needs_render);
    }

    /// Overlays belong to the window they were opened in.  The cc
    /// usage modal used to sit on `CoreApp`, and since every window
    /// publishes overlay state right before it paints, one Cmd-Shift-C
    /// drew the modal into every open window at once.
    #[test]
    fn an_overlay_opens_in_one_window_only() {
        let mut app = app_with(vec![win(1, Vec::new()), win(2, Vec::new())]);

        app.toggle_cc_usage_modal(1);
        assert!(app.windows[1].cc_usage_modal.is_some());
        assert!(app.windows[0].cc_usage_modal.is_none(), "peer must stay closed");

        app.toggle_cc_usage_modal(1);
        assert!(app.windows[1].cc_usage_modal.is_none(), "toggles off again");
    }

    /// The Esc-three-times escape hatch is about one pane refusing to
    /// give the keyboard back, so its history is per session.  Global,
    /// presses aimed at one pane could force-end another — reachable
    /// as soon as two windows each hold a locked pane.
    #[test]
    fn the_escape_hatch_counts_presses_per_session() {
        let mut app = app_with(vec![win(1, Vec::new())]);
        let t = std::time::Instant::now();

        assert!(!app.note_escape_for_pane_session(10, t));
        assert!(!app.note_escape_for_pane_session(10, t));
        assert!(
            !app.note_escape_for_pane_session(20, t),
            "another session's press must not complete this one's count"
        );
        assert!(
            app.note_escape_for_pane_session(10, t),
            "third press on THIS session fires"
        );

        // Ending the session forgets its history, so the next lock
        // starts from zero rather than firing on the first Esc.
        app.pane_session_end(10);
        assert!(!app.note_escape_for_pane_session(10, t));
    }

    /// The modal's slot map is sized from the window's own grid, so a
    /// window created with a non-default shape starts consistent.
    #[test]
    fn card_slots_match_the_grid_the_window_was_built_with() {
        let w = WindowState::new(3, Vec::new(), 0, 4, 2, 800.0, 600.0, 2.0);
        assert_eq!(w.card_slots.len(), 8);
        assert_eq!(w.pending_grid_cols, 4);
        assert_eq!(w.pending_grid_rows, 2);
    }
}

#[cfg(test)]
mod boot_assembly_tests {
    use super::*;
    use marspot::state::{SavedPane, SavedWindowLayout};
    use marspot_term::session_registry::{
        self as reg, write_session_entry, SessionEntry,
    };

    // `cargo test` runs tests as THREADS in one process; the sandbox
    // env vars (MARSPOT_STATE_DIR / MARSPOT_SESSION_BIN) are process-
    // global, so unsynchronised tests race and a spawned L3 lands in
    // the wrong sandbox (observed: wait_and_connect polling a path
    // the child never writes → spurious TimedOut).  nextest is
    // process-per-test and immune, but keep `cargo test` correct too.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct Sandbox {
        dir: std::path::PathBuf,
        _env: std::sync::MutexGuard<'static, ()>,
    }
    impl Sandbox {
        fn new(tag: &str) -> Self {
            let env = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let dir = std::env::temp_dir().join(format!(
                "marspot-rfc004-{}-{}",
                tag,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            // SAFETY: nextest runs one test per process; no other
            // thread reads these vars concurrently at this point.
            unsafe {
                std::env::set_var("MARSPOT_STATE_DIR", &dir);
                std::env::set_var("MARSPOT_SESSION_BIN", real_session_bin());
                std::env::remove_var("MARSPOT_SESSION_ID");
            }
            Self { dir, _env: env }
        }
        fn break_session_bin(&self) {
            // SAFETY: as above.
            unsafe {
                std::env::set_var(
                    "MARSPOT_SESSION_BIN",
                    "/nonexistent/marspot-session-broken",
                );
            }
        }
    }
    impl Drop for Sandbox {
        fn drop(&mut self) {
            // Kill any L3 the test spawned in this sandbox, then wipe.
            for e in reg::list_session_entries() {
                if reg::pid_is_live_session(e.pid) {
                    unsafe { libc::kill(e.pid, libc::SIGKILL) };
                }
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// target/<profile>/marspot-session — sibling of the test binary's
    /// grandparent (test bin lives in target/<profile>/deps/).
    fn real_session_bin() -> std::path::PathBuf {
        let exe = std::env::current_exe().expect("current_exe");
        exe.parent()
            .and_then(|d| d.parent())
            .map(|d| d.join("marspot-session"))
            .expect("target dir layout")
    }

    fn dead_entry(id: u64) -> SessionEntry {
        SessionEntry {
            id,
            // Far above macOS pid_max (99999) — kill(pid, 0) fails,
            // guaranteed dead.
            pid: 3_999_999,
            socket: reg::session_socket_path(id),
            cols: 80,
            rows: 24,
            title: format!("saved-title-{id}"),
            cwd: String::new(),
            proto_version: reg::PROTO_VERSION,
            created_at_unix: 1,
            shm_name: String::new(),
            shell_child_pid: 0,
        }
    }

    fn saved(panes: &[(u64, &str)]) -> SavedWindowLayout {
        SavedWindowLayout {
            grid_cols: 3,
            grid_rows: 3,
            focused_idx: 0,
            panes: panes
                .iter()
                .map(|(sid, title)| SavedPane {
                    sid: *sid,
                    custom_title: title.to_string(),
                    last_cwd: String::new(),
                })
                .collect(),
        }
    }

    fn pane_sids(panes: &[Pane]) -> Vec<u64> {
        panes
            .iter()
            .map(|p| p.session().shelld_session_id().unwrap_or(0))
            .collect()
    }

    /// Full-crash restore (the 2026-07-17 field report): every L3
    /// dead, session dirs on disk in arbitrary readdir order.  Slots
    /// must come back in SAVED order — not readdir order — with each
    /// slot's own sid resurrected as a live pane.
    #[test]
    fn dead_sessions_resurrect_in_saved_slot_order() {
        let _sb = Sandbox::new("resurrect-order");
        for id in [12u64, 5, 9] {
            write_session_entry(&dead_entry(id)).unwrap();
        }
        let s = saved(&[(9, "nine"), (12, "twelve"), (5, "five")]);
        let (tx, _rx) = mpsc::channel();
        let (panes, reattached) =
            assemble_panes_at_boot(Some(&s), 3, 60, 16, &tx, true, &Default::default());
        assert_eq!(
            pane_sids(&panes),
            vec![9, 12, 5],
            "slots must follow saved order, not readdir order"
        );
        assert!(reattached.is_empty(), "nothing was alive to reattach");
        for (i, p) in panes.iter().enumerate() {
            assert!(
                !p.is_vacant(),
                "slot {i} must be a live resurrected L3, got vacant"
            );
        }
    }

    /// Titles bind during assembly, riding the fallback chain: a
    /// saved slot title wins; an empty saved title falls back to the
    /// session's own entry.toml title.  The binding lives inside
    /// `assemble_panes_at_boot` (it used to be a loop in `main()`,
    /// invisible to these tests) and lands on `Pane::custom_title`,
    /// so it travels with the pane through any later reorder.
    #[test]
    fn titles_bind_at_assembly_saved_over_entry_toml() {
        let _sb = Sandbox::new("titles");
        for id in [4u64, 6] {
            write_session_entry(&dead_entry(id)).unwrap();
        }
        let s = saved(&[(4, "user-title"), (6, "")]);
        let (tx, _rx) = mpsc::channel();
        let (panes, _) = assemble_panes_at_boot(Some(&s), 2, 60, 16, &tx, true, &Default::default());
        assert_eq!(pane_sids(&panes), vec![4, 6]);
        assert_eq!(
            panes[0].custom_title.as_deref(),
            Some("user-title"),
            "saved slot title must win over entry.toml"
        );
        assert_eq!(
            panes[1].custom_title.as_deref(),
            Some("saved-title-6"),
            "empty saved title must fall back to the entry.toml title"
        );
    }

    /// Spawn failure must hold the slot open as a vacant pane carrying
    /// the sid — never compact, never shift the neighbours.
    #[test]
    fn spawn_failure_yields_vacant_slots_never_compacts() {
        let sb = Sandbox::new("vacant");
        sb.break_session_bin();
        let s = saved(&[(7, "seven"), (0, ""), (8, "eight")]);
        let (tx, _rx) = mpsc::channel();
        let (panes, _) =
            assemble_panes_at_boot(Some(&s), 3, 60, 16, &tx, true, &Default::default());
        assert_eq!(panes.len(), 3, "failed slots must NOT compact away");
        let sids = pane_sids(&panes);
        assert_eq!(sids[0], 7, "slot 0 keeps its sid for revive");
        assert_ne!(sids[1], 0, "anonymous slot got a fresh allocated id");
        assert_eq!(sids[2], 8, "slot 2 keeps its sid for revive");
        for (i, p) in panes.iter().enumerate() {
            assert!(p.is_vacant(), "slot {i} must be vacant");
            assert!(p.is_exited(), "vacant reports exited → revive path");
        }
    }

    /// A live session missing from the saved layout is real user work
    /// — it must be ADOPTED as an extra pane, not SIGKILLed + deleted
    /// (the pre-RFC-004 behaviour when a layout shrank).
    #[test]
    fn orphan_live_session_adopted_not_killed() {
        let _sb = Sandbox::new("adopt");
        let (tx, _rx) = mpsc::channel();
        // Bring up a REAL live L3 outside any saved layout.
        let orphan_sid = reg::allocate_next_session_id().unwrap();
        let live = spawn_l3_pane_with_cwd(60, 16, orphan_sid, "", &tx)
            .expect("real L3 spawn (is marspot-session built?)");
        // Keep it alive across the assembly (Drop would SIGKILL it);
        // Sandbox::drop kills it via the registry at test end.
        std::mem::forget(live);

        let s = saved(&[(0, "")]);
        let (panes, reattached) =
            assemble_panes_at_boot(Some(&s), 1, 60, 16, &tx, true, &Default::default());
        let sids = pane_sids(&panes);
        assert_eq!(panes.len(), 2, "slot pane + adopted orphan: {sids:?}");
        assert!(
            sids.contains(&orphan_sid),
            "live orphan {orphan_sid} must be adopted, got {sids:?}"
        );
        assert_eq!(reattached, vec![orphan_sid]);
        // And its dir must still exist (never deleted).
        assert!(reg::session_dir(orphan_sid).exists());
    }

    /// A dead dir that no slot references retires to the recycle bin
    /// (recoverable), never deletes.
    #[test]
    fn unreferenced_dead_dir_retires_to_recycle_bin() {
        let sb = Sandbox::new("retire");
        write_session_entry(&dead_entry(31)).unwrap();
        // Give it recognisable history so the retired copy is provably
        // the same dir.
        std::fs::write(reg::session_dir(31).join("bytelog"), b"HISTORY").unwrap();
        sb.break_session_bin(); // slots don't matter; keep them vacant
        let s = saved(&[(700, "")]);
        let (tx, _rx) = mpsc::channel();
        let (panes, _) =
            assemble_panes_at_boot(Some(&s), 1, 60, 16, &tx, true, &Default::default());
        assert_eq!(panes.len(), 1);
        assert!(
            !reg::session_dir(31).exists(),
            "unreferenced dead dir must move out of sessions/"
        );
        let retired_root = sb.dir.join("retired");
        let retired: Vec<_> = std::fs::read_dir(&retired_root)
            .expect("retired/ must exist")
            .flatten()
            .filter(|e| {
                e.file_name().to_string_lossy().starts_with("31-")
            })
            .collect();
        assert_eq!(retired.len(), 1, "dir 31 must be in the recycle bin");
        let bytelog = retired[0].path().join("bytelog");
        assert_eq!(std::fs::read(bytelog).unwrap(), b"HISTORY");
    }


    /// RFC-005 step 6b — a restored window runs the same assembly, but
    /// it must NOT sweep the registry.  It knows only its own saved
    /// sids, so sweeping would (a) adopt another window's live panes a
    /// second time and (b) retire the dirs of sessions it never heard
    /// of.  Both halves are checked here against a sandbox that
    /// contains exactly one of each.
    #[test]
    fn a_restore_assembly_leaves_other_windows_sessions_alone() {
        let sb = Sandbox::new("no-sweep");
        let (tx, _rx) = mpsc::channel();
        // A live session belonging to some other window.
        let other_sid = reg::allocate_next_session_id().unwrap();
        let live = spawn_l3_pane_with_cwd(60, 16, other_sid, "", &tx)
            .expect("real L3 spawn (is marspot-session built?)");
        std::mem::forget(live); // Sandbox::drop kills it via the registry
        // A dead dir the sweeping assembly would retire.
        write_session_entry(&dead_entry(31)).unwrap();
        std::fs::write(reg::session_dir(31).join("bytelog"), b"HISTORY").unwrap();

        sb.break_session_bin(); // this window's own slot stays vacant
        let s = saved(&[(700, "")]);
        let (panes, reattached) =
            assemble_panes_at_boot(Some(&s), 1, 60, 16, &tx, false, &Default::default());

        let sids = pane_sids(&panes);
        assert_eq!(panes.len(), 1, "only its own slot: {sids:?}");
        assert!(
            !sids.contains(&other_sid),
            "another window's live session must not be adopted: {sids:?}"
        );
        assert!(reattached.is_empty());
        assert!(
            reg::session_dir(31).exists(),
            "a non-sweeping assembly must not retire dirs it knows nothing about"
        );
        assert!(reg::session_dir(other_sid).exists());
    }

    /// A session another window's saved record owns is neither an
    /// orphan to adopt nor junk to retire — its window just has not
    /// been opened yet.
    ///
    /// This is the core-swap case: the replacement core assembles the
    /// boot window first, while windows 2..N are still unannounced and
    /// their L3s very much alive.  Without the reservation the boot
    /// window swallowed them, and the real window's restore then tried
    /// to reattach sessions that were already bound elsewhere.
    #[test]
    fn sessions_owned_by_another_saved_window_are_left_for_it() {
        let sb = Sandbox::new("reserved");
        let (tx, _rx) = mpsc::channel();
        // Window 2's live session…
        let live_sid = reg::allocate_next_session_id().unwrap();
        let live = spawn_l3_pane_with_cwd(60, 16, live_sid, "", &tx)
            .expect("real L3 spawn (is marspot-session built?)");
        std::mem::forget(live);
        // …and window 2's dead-but-saved session, which must keep its
        // dir so that window can resurrect it.
        write_session_entry(&dead_entry(41)).unwrap();

        sb.break_session_bin();
        let reserved: std::collections::HashSet<u64> = [live_sid, 41].into_iter().collect();
        let s = saved(&[(700, "")]);
        let (panes, reattached) =
            assemble_panes_at_boot(Some(&s), 1, 60, 16, &tx, true, &reserved);

        let sids = pane_sids(&panes);
        assert_eq!(panes.len(), 1, "boot window keeps only its own slot: {sids:?}");
        assert!(!sids.contains(&live_sid), "reserved live session adopted");
        assert!(reattached.is_empty());
        assert!(
            reg::session_dir(41).exists(),
            "reserved dead session's history must survive for its window"
        );
    }

    /// Duplicate sid in a corrupt saved state must not double-bind one
    /// session to two panes — the second slot falls back to a fresh id.
    #[test]
    fn duplicate_saved_sid_does_not_double_bind() {
        let sb = Sandbox::new("dup-sid");
        sb.break_session_bin();
        let s = saved(&[(9, "a"), (9, "b")]);
        let (tx, _rx) = mpsc::channel();
        let (panes, _) =
            assemble_panes_at_boot(Some(&s), 2, 60, 16, &tx, true, &Default::default());
        let sids = pane_sids(&panes);
        assert_eq!(sids.len(), 2);
        assert_eq!(sids[0], 9);
        assert_ne!(sids[1], 9, "second slot must not re-bind sid 9");
    }
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
    fn email_yields_copy_email_then_send_email() {
        let items = link_menu_items_for(&ctx(LinkKind::Email));
        assert_eq!(items.len(), 2, "Email must surface Copy + Send");
        assert_eq!(items[0].label, "Copy email");
        assert_eq!(items[0].action_tag, ContextMenuAction::CopyLink.tag());
        assert_eq!(items[1].label, "Send email");
        assert_eq!(items[1].action_tag, ContextMenuAction::OpenLink.tag());
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
        for kind in [
            LinkKind::Url,
            LinkKind::File,
            LinkKind::Email,
            LinkKind::Ip,
            LinkKind::Uuid,
        ] {
            let items = link_menu_items_for(&ctx(kind));
            let expected = match kind {
                LinkKind::Uuid => 1, // copy-only
                _ => 2,
            };
            assert_eq!(items.len(), expected, "{kind:?}");
        }
    }

    #[test]
    fn ip_yields_copy_ip_then_open_in_browser() {
        let items = link_menu_items_for(&ctx(LinkKind::Ip));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "Copy IP");
        assert_eq!(items[0].action_tag, ContextMenuAction::CopyLink.tag());
        assert_eq!(items[1].label, "Open in browser");
        assert_eq!(items[1].action_tag, ContextMenuAction::OpenLink.tag());
    }

    #[test]
    fn uuid_yields_copy_only() {
        let items = link_menu_items_for(&ctx(LinkKind::Uuid));
        assert_eq!(items.len(), 1, "UUID must be copy-only");
        assert_eq!(items[0].label, "Copy UUID");
        assert_eq!(items[0].action_tag, ContextMenuAction::CopyLink.tag());
    }
}

/// cc — state behind the toolbar `Cc` (Claude usage) modal.
struct CcUsageModalState {
    /// Parsed feed; `None` = file missing/unreadable (modal shows a
    /// placeholder instead of closing).
    data: Option<marspot::cc_usage::CcUsage>,
    loaded_at: Instant,
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

/// Carries an off-loop spawn result through `CoreEvent`.
///
/// A newtype only because `CoreEvent` derives `Debug` and `L3Spawn`
/// holds a `GridShmReader`, which owns a raw mapping and reasonably
/// declines to implement it.
/// Assembled panes in transit from a restore worker.  A newtype for
/// the same reason `SpawnOutcome` is one: `CoreEvent` derives Debug and
/// `Pane` does not implement it.
struct RestoredPanes(Vec<Pane>);

impl std::fmt::Debug for RestoredPanes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RestoredPanes({})", self.0.len())
    }
}

struct SpawnOutcome(Result<marspot::pane::L3Spawn, String>);

impl std::fmt::Debug for SpawnOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Ok(_) => write!(f, "SpawnOutcome(ok)"),
            Err(e) => write!(f, "SpawnOutcome(err: {e})"),
        }
    }
}

#[derive(Debug)]
enum CoreEvent {
    /// RFC-005 step 4e — every input event names its window.  L1 tags
    /// each frame with the NSWindow that received it, so nothing here
    /// has to fall back on "whichever window is key" — the two answers
    /// differ exactly when it matters (a scroll over an unfocused
    /// window, a drag that continues after focus moved).
    Key(MarspotKeyEvent, Modifiers, u32),
    MouseDown(f64, f64, Modifiers, u32),
    /// F3+9 — right-click in screen coords + modifier byte.
    /// Drives the L2 context menu (same handler shape as MouseDown).
    MouseRightDown(f64, f64, Modifiers, u32),
    MouseDrag(f64, f64, u32),
    /// Coordinates are on the wire but unused — release only ends
    /// the drag (same as src/main.rs `mouse_up`).
    MouseUp(u32),
    /// Bare mouse-move (no button).  L2 hit-tests against chrome
    /// rects so icon-button hover affordances update under the
    /// cursor.  Modifier byte on the wire is currently unused.
    MouseMove(f64, f64, u32),
    /// `(dy_phys, precise, window)`; the horizontal delta is dropped
    /// at decode (terminal scrollback is vertical-only).  The window
    /// is the one under the cursor, which on macOS receives the wheel
    /// whether or not it is key — scrolling an unfocused window must
    /// scroll THAT window.
    Scroll(f64, bool, u32),
    /// Finder file drop forwarded by L1: `(x, y)` drop point in
    /// physical px + resolved filesystem paths.  L2 hit-tests the
    /// pane and inserts shell-quoted paths via the Paste path.
    FileDrop(f64, f64, Vec<String>, u32),
    Focus(bool),
    /// RFC-005 — window-aware surface attach.  A `window_id` the core
    /// has not seen before *is* that window's birth event.
    SurfaceAttachWindow(u32, u32, f64, f64, f64, u32),
    /// RFC-005 step 6b — a restore worker finished assembling a saved
    /// window's panes; they replace that window's placeholders.
    WindowRestoreFinished(u32, RestoredPanes),
    /// RFC-005 — a window went away; drop its `WindowState`.
    WindowClosed(u32),
    /// RFC-005 — which window is key now.
    WindowFocus(u32),
    /// PROTO_VERSION=1 single-surface resize.  Kept for tolerance; the
    /// PROTO_VERSION=2 path uses `SurfaceAttach` (dual-buffer).
    Resize(u32, f64, f64, f64),
    /// PROTO_VERSION=2 dual-buffer pair handshake: `(front_id, back_id,
    /// w_phys, h_phys, scale)`.  Either announces a fresh pair (resize
    /// / restart / pending-update spawn) or re-confirms the live pair
    /// at the same dims after a restart.
    SurfaceAttach(u32, u32, f64, f64, f64),
    Preedit(String, u32),
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
    /// A background reconnect finished.  `Some(stream)` swaps the
    /// pane's control onto it; `None` means every attempt failed and
    /// the pane stays on its dead stream.
    ///
    /// The reconnect itself must not run on this loop: it polls for
    /// entry.toml and retries `connect` with backoff, up to twice the
    /// timeout it is given.  Done inline, N panes EOF'ing together (a
    /// silent update does exactly that) serialise into tens of seconds
    /// with every pane frozen.
    L3ControlReconnected(u64, Option<std::os::unix::net::UnixStream>),
    /// An off-loop `spawn_l3` finished for this session id.
    ///
    /// Bringing a pane up means forking the child, mapping its shm, then
    /// polling for its entry.toml and handshaking — up to the full
    /// `wait_and_connect` budget.  Done on this loop that froze every
    /// pane for the duration, on a path the user triggers directly
    /// ([+], and the revive-on-keystroke retry).
    L3SpawnFinished(u64, SpawnOutcome),
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
    /// Shell → core: context-menu items for a pane badge, replying to
    /// a `PaneBadgeMenuRequest` this core sent from a right-click on
    /// the badge prefix.  `(sid, anchor_x, anchor_y, items)` — the
    /// anchor echoes the request so the open is stateless.  Empty
    /// items = no menu.
    PaneBadgeMenu(u64, f64, f64, Vec<marspot::shell_proto::PaneBadgeMenuItem>),
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
        MsgType::KeyEvent => decode_key_event(&f.payload).ok().map(|(w, win)| {
            let (e, m) = wire_to_event(w);
            CoreEvent::Key(e, m, win)
        }),
        MsgType::MouseDown => decode_mouse(&f.payload)
            .ok()
            .map(|(x, y, m, win)| CoreEvent::MouseDown(x, y, mods_to_struct(m), win)),
        MsgType::MouseRightDown => decode_mouse(&f.payload).ok().map(|(x, y, m, win)| {
            CoreEvent::MouseRightDown(x, y, mods_to_struct(m), win)
        }),
        MsgType::MouseDrag => decode_mouse(&f.payload)
            .ok()
            .map(|(x, y, _, win)| CoreEvent::MouseDrag(x, y, win)),
        MsgType::MouseUp => decode_mouse(&f.payload)
            .ok()
            .map(|(_, _, _, win)| CoreEvent::MouseUp(win)),
        MsgType::MouseMove => decode_mouse(&f.payload)
            .ok()
            .map(|(x, y, _, win)| CoreEvent::MouseMove(x, y, win)),
        MsgType::Scroll => decode_scroll(&f.payload)
            .ok()
            .map(|(_dx, dy, p, win)| CoreEvent::Scroll(dy, p, win)),
        MsgType::FileDrop => decode_file_drop(&f.payload)
            .ok()
            .map(|(x, y, paths, win)| CoreEvent::FileDrop(x, y, paths, win)),
        MsgType::SurfaceAttachWindow => decode_surface_attach_window(&f.payload)
            .ok()
            .map(|(fr, bk, w, h, sc, win)| {
                CoreEvent::SurfaceAttachWindow(fr, bk, w, h, sc, win)
            }),
        MsgType::WindowClosed => decode_window_closed(&f.payload)
            .ok()
            .map(CoreEvent::WindowClosed),
        MsgType::WindowFocus => decode_window_focus(&f.payload)
            .ok()
            .map(CoreEvent::WindowFocus),
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
        MsgType::PaneBadgeMenu => marspot::shell_proto::decode_pane_badge_menu(&f.payload)
            .ok()
            .map(|(sid, x, y, items)| CoreEvent::PaneBadgeMenu(sid, x, y, items)),
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
        MsgType::Preedit => decode_preedit(&f.payload)
            .ok()
            .map(|(text, win)| CoreEvent::Preedit(text, win)),
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
/// `MARSPOT_L3=1`.  Used by `begin_pane_swap` to bring up a
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
    // 10 s, explicitly.  This used to read 5 s and get 10 in practice,
    // because `wait_and_connect` ran a fresh deadline for each of its
    // two halves.  Now that the deadline is honestly single, the number
    // here has to be the real budget — and a booting L3 needs it: it
    // execs, then applies a state.bin that can run into the hundreds of
    // KB before it binds its socket.  Halving it silently turned a slow
    // boot into a lost pane (caught by the boot-assembly tests).
    let control = match marspot::uds_session_client::wait_and_connect(
        session_id,
        std::time::Duration::from_secs(10),
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

/// Start a pane whose L3 is brought up **off** this loop.
///
/// Returns immediately with a slot rendering "starting…"; the fork,
/// shm map, entry.toml poll and handshake all run on a worker, and
/// `CoreEvent::L3SpawnFinished` delivers the result.  Boot assembly
/// keeps calling `spawn_l3_pane_with_cwd` synchronously — there is no
/// loop to freeze before the loop starts, and boot wants the pane fully
/// formed before it lays out.
fn spawn_l3_pane_async(
    cols: u16,
    rows: u16,
    session_id: u64,
    event_tx: &Sender<CoreEvent>,
) -> Pane {
    let tx = event_tx.clone();
    // Both callers ([+] and revive) want the default cwd; a fresh L3
    // resolves it from $HOME.  Carrying a parameter that is always ""
    // just invites a reader to look for the caller that sets it.
    let cwd = String::new();
    std::thread::Builder::new()
        .name(format!("l2-spawn-{session_id}"))
        .spawn(move || {
            let inner_tx = tx.clone();
            let outcome = spawn_l3_with_cwd(cols, rows, session_id, &cwd, &inner_tx)
                .map_err(|e| format!("{e}"));
            let _ = tx.send(CoreEvent::L3SpawnFinished(
                session_id,
                SpawnOutcome(outcome),
            ));
        })
        .expect("spawn l2-spawn worker");
    Pane::new_pending(session_id, cols, rows)
}

/// RFC-005 step 6b — assemble a restored window's panes on a worker
/// thread and deliver them whole via `CoreEvent::WindowRestoreFinished`.
///
/// Off-loop is not an optimisation here, it is the rule: reattaching an
/// L3 blocks on a UDS handshake with its own deadline, and RFC-005 says
/// opening a window must never freeze the windows already up.  The
/// window appears immediately holding one "starting…" pane per saved
/// slot; the real panes replace them when the worker lands.
///
/// `sweeps_registry: false` — see `assemble_panes_at_boot`.
fn assemble_restore_window_async(
    window_id: u32,
    record: marspot::state::SavedWindowLayout,
    cols: u16,
    rows: u16,
    event_tx: &Sender<CoreEvent>,
) {
    let tx = event_tx.clone();
    std::thread::Builder::new()
        .name(format!("l2-restore-w{window_id}"))
        .spawn(move || {
            let n = record.panes.len().clamp(1, SESSION_COUNT_HARD_CAP);
            let inner_tx = tx.clone();
            let (panes, _reattached) = assemble_panes_at_boot(
                Some(&record),
                n,
                cols,
                rows,
                &inner_tx,
                false,
                &std::collections::HashSet::new(),
            );
            let _ = tx.send(CoreEvent::WindowRestoreFinished(
                window_id,
                RestoredPanes(panes),
            ));
        })
        .expect("spawn l2-restore worker");
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
/// through to `spawn_l3_pane_with_cwd`.
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
    // 4 s, explicitly — the effective budget this path had before the
    // deadline was made single.  Reattach races a live L3 that may be
    // mid-execv.
    let control = marspot::uds_session_client::wait_and_connect(
        session_id,
        std::time::Duration::from_secs(4),
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
/// One native window: its pane grid plus every piece of UI state
/// scoped to that window.
///
/// RFC-005 — the uniform model is **window → pane, always**; a lone
/// pane is `windows.len() == 1, panes.len() == 1`, not a special
/// case.  Everything here used to sit directly on `CoreApp`, which
/// silently encoded "there is exactly one window" into ~450 field
/// accesses.
///
/// What is NOT here (stays on `CoreApp`): anything keyed by session
/// id, which travels with a pane across windows for free, and the
/// renderer's shared resources.
/// Where one window's frames land: the IOSurface pair L1 handed us
/// for it, the Metal textures wrapping them, and which half the next
/// frame writes into.
///
/// RFC-005 step 4d — this used to be three locals in `main()`, which
/// is how "there is exactly one window" was encoded on the L2 side:
/// a second window's `SurfaceAttach` overwrote the first window's
/// pair, and from then on the first window's presenter sampled a
/// surface nobody was painting into.  Windows are peers; each owns
/// its own paint target.
struct WindowSurfaces {
    pair: [IOSurface; 2],
    tex: [objc2::rc::Retained<ProtocolObject<dyn MTLTexture>>; 2],
    /// The half the next render writes; flipped after each frame so
    /// the shell only ever presents a surface the GPU has finished.
    writing_idx: usize,
}

impl WindowSurfaces {
    /// Look both ids up, retain them, and wrap them in textures.
    /// `None` when either lookup or texture creation fails — the
    /// caller keeps whatever pair the window already had, which is
    /// the difference between a dropped frame and a black window.
    fn attach(
        front_id: u32,
        back_id: u32,
        device: &ProtocolObject<dyn objc2_metal::MTLDevice>,
    ) -> Option<Self> {
        let front = IOSurface::lookup(front_id)?;
        let Some(back) = IOSurface::lookup(back_id) else {
            return None;
        };
        front.increment_use();
        back.increment_use();
        let tex_f = front.make_metal_texture(device);
        let tex_b = back.make_metal_texture(device);
        match (tex_f, tex_b) {
            (Ok(tf), Ok(tb)) => Some(Self {
                pair: [front, back],
                tex: [tf, tb],
                writing_idx: 0,
            }),
            (tf, tb) => {
                if let Err(e) = tf {
                    lx_error!("core.attach.metal_texture_front_failed", &format!("{e:?}"));
                }
                if let Err(e) = tb {
                    lx_error!("core.attach.metal_texture_back_failed", &format!("{e:?}"));
                }
                front.decrement_use();
                back.decrement_use();
                None
            }
        }
    }

    /// Balance the two `increment_use` calls `attach` made.  Called
    /// when the pair is replaced by a fresh one and when the window
    /// itself goes away.
    fn release(&self) {
        self.pair[0].decrement_use();
        self.pair[1].decrement_use();
    }

    fn writing_tex(&self) -> objc2::rc::Retained<ProtocolObject<dyn MTLTexture>> {
        self.tex[self.writing_idx].clone()
    }

    fn writing_surface_id(&self) -> u32 {
        self.pair[self.writing_idx].id()
    }

    fn flip(&mut self) {
        self.writing_idx = 1 - self.writing_idx;
    }
}

struct WindowState {
    /// L1-allocated, monotonic.  Stable across a core swap because
    /// L1 replays one `SurfaceAttach` per window into the new core.
    window_id: u32,
    /// This window's paint target.  `None` between the window's birth
    /// and its first successful attach (a stale id from a mid-spawn
    /// surface rotation), during which the window simply isn't
    /// rendered — the other windows keep painting.
    surfaces: Option<WindowSurfaces>,
    /// Per-window frame-interval cap.  Global would let a busy window
    /// gate a quiet one's repaint, which is exactly the coupling the
    /// peer model forbids.
    last_render_at: Option<Instant>,
    /// Has this window ever completed a frame?  Drives one INFO line
    /// per window — "did this window ever paint" is the first question
    /// asked of a black window, and the per-frame log is sampled 1-in-8
    /// so a quiet window can leave no trace of painting at all.
    painted_once: bool,
    /// The renderer state that cannot be shared with the other
    /// windows — this window's per-pane instance caches and its own
    /// "still owes a clear" flag.  Everything else the renderer holds
    /// (device, pipelines, font cache, both atlases, every scratch
    /// buffer) is shared, which is why a second window costs a layout
    /// and a render target rather than a second atlas.
    render: marspot::render_metal::WindowRender,
    layout: Layout,
    panes: Vec<Pane>,
    focused_idx: usize,
    editing_title: Option<usize>,
    title_edit_buffer: String,
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
    /// cc — open `Cc` usage modal.  `None` = closed; the feed file is
    /// only read while this is `Some` (open + 5 s refresh).
    ///
    /// Per window, like the process panel next to it.  Held on
    /// `CoreApp` it was drawn into EVERY window at once, because each
    /// window publishes the modal state right before it paints.
    cc_usage_modal: Option<CcUsageModalState>,
    ime_preedit: String,
    /// Window physical dims + scale, updated by Resize frames.
    w_phys: f64,
    h_phys: f64,
    scale: f64,
    /// Set by anything that changes what the next frame should look
    /// like; cleared after each `render`.
    needs_render: bool,
    /// Last caret rect sent to the shell — dedupe so an idle cursor
    /// doesn't stream identical CaretRect frames at render cadence.
    last_caret_sent: Option<Option<(f64, f64, f64, f64)>>,
}

/// L1 allocates window ids; the boot window is always this one, so
/// a core that has not yet heard a `SurfaceAttach` still has a
/// well-defined key window.
const FIRST_WINDOW_ID: u32 = 1;

impl WindowState {
    /// A window with its panes already assembled.  `layout` is a
    /// placeholder — the caller runs `rebuild_layout`, which needs
    /// chrome state that only exists once `CoreApp` is whole.
    #[allow(clippy::too_many_arguments)]
    fn new(
        window_id: u32,
        panes: Vec<Pane>,
        focused_idx: usize,
        grid_cols: usize,
        grid_rows: usize,
        w_phys: f64,
        h_phys: f64,
        scale: f64,
    ) -> Self {
        Self {
            window_id,
            surfaces: None,
            last_render_at: None,
            painted_once: false,
            render: marspot::render_metal::WindowRender::new(),
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
            focused_idx,
            editing_title: None,
            title_edit_buffer: String::new(),
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
            sidebar_collapsed: true,
            hover_chrome_btn: None,
            process_panel: None,
            cc_usage_modal: None,
            ime_preedit: String::new(),
            w_phys,
            h_phys,
            scale,
            needs_render: true,
            last_caret_sent: None,
        }
    }

    /// The focused pane.
    ///
    /// Indexing rather than `get`: every mutation of `panes` clamps
    /// `focused_idx`, so out of range is a bug worth hearing about.
    /// This exists because `windows[k].panes[windows[k].focused_idx]`
    /// is a borrow conflict — the index read and the element borrow
    /// both go through `windows`.  Reading the index into the
    /// method's own frame first is the fix, and it reads better at
    /// the call site.
    #[inline]
    fn focused_pane_mut(&mut self) -> &mut Pane {
        let i = self.focused_idx;
        &mut self.panes[i]
    }

    /// Tolerant variants, for the paths that legitimately run while
    /// the pane list is empty or shrinking.
    #[inline]
    fn try_focused_pane(&self) -> Option<&Pane> {
        self.panes.get(self.focused_idx)
    }

    #[inline]
    fn try_focused_pane_mut(&mut self) -> Option<&mut Pane> {
        let i = self.focused_idx;
        self.panes.get_mut(i)
    }
}

/// The key window's state.
///
/// A macro rather than an accessor method on purpose: it expands to
/// a plain field path, so the borrow checker still sees
/// `win!(self).panes` and `self.renderer` as disjoint borrows.  An
/// `fn win_mut(&mut self)` would borrow all of `self` and make most
/// of the render path unwritable.
///
/// Every use marks a site that means "the window the user is acting
/// on".  RFC-005 step 4d threads an explicit window index into the
/// event handlers, at which point these become `win!(self, wi)`.
///
/// The two-argument form is the peer form: it names the window being
/// worked on outright, with no appeal to which window happens to hold
/// keyboard focus.  Anything that runs on behalf of *all* windows (the
/// pump, the render pass, persistence) must use it.
macro_rules! win {
    ($s:expr) => {
        $s.windows[$s.key_window]
    };
    ($s:expr, $i:expr) => {
        $s.windows[$i]
    };
}

struct CoreApp {
    renderer: MetalRenderer,
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
    /// Rolling timestamps of recent Escape presses per locked session;
    /// 3 within 5 s force-ends THAT session regardless of plugin
    /// opinion.  Bounded to the last 3 entries per session, and the
    /// entry is dropped when the session ends.
    ///
    /// Keyed by session, not global: the escape hatch is about one
    /// pane refusing to give the keyboard back, and with two windows a
    /// global deque let presses aimed at one pane satisfy the
    /// threshold for another.
    esc_history: std::collections::HashMap<u64, std::collections::VecDeque<std::time::Instant>>,
    /// F3+5 — per-sid debounce window for `refresh_pane_cwd_for`.
    /// A burst of Enter keys (multi-line paste) hits this map and
    /// returns within the debounce → at most one syscall per
    /// `CWD_REFRESH_DEBOUNCE` per pane.  Cleared per-sid on
    /// `close_session`.
    last_cwd_refresh: std::collections::HashMap<u64, Instant>,
    /// Sessions with a reconnect already in flight.  Without this, a
    /// burst of EOFs for one session would spawn a thread each.
    reconnecting: std::collections::HashSet<u64>,
    /// Consecutive cwd-resolution failures per session.  Past
    /// `CWD_MAX_FAILURES` the per-frame lazy fill stops asking.
    cwd_unresolvable: std::collections::HashMap<u64, u32>,
    /// True once every pane's session has exited — the loop exits
    /// cleanly and the shell respawns a fresh core (which creates a
    /// fresh session), mirroring "marspot quits when all shells die".
    all_exited: bool,
    /// True once a `SurfaceAttachWindow` has arrived.  From then on the
    /// legacy `SurfaceAttach` is ignored as its duplicate — the shell
    /// sends both so that a core predating RFC-005 still gets its
    /// surface.
    saw_window_aware_attach: bool,
    /// `MARSPOT_L3=1`: panes are per-session L3 processes, so [+] spawns
    /// a fresh L3 (with an L2-allocated session) instead of an in-process
    /// shelld pane.  Clone of the event channel so a new L3's poke reader
    /// can wake the loop, exactly like the boot spawns.
    l3_mode: bool,
    event_tx: Sender<CoreEvent>,
    /// Every open window, in creation order.  Never empty: the last
    /// window closing exits the app.
    windows: Vec<WindowState>,
    /// The window that took the current mouse press, if any.  Set on
    /// press, cleared on release; `drag_target` reads it.
    drag_window: Option<u32>,
    /// RFC-005 step 6b — saved windows past the boot one, waiting for
    /// L1 to reopen them.  Each `SurfaceAttachWindow` for an unseen id
    /// pops the front record, so the queue is also what distinguishes
    /// "restoring a window" from "the user pressed Cmd-N".
    saved_windows: std::collections::VecDeque<marspot::state::SavedWindowLayout>,
    /// Index into `windows` of the window with keyboard focus.
    key_window: usize,
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
            self.mark_sid_window_dirty(shelld_session_id);
        }
    }

    /// L1 plugin released the pane back to live mode.
    fn pane_session_end(&mut self, shelld_session_id: u64) {
        self.esc_history.remove(&shelld_session_id);
        if self.pane_sessions.remove(&shelld_session_id).is_some() {
            self.mark_sid_window_dirty(shelld_session_id);
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
    fn focused_pane_active_session(&self, wi: usize) -> Option<u64> {
        let p = win!(self, wi).try_focused_pane()?;
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
    fn note_escape_for_pane_session(&mut self, sid: u64, now: std::time::Instant) -> bool {
        const WINDOW: std::time::Duration = std::time::Duration::from_secs(5);
        const THRESHOLD: usize = 3;
        let hist = self.esc_history.entry(sid).or_default();
        while let Some(&front) = hist.front() {
            if now.duration_since(front) > WINDOW {
                hist.pop_front();
            } else {
                break;
            }
        }
        hist.push_back(now);
        if hist.len() > THRESHOLD {
            hist.pop_front();
        }
        hist.len() >= THRESHOLD
    }

    /// L1 plugin → control socket → here: stash a per-shelld-session
    /// right-side decoration for the title strip.  Empty `text` clears
    /// any prior badge.  Forces a redraw on transition.
    /// cc plugin asked to push raw bytes into the PTY behind
    /// `shelld_session_id`.  Find the matching L3 pane and let it
    /// forward via the existing control channel.
    /// Plugin-injected bytes for one session.  Sid-keyed, so it looks
    /// in every window — the plugin knows nothing about windows and
    /// the pane may well be in one that is not focused.
    fn inject_input(&mut self, shelld_session_id: u64, bytes: &[u8]) {
        for pane in self.windows.iter_mut().flat_map(|w| w.panes.iter_mut()) {
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
            self.mark_sid_window_dirty(shelld_session_id);
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
            self.mark_sid_window_dirty(shelld_session_id);
        }
    }

    fn rebuild_layout(&mut self, wi: usize) {
        let (cell_w, cell_h) = self.renderer.cell_dims();
        let sidebar_phys = if win!(self, wi).sidebar_collapsed {
            0.0
        } else {
            SIDEBAR_W_LOGICAL * win!(self, wi).scale
        };
        let (lc, lr) = (win!(self, wi).grid_cols, win!(self, wi).grid_rows);
        let layout = Layout::build(
            win!(self, wi).w_phys,
            win!(self, wi).h_phys,
            sidebar_phys,
            HEADER_PT * win!(self, wi).scale,
            CELL_TITLE_PT * win!(self, wi).scale,
            lc,
            lr,
            cell_w,
            cell_h,
        )
        .with_chrome(
            win!(self, wi).scale,
            win!(self, wi).panes.len(),
            marspot::TITLE_STRIP_PT * win!(self, wi).scale,
        );
        for (i, p) in win!(self, wi).panes.iter_mut().enumerate() {
            if let Some(rect) = layout.cells.get(i) {
                p.resize(rect.cols, rect.rows);
            }
        }
        win!(self, wi).layout = layout;
        win!(self, wi).needs_render = true;
        // The shape of the BG region just changed (sidebar / layout
        // mode / pane count / window dims).  Force a hard Clear on
        // the next IOSurface render so any newly-uncovered area
        // shows SIDEBAR_BG, not the previous frame's stale pixels.
        // (Steady-state frames use Load to dodge the cross-process
        // race; see `MetalRenderer::clear_bg_required`.)
        win!(self, wi).render.mark_bg_clear_required();
    }

    /// F3+3.3 — reset `card_slots` to identity for the current
    /// pending grid shape.  Called when the modal opens, when the
    /// user changes cols/rows in the modal (since the cell count
    /// changes), and on apply (after permuting).
    /// F3+3.6 — batch pull-based cwd refresh.  Walks every pane,
    /// `read_shell_child_pid` + `pidtree::proc_cwd` per pane.  Used
    /// by LayoutModal open to one-shot fresh all 9.
    fn refresh_pane_cwds(&mut self, wi: usize) {
        let now = Instant::now();
        for pane in &win!(self, wi).panes {
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
    fn refresh_pane_cwd_for(&mut self, wi: usize, pane_idx: usize, force: bool) -> bool {
        let Some(pane) = win!(self, wi).panes.get(pane_idx) else { return false };
        let Some(sid) = pane.shelld_session_id() else { return false };
        let now = Instant::now();
        if force {
            // An explicit trigger means something just changed; drop
            // the give-up mark so this attempt really runs.
            self.cwd_unresolvable.remove(&sid);
        }
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
        // F3+5.2 — the debounce above bounds the *rate* but not the
        // *duration*: a pane whose cwd can never be resolved (entry.toml
        // without shell_child_pid, sandbox-blocked proc_pidinfo) kept
        // retrying at 1/150 ms for the life of the process, because
        // `lazy_fill_missing_cwds` asks again every frame for any pane
        // with no cached entry.  Nine such panes is ~60 open+read+close
        // per second, forever.  Give up after a bounded number of
        // consecutive failures.  A real trigger (`force=true` — pane
        // spawn, modal open, Enter) bypasses this check outright, so a
        // pane that becomes resolvable later is never stuck; the mark
        // itself is only cleared on a successful resolve.
        if !force && self.cwd_unresolvable.get(&sid).is_some_and(|&n| n >= CWD_MAX_FAILURES) {
            return false;
        }
        let failed = |me: &mut Self| {
            *me.cwd_unresolvable.entry(sid).or_insert(0) += 1;
        };
        let Some(pid) = read_shell_child_pid(sid) else {
            failed(self);
            return false;
        };
        let Some(path) = marspot::pidtree::proc_cwd(pid) else {
            failed(self);
            return false;
        };
        self.pane_cwds.insert(sid, path.to_string_lossy().into_owned());
        // Resolved — clear any accumulated failure count so a pane that
        // goes unresolvable again gets a fresh budget.
        self.cwd_unresolvable.remove(&sid);
        true
    }

    /// F3+6 — snapshot every persistable bit of state to
    /// `shell-state.bin`.  Called from spawn / close / focus-change /
    /// layout-apply / title-commit so a hard kill leaves a recent
    /// state on disk.  ~50 us per call (memcpy + atomic rename); no
    /// debounce because we never call this on the render hot path.
    fn save_session_state(&self) {
        use marspot::state::{SavedPane, SavedState, SavedWindowLayout};
        // RFC-005 step 6 — every window, in creation order.  Saving
        // only the key window is what made opening a second window
        // destructive: the new window became key the instant it
        // appeared, and the next save replaced a 16-pane record with
        // its single pane.
        let windows: Vec<SavedWindowLayout> = self
            .windows
            .iter()
            .map(|w| SavedWindowLayout {
                grid_cols: w.grid_cols as u16,
                grid_rows: w.grid_rows as u16,
                focused_idx: w.focused_idx as u16,
                panes: w
                    .panes
                    .iter()
                    .map(|p| {
                        let sid = p.shelld_session_id().unwrap_or(0);
                        let custom_title = p.custom_title.clone().unwrap_or_default();
                        let last_cwd = self.pane_cwds.get(&sid).cloned().unwrap_or_default();
                        SavedPane { sid, custom_title, last_cwd }
                    })
                    .collect(),
            })
            .collect();
        let saved = SavedState {
            windows,
            key_window: self.key_window as u16,
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
    fn lazy_fill_missing_cwds(&mut self, wi: usize) {
        let mut filled = false;
        for i in 0..win!(self, wi).panes.len() {
            let Some(sid) = win!(self, wi).panes[i].shelld_session_id() else { continue };
            if self.pane_cwds.contains_key(&sid) { continue; }
            // F3+5.1 — `force=false`: paired with the now-always-bumped
            // debounce clock in `refresh_pane_cwd_for`, this means a
            // pane that hasn't filled yet retries at most every
            // `CWD_REFRESH_DEBOUNCE`, not every frame.  Steady state
            // (all populated) skips entirely via contains_key above.
            if self.refresh_pane_cwd_for(wi, i, false) && self.pane_cwds.contains_key(&sid) {
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

    fn reset_card_slots(&mut self, wi: usize) {
        let cells = win!(self, wi).pending_grid_cols * win!(self, wi).pending_grid_rows;
        win!(self, wi).card_slots = (0..cells).collect();
        win!(self, wi).layout_drag = None;
    }

    /// F3+3.3 — apply `card_slots` as a permutation on the leading
    /// `cells` panes.  After this, `win!(self).panes[slot_idx]` is the
    /// pane that previously sat at `card_slots[slot_idx]` (== the
    /// pane the user dragged into slot_idx in the modal).
    ///
    /// `card_slots` entries that point past the live pane count
    /// are skipped (empty cards stay empty).  The post-cells tail
    /// of `win!(self).panes` (sidebar overflow) is untouched.  Titles,
    /// scroll offsets, search state all travel for free — they are
    /// fields of `Pane`, and it is whole `Pane`s being permuted.
    /// Resets `card_slots` to identity afterwards.
    fn apply_card_slot_permutation(&mut self, wi: usize, cells: usize) {
        let n_in_grid = cells.min(win!(self, wi).panes.len());
        if n_in_grid == 0 || win!(self, wi).card_slots.len() < n_in_grid {
            self.reset_card_slots(wi);
            return;
        }
        // Build new layouts for the leading n_in_grid slots.  Slots
        // pointing at out-of-range pane indices map to None (empty)
        // and the corresponding existing pane keeps its place at
        // the tail (skipped during reorder).
        let mut new_panes: Vec<Option<Pane>> = (0..n_in_grid).map(|_| None).collect();
        // Drain the leading n_in_grid panes into Option holders so
        // we can move them around without re-borrow conflicts.
        let mut drained: Vec<Option<Pane>> =
            win!(self, wi).panes.drain(..n_in_grid).map(Some).collect();
        for slot_idx in 0..n_in_grid {
            let from = win!(self, wi).card_slots[slot_idx];
            if from < drained.len() {
                new_panes[slot_idx] = drained[from].take();
            }
        }
        // Re-insert at the head.  Any leftover (None) means the slot
        // had no source pane — should not happen with identity-only
        // permutations but defensible: pull from a leftover pool to
        // avoid panicking.
        let mut leftover: Vec<Pane> = drained.into_iter().flatten().collect();
        let mut ordered: Vec<Pane> = Vec::with_capacity(n_in_grid);
        for i in 0..n_in_grid {
            match new_panes[i].take() { Some(p) => {
                ordered.push(p);
            } _ => { match leftover.pop() { Some(p) => {
                ordered.push(p);
            } _ => {}}}}
        }
        // Re-prepend.
        let tail_panes = std::mem::take(&mut win!(self, wi).panes);
        win!(self, wi).panes = ordered;
        win!(self, wi).panes.extend(tail_panes);
        self.reset_card_slots(wi);
    }

    /// Spawn a fresh session and append it.  Refuses past
    /// `SESSION_COUNT_HARD_CAP`.  Sized to the cell it will land in
    /// (falling back to the first cell's shape) so the shell prompt
    /// prints at the right width from its very first byte.
    // ─── F3+9 — right-click context menu (split-arch L2 side) ────────

    fn mouse_right_down(
        &mut self, wi: usize,
        x_phys: f64,
        y_phys: f64,
        _modifiers: Modifiers,
    ) {
        // Second right-click closes the previous menu first.
        if win!(self, wi).context_menu.take().is_some() {
            win!(self, wi).needs_render = true;
        }
        // Badge-prefix right-click: the menu CONTENT lives in the L1
        // plugin that owns the badge, so ask it (PaneBadgeMenuRequest)
        // and open the menu when the PaneBadgeMenu reply arrives —
        // same request/response shape as GetSelectionText.  Checked
        // ahead of the link / region paths so `P<n>` never opens the
        // generic pane menu.
        if let Some(i) = self.hit_test_pane_badge_prefix(wi, x_phys, y_phys) {
            if let Some(sid) = win!(self, wi).panes.get(i).and_then(|p| p.shelld_session_id()) {
                let payload = marspot::shell_proto::encode_pane_badge_menu_request(
                    sid, x_phys, y_phys,
                );
                self.pending_to_shell
                    .push((MsgType::PaneBadgeMenuRequest, payload));
                return;
            }
        }
        // A right-click that lands on a recognised URL / file path
        // gets a link-specific menu (Open / Copy) instead of the
        // generic pane menu.  Email is recognised but inert.
        let link = self.hit_test_link_at_xy(wi, x_phys, y_phys);
        let region = self.resolve_context_region(wi, x_phys, y_phys);
        let items = match &link {
            Some(l) => self.build_link_menu_items(l),
            None => self.build_menu_items(wi, region),
        };
        if items.is_empty() {
            return;
        }
        win!(self, wi).context_menu = Some(ContextMenuState {
            items,
            anchor_x: x_phys,
            anchor_y: y_phys,
            region,
            hovered_idx: None,
            link,
        });
        win!(self, wi).needs_render = true;
    }

    /// Open the badge context menu from a `PaneBadgeMenu` reply.  The
    /// anchor is the echoed right-click position; empty items = the
    /// owning plugin has nothing to offer, show nothing.
    fn open_pane_badge_menu(
        &mut self, wi: usize,
        sid: u64,
        anchor_x: f64,
        anchor_y: f64,
        items: Vec<marspot::shell_proto::PaneBadgeMenuItem>,
    ) {
        if items.is_empty() {
            return;
        }
        let items = items
            .into_iter()
            .map(|it| marspot::ui::components::MenuItem::entry(&it.label, it.tag))
            .collect();
        win!(self, wi).context_menu = Some(ContextMenuState {
            items,
            anchor_x,
            anchor_y,
            region: ContextRegion::PaneBadge(sid),
            hovered_idx: None,
            link: None,
        });
        win!(self, wi).needs_render = true;
    }

    fn resolve_context_region(&self, wi: usize, x_phys: f64, y_phys: f64) -> ContextRegion {
        // Sidebar row check first — wins over the cell-area hit when
        // both overlap (the sidebar overlays the title-strip band).
        let row_phys = marspot_term::layout::SIDEBAR_ROW_H_PHYS;
        let top_pad_phys = win!(self, wi).layout.top_inset + win!(self, wi).layout.sidebar_top_pad_phys;
        if let Some(idx) = win!(self, wi).layout.hit_test_sidebar_row(
            x_phys, y_phys, top_pad_phys, row_phys, win!(self, wi).panes.len(),
        ) {
            return ContextRegion::SidebarSlot(idx);
        }
        if let Some(idx) = win!(self, wi).layout.hit_test(x_phys, y_phys) {
            return ContextRegion::Pane(idx);
        }
        ContextRegion::TitleStrip
    }

    fn build_menu_items(
        &self, wi: usize,
        region: ContextRegion,
    ) -> Vec<marspot::ui::components::MenuItem> {
        use marspot::ui::components::MenuItem;
        match region {
            ContextRegion::Pane(_) => {
                let close_disabled = win!(self, wi).panes.len() <= 1;
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
                    if win!(self, wi).selection.is_some() { copy } else { copy.disabled() },
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
                let close_disabled = win!(self, wi).panes.len() <= 1;
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
            // Badge menus never come through here — their items
            // arrive from L1 via PaneBadgeMenu and open through
            // `open_pane_badge_menu`.
            ContextRegion::PaneBadge(_) => Vec::new(),
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
        &mut self, wi: usize,
        action: ContextMenuAction,
        region: ContextRegion,
    ) {
        // Snapshot the link before clearing the menu — the OpenLink /
        // CopyLink arms read it after the clear.  Kind matters for
        // OpenLink (Email needs a `mailto:` prefix so `open(1)` routes
        // to the default mail client, not the browser).
        let link_snapshot = win!(self, wi)
            .context_menu
            .as_ref()
            .and_then(|s| s.link.clone());
        win!(self, wi).context_menu = None;
        match action {
            ContextMenuAction::CopySelection => {
                let _ = self.copy_selection_to_clipboard(wi);
            }
            ContextMenuAction::Paste => {
                if let Some(txt) = marspot::input::read_clipboard_text() {
                    if let Some(pane) = win!(self, wi).try_focused_pane_mut() {
                        pane.session_mut().forward_paste(&txt);
                    }
                }
            }
            ContextMenuAction::ClearScrollback => {
                if let Some(pane) = win!(self, wi).try_focused_pane_mut() {
                    pane.session_mut().forward_inject_input(b"\x1b[3J");
                }
            }
            ContextMenuAction::ClosePane => {
                let idx = match region {
                    ContextRegion::Pane(i) | ContextRegion::SidebarSlot(i) => i,
                    _ => win!(self, wi).focused_idx,
                };
                if win!(self, wi).panes.len() > 1 && idx < win!(self, wi).panes.len() {
                    self.close_session(wi, idx);
                    self.rebuild_layout(wi);
                }
            }
            ContextMenuAction::SplitNewPane => {
                if win!(self, wi).panes.len() < marspot::ui::SESSION_COUNT_HARD_CAP {
                    self.spawn_session(wi);
                    self.rebuild_layout(wi);
                }
            }
            ContextMenuAction::RenameTitle => {
                let idx = match region {
                    ContextRegion::Pane(i) | ContextRegion::SidebarSlot(i) => i,
                    _ => win!(self, wi).focused_idx,
                };
                if idx < win!(self, wi).panes.len() {
                    win!(self, wi).editing_title = Some(idx);
                }
            }
            ContextMenuAction::ToggleSidebar => {
                win!(self, wi).sidebar_collapsed = !win!(self, wi).sidebar_collapsed;
                self.rebuild_layout(wi);
            }
            ContextMenuAction::OpenLayout => {
                win!(self, wi).layout_modal_open = true;
            }
            ContextMenuAction::OpenLink => {
                if let Some(link) = link_snapshot.as_ref() {
                    let arg = match link.kind {
                        marspot::grid_links::LinkKind::Email => {
                            Some(format!("mailto:{}", link.text))
                        }
                        // Bare IP (`47.96.114.231`, `::1`,
                        // `2001:db8::1`, `[fe80::1]:8080/foo`) needs an
                        // `http://` scheme so `open(1)` routes it —
                        // defaults to port 80.  Bare IPv6 without
                        // brackets gets wrapped so URL parsers accept
                        // it (colons in a hostname are ambiguous with
                        // `host:port` otherwise).
                        marspot::grid_links::LinkKind::Ip => {
                            Some(if link.text.starts_with('[')
                                || !ip_text_is_bare_ipv6(&link.text)
                            {
                                format!("http://{}", link.text)
                            } else {
                                format!("http://[{}]", link.text)
                            })
                        }
                        marspot::grid_links::LinkKind::Url => {
                            let head: String = link
                                .text
                                .chars()
                                .take(8)
                                .flat_map(char::to_lowercase)
                                .collect();
                            Some(
                                if head.starts_with("http://")
                                    || head.starts_with("https://")
                                {
                                    link.text.clone()
                                } else {
                                    format!("http://{}", link.text)
                                },
                            )
                        }
                        // UUID has no Open action (menu doesn't offer
                        // it); if a stale click reaches here, no-op.
                        marspot::grid_links::LinkKind::Uuid => None,
                        marspot::grid_links::LinkKind::File => {
                            Some(link.text.clone())
                        }
                    };
                    if let Some(a) = arg {
                        spawn_open(&a);
                    }
                }
            }
            ContextMenuAction::CopyLink => {
                if let Some(link) = link_snapshot.as_ref() {
                    let _ = marspot::input::write_clipboard_text(&link.text);
                }
            }
        }
        win!(self, wi).needs_render = true;
    }

    fn spawn_session(&mut self, wi: usize) {
        if win!(self, wi).panes.len() >= SESSION_COUNT_HARD_CAP {
            return;
        }
        let (cols, rows) = win!(self, wi)
            .layout
            .cells
            .get(win!(self, wi).panes.len())
            .or_else(|| win!(self, wi).layout.cells.first())
            .map(|c| (c.cols, c.rows))
            .unwrap_or((INITIAL_COLS, INITIAL_ROWS));
        if self.l3_mode {
            // RFC-003 step 3a: L2 allocates the session id itself via
            // the on-disk registry (sessions/.next_id with flock), no
            // L4 round-trip.
            // The spawn itself runs off-loop; this only allocates the
            // id (a flock + a directory scan) and pushes a "starting…"
            // slot.  Clicking [+] used to freeze every pane for as long
            // as the new L3 took to register and handshake.
            match allocate_next_session_id().map(|id| {
                spawn_l3_pane_async(cols, rows, id, &self.event_tx)
            }) {
                Ok(pane) => {
                    win!(self, wi).panes.push(pane);
                    // F3+5 — initial cwd pull for the new pane so the
                    // title strip lands populated on its first paint.
                    // shell_child_pid may not be written yet on this
                    // very tick — `refresh_pane_cwd_for` silently
                    // returns false, and the build_views lazy-fill
                    // catches it on a later frame.
                    let new_idx = win!(self, wi).panes.len() - 1;
                    self.refresh_pane_cwd_for(wi, new_idx, true);
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
        for pane in self.windows.iter().flat_map(|w| w.panes.iter()) {
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
    fn resolve_pending_on_defocus(&mut self, wi: usize, new_idx: usize) {
        if new_idx != win!(self, wi).focused_idx
            && win!(self, wi)
                .panes
                .get(win!(self, wi).focused_idx)
                .is_some_and(|p| p.update_pending())
        {
            self.begin_pane_swap(wi, win!(self, wi).focused_idx);
        }
    }

    /// Bring up a replacement L3 on pane `i`'s session and stage the swap
    /// (clearing any deferred-update flag).  Caller has checked it's a live,
    /// not-already-swapping L3 pane.
    fn begin_pane_swap(&mut self, wi: usize, i: usize) {
        let pane = &win!(self, wi).panes[i];
        let Some(sid) = pane.session().l3_session_id() else {
            return;
        };
        let (cols, rows) = (pane.session().grid().cols(), pane.session().grid().rows());
        match spawn_l3(cols, rows, sid, &self.event_tx) {
            Ok(spawn) => {
                win!(self, wi).panes[i].session_mut().begin_l3_swap(spawn);
                win!(self, wi).panes[i].set_update_pending(false);
                win!(self, wi).needs_render = true;
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
    /// Retire the session behind a pane: signal its L3, drop the
    /// registry dir + shm region, and forget the sid-keyed caches.
    ///
    /// Takes the id and kind by value rather than a `&Pane` so the
    /// caller does not hold a borrow of `windows` across the call —
    /// and so closing one pane and closing a whole window can share
    /// the teardown while keeping their own bookkeeping.
    ///
    /// RFC-003 step 3c: L3 panes own their PTY in-process, so the
    /// close is SIGTERM via entry.toml's pid + delete the registry dir
    /// (which also wipes the bytelog, so the slot cannot replay if a
    /// later L3 picks the same id).
    fn retire_pane_session(&mut self, id: u64, is_l3: bool) {
        if is_l3 {
            if let Ok(entry) = session_registry::read_session_entry(id) {
                unsafe { libc::kill(entry.pid, libc::SIGTERM) };
            }
            let _ = session_registry::delete_session(id);
            // Amendment 7 step 3: also drop the named shm region so
            // the kernel actually frees the pages once every fd-holder
            // closes.
            grid_shm::delete_region(&grid_shm::session_shm_name(id));
        }
        // F3+5 — drop cached cwd state so it can't leak past the pane.
        // The same id may eventually be reused; a fresh pane gets a
        // fresh refresh.
        self.pane_cwds.remove(&id);
        self.last_cwd_refresh.remove(&id);
        self.pane_badges.remove(&id);
        self.pane_titles.remove(&id);
        self.esc_history.remove(&id);
    }

    /// Take on a window the shell has just opened (or re-announced
    /// after a core swap).
    ///
    /// RFC-005: a fresh window starts as a 1×1 grid with one pane.
    /// The pane is spawned off-loop like `[+]` does — a window opening
    /// must not freeze the panes of the windows already up.
    fn adopt_window(&mut self, window_id: u32, w_phys: f64, h_phys: f64, scale: f64) {
        // RFC-005 step 6b — a queued record means L1 is reopening a
        // window from the last session, not making a new one.  The
        // window comes up with its saved grid and one "starting…"
        // placeholder per saved slot; the assembly worker replaces
        // them with the real panes.
        if let Some(record) = self.saved_windows.pop_front() {
            self.adopt_restored_window(window_id, record, w_phys, h_phys, scale);
            return;
        }
        let (cell_w, cell_h) = self.renderer.cell_dims();
        let cols = ((w_phys / cell_w) as u16).max(INITIAL_COLS);
        let rows = ((h_phys / cell_h) as u16).max(INITIAL_ROWS);
        let pane = match allocate_next_session_id() {
            Ok(id) => spawn_l3_pane_async(cols, rows, id, &self.event_tx),
            Err(e) => {
                lx_error!("core.window.session_id_failed", &format!("{e}"));
                return;
            }
        };
        let mut w = WindowState::new(
            window_id,
            vec![pane],
            0,
            1,
            1,
            w_phys,
            h_phys,
            scale,
        );
        // A brand-new window has never been painted.
        w.render.mark_bg_clear_required();
        self.windows.push(w);
        let wi = self.windows.len() - 1;
        // A freshly-opened window takes keyboard focus — that is a
        // statement about focus, not about rank: every other window
        // keeps pumping, painting and persisting exactly as before.
        self.key_window = wi;
        self.rebuild_layout(wi);
        lx_event!(
            "WINDOW_ADOPTED",
            "core took on a new window",
            window_id = window_id,
            windows = self.windows.len()
        );
        self.save_session_state();
    }

    /// RFC-005 step 6b — bring up a window from its saved record.
    ///
    /// Placeholders first, panes later: the window is on screen with
    /// its own grid before any reattach has been attempted, so the
    /// windows already up never stall on this one's I/O.
    fn adopt_restored_window(
        &mut self,
        window_id: u32,
        record: marspot::state::SavedWindowLayout,
        w_phys: f64,
        h_phys: f64,
        scale: f64,
    ) {
        let grid_cols = (record.grid_cols as usize).clamp(1, 6);
        let grid_rows = (record.grid_rows as usize).clamp(1, 6);
        // Cell dims for this window's own grid — a restored 3×3 window
        // must hand its L3s the size they will actually be shown at,
        // exactly as the boot path does.
        let (cell_w, cell_h) = self.renderer.cell_dims();
        let probe = Layout::build(
            w_phys,
            h_phys,
            0.0, // sidebar starts collapsed
            HEADER_PT * scale,
            CELL_TITLE_PT * scale,
            grid_cols,
            grid_rows,
            cell_w,
            cell_h,
        );
        let (cols, rows) = (probe.cells[0].cols, probe.cells[0].rows);
        let placeholders: Vec<Pane> = record
            .panes
            .iter()
            .map(|p| Pane::new_pending(p.sid, cols, rows))
            .collect();
        // A record with no panes would leave an empty window, which
        // several paths index into; one placeholder is the floor.
        let placeholders = if placeholders.is_empty() {
            vec![Pane::new_pending(0, cols, rows)]
        } else {
            placeholders
        };
        let focused_idx = (record.focused_idx as usize).min(placeholders.len() - 1);
        let mut w = WindowState::new(
            window_id,
            placeholders,
            focused_idx,
            grid_cols,
            grid_rows,
            w_phys,
            h_phys,
            scale,
        );
        w.render.mark_bg_clear_required();
        self.windows.push(w);
        let wi = self.windows.len() - 1;
        self.key_window = wi;
        self.rebuild_layout(wi);
        assemble_restore_window_async(window_id, record, cols, rows, &self.event_tx);
        lx_event!(
            "WINDOW_RESTORING",
            "saved window reopened; panes assembling off-loop",
            window_id = window_id,
            slots = win!(self, wi).panes.len(),
            windows = self.windows.len()
        );
    }

    /// A restore worker landed.  Swap the placeholders for the real
    /// panes; a window closed in the meantime drops them.
    fn adopt_restored_panes(&mut self, window_id: u32, panes: Vec<Pane>) {
        let Some(wi) = self.window_index(window_id) else {
            lx_warn!(
                "core.window.restore_window_gone",
                "window closed while its panes were assembling; dropping them",
                window_id = window_id,
                panes = panes.len()
            );
            return;
        };
        if panes.is_empty() {
            lx_warn!(
                "core.window.restore_empty",
                "assembly produced no panes; window keeps its placeholders",
                window_id = window_id
            );
            return;
        }
        let n = panes.len();
        win!(self, wi).panes = panes;
        win!(self, wi).focused_idx = win!(self, wi).focused_idx.min(n - 1);
        // Placeholder-era selection / title edit referred to panes that
        // no longer exist.
        win!(self, wi).selection = None;
        win!(self, wi).selection_dragging = false;
        win!(self, wi).editing_title = None;
        win!(self, wi).title_edit_buffer.clear();
        self.rebuild_layout(wi);
        self.save_session_state();
        lx_event!(
            "WINDOW_RESTORED",
            "assembled panes replaced the placeholders",
            window_id = window_id,
            panes = n
        );
    }

    /// `(window index, pane index)` of the pane holding this session.
    ///
    /// Sessions are window-agnostic: a session id says nothing about
    /// which window its pane currently lives in, and an off-loop
    /// result (spawn finished, control reconnected, search hits) has
    /// to find it wherever it is.  Searching only the key window meant
    /// those results were silently dropped whenever the user had
    /// focused a different window in the meantime.
    fn find_pane_by_sid(&self, sid: u64) -> Option<(usize, usize)> {
        self.windows.iter().enumerate().find_map(|(wi, w)| {
            w.panes
                .iter()
                .position(|p| p.shelld_session_id() == Some(sid))
                .map(|pi| (wi, pi))
        })
    }

    /// Total panes across every window — what the diagnostics mean by
    /// "how big is this session", now that panes live in more than one
    /// window.
    fn total_panes(&self) -> usize {
        self.windows.iter().map(|w| w.panes.len()).sum()
    }

    /// Repaint the window whose pane carries this session id.
    ///
    /// Plugin-driven state (badges, titles, PaneSession caps) is keyed
    /// by session, and a session says nothing about which window shows
    /// it.  Marking the key window instead would paint the badge into
    /// whichever window the user happened to be looking at — and leave
    /// the one that actually changed stale.
    fn mark_sid_window_dirty(&mut self, sid: u64) {
        if let Some((wi, _)) = self.find_pane_by_sid(sid) {
            win!(self, wi).needs_render = true;
        }
    }

    /// Index of the window carrying `window_id`, if the core has it.
    fn window_index(&self, window_id: u32) -> Option<usize> {
        self.windows.iter().position(|w| w.window_id == window_id)
    }

    /// L1 handed one window a freshly-created IOSurface pair (resize,
    /// core restart, pending-update spawn, or the window's birth).
    /// Swap that window's paint target, adopt the new dims, and
    /// re-lay it out.  Every other window is untouched.
    ///
    /// Returns the window index when the pair took, `None` when the
    /// window is gone or the surfaces couldn't be looked up — in the
    /// latter case the window keeps the pair it had rather than going
    /// black.
    fn attach_window_surfaces(
        &mut self,
        window_id: u32,
        front_id: u32,
        back_id: u32,
        w_phys: f64,
        h_phys: f64,
        scale: f64,
    ) -> Option<usize> {
        let wi = self.window_index(window_id)?;
        let Some(fresh) = WindowSurfaces::attach(front_id, back_id, self.renderer.device())
        else {
            lx_warn!(
                "core.attach.surface_lookup_nil",
                "IOSurfaceLookup returned nil; keeping the previous pair",
                window_id = window_id,
                front_id = front_id,
                back_id = back_id
            );
            return None;
        };
        if let Some(old) = win!(self, wi).surfaces.replace(fresh) {
            old.release();
        }
        win!(self, wi).w_phys = w_phys;
        win!(self, wi).h_phys = h_phys;
        win!(self, wi).scale = scale;
        self.rebuild_layout(wi);
        Some(wi)
    }

    /// Make `window_id` the key window.  Returns false (and changes
    /// nothing) when the core has no such window — a frame for a
    /// window that already closed, which is a normal race, not an
    /// error.
    fn focus_window(&mut self, window_id: u32) -> Option<usize> {
        let i = self.window_index(window_id)?;
        if self.key_window != i {
            let was = self.key_window;
            self.key_window = i;
            // The focus ring moves, so BOTH windows owe a frame — the
            // one that gained it and the one that lost it.
            win!(self, was).needs_render = true;
            win!(self, i).needs_render = true;
        }
        Some(i)
    }

    /// Which window a drag update or release belongs to: the one that
    /// took the press.  A drag that leaves its window still steers the
    /// selection it started, and a release delivered after focus moved
    /// still ends that drag rather than poking whatever is key now.
    /// Falls back to the window on the frame when no press is
    /// outstanding (a release with no drag, e.g. after a core swap).
    fn drag_target(&self, frame_window: u32) -> Option<usize> {
        self.drag_window
            .and_then(|w| self.window_index(w))
            .or_else(|| self.window_index(frame_window))
    }

    /// A window closed: retire every session it held and drop its
    /// `WindowState`.
    ///
    /// The last window is left alone — L1 owns app teardown
    /// (`close_requested` already SIGTERMs every session and exits),
    /// and tearing the state down here first would race it.
    fn close_window(&mut self, window_id: u32) {
        let Some(i) = self.window_index(window_id) else { return };
        if self.windows.len() <= 1 {
            lx_event!(
                "WINDOW_CLOSE_LAST",
                "last window closed — L1 drives app teardown",
                window_id = window_id
            );
            return;
        }
        let doomed: Vec<(u64, bool)> = self.windows[i]
            .panes
            .iter()
            .filter_map(|p| p.shelld_session_id().map(|id| (id, p.is_l3())))
            .collect();
        for (id, is_l3) in doomed {
            self.retire_pane_session(id, is_l3);
        }
        let gone = self.windows.remove(i);
        // Balance the `increment_use` from this window's last attach;
        // without it the IOSurface pair leaks for the life of the core.
        if let Some(s) = gone.surfaces.as_ref() {
            s.release();
        }
        self.key_window = self.key_window.min(self.windows.len() - 1);
        lx_event!(
            "WINDOW_CLOSED",
            "window and its panes retired",
            window_id = window_id,
            remaining = self.windows.len()
        );
    }

    fn close_session(&mut self, wi: usize, idx: usize) {
        if idx >= win!(self, wi).panes.len() {
            return;
        }
        if let Some(id) = win!(self, wi).panes[idx].shelld_session_id() {
            let is_l3 = win!(self, wi).panes[idx].is_l3();
            self.retire_pane_session(id, is_l3);
        }
        win!(self, wi).panes.remove(idx);
        if !win!(self, wi).panes.is_empty() {
            if win!(self, wi).focused_idx == idx {
                win!(self, wi).focused_idx = idx.min(win!(self, wi).panes.len() - 1);
            } else if win!(self, wi).focused_idx > idx {
                win!(self, wi).focused_idx -= 1;
            }
        } else {
            win!(self, wi).focused_idx = 0;
        }
        if let Some(sel) = win!(self, wi).selection {
            if sel.session_idx == idx {
                win!(self, wi).selection = None;
                win!(self, wi).selection_dragging = false;
            } else if sel.session_idx > idx {
                win!(self, wi).selection = Some(Selection {
                    session_idx: sel.session_idx - 1,
                    ..sel
                });
            }
        }
        match win!(self, wi).editing_title {
            Some(i) if i == idx => {
                win!(self, wi).editing_title = None;
                win!(self, wi).title_edit_buffer.clear();
            }
            Some(i) if i > idx => {
                win!(self, wi).editing_title = Some(i - 1);
            }
            _ => {}
        }
        self.save_session_state();
    }

    fn commit_title_edit(&mut self, wi: usize) {
        let w = &mut win!(self, wi);
        if let Some(idx) = w.editing_title.take() {
            let trimmed = w.title_edit_buffer.trim().to_string();
            if let Some(pane) = w.panes.get_mut(idx) {
                // RFC-003 Phase 6: titles survive an L2 swap via the
                // L3 process's persisted entry.toml (Amendment 7
                // reattach path).  The pane's `custom_title` is the
                // current truth.
                pane.custom_title =
                    if trimmed.is_empty() { None } else { Some(trimmed) };
            }
            w.title_edit_buffer.clear();
            self.save_session_state();
        }
    }

    fn cancel_title_edit(&mut self, wi: usize) {
        win!(self, wi).editing_title = None;
        win!(self, wi).title_edit_buffer.clear();
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
    fn handle_cmd_f(&mut self, wi: usize) -> bool {
        let idx = win!(self, wi).focused_idx;
        let Some(pane) = win!(self, wi).panes.get_mut(idx) else { return false };
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
                win!(self, wi).needs_render = true;
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
                win!(self, wi).needs_render = true;
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
        let Some((wi, pi)) = self.find_pane_by_sid(shelld_session_id) else { return };
        let Some(s) = win!(self, wi).panes[pi].search.as_mut() else { return };
        let changed = s.list.apply_results(query_id, hits, has_more);
        if changed {
            win!(self, wi).needs_render = true;
        }
    }

    /// Walk panes whose search is open + debounce_until is past;
    /// emit a fresh `SearchScrollback` frame on each.  Called from
    /// `pump_all` each loop iteration; cheap when no search is open
    /// (idle = single `is_none` check per pane).
    fn process_search_debounces(&mut self) {
        let now = std::time::Instant::now();
        for pane in self.windows.iter_mut().flat_map(|w| w.panes.iter_mut()) {
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
        &mut self, wi: usize,
        event: &MarspotKeyEvent,
        mods: Modifiers,
    ) -> bool {
        use marspot::input::{LogicalKey, NamedKey};
        let idx = win!(self, wi).focused_idx;
        // Decision: list vs bar.  Done in a scope so the mutable
        // borrow of `win!(self, wi).panes` ends before we call
        // `jump_to_focused_hit(idx)` (which needs a fresh &mut self).
        enum Outcome {
            NotOpen,
            Consumed,
            ConsumedJump,
            ConsumedClosed,
            Pass,
        }
        let outcome: Outcome = {
            let Some(pane) = win!(self, wi).panes.get_mut(idx) else { return false };
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
                win!(self, wi).needs_render = true;
                true
            }
            Outcome::ConsumedJump => {
                win!(self, wi).needs_render = true;
                self.jump_to_focused_hit(wi, idx);
                true
            }
        }
    }

    /// Realise a `JumpRequest::JumpToFocused`: compute view_offset +
    /// HighlightSpan from the focused hit's WireSearchHit, store on
    /// the pane, and forward GridScroll if needed.
    fn jump_to_focused_hit(&mut self, wi: usize, pane_idx: usize) {
        let Some(pane) = win!(self, wi).panes.get_mut(pane_idx) else { return };
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
        win!(self, wi).needs_render = true;
    }

    fn copy_selection_to_clipboard(&mut self, wi: usize) -> bool {
        let Some(sel) = win!(self, wi).selection else { return false };
        let idx = sel.session_idx;
        // L3 owns the real grid + scrollback; L2's mirror is a window-only
        // synthetic grid that `grid_selection_text` can't read back, so the
        // text round-trips through the session process.  In-process panes
        // read it locally.  The round-trip is made reliable by a per-request
        // sequence id (see `request_selection_text`) so a late reply from a
        // timed-out request can't alias the next copy.
        let is_l3 = win!(self, wi).panes.get(idx).is_some_and(|p| p.is_l3());
        let text = if is_l3 {
            let blockwise = sel.mode == marspot::ui::SelectionMode::Blockwise;
            win!(self, wi).panes
                .get_mut(idx)
                .and_then(|p| p.session_mut().request_selection_text(sel.anchor, sel.focus, blockwise))
        } else {
            win!(self, wi).panes.get(idx).and_then(|pane| selection_text(pane, &sel))
        };
        // cc-only post-processing: claudecode renders to a fixed inner
        // width with hard `\n` wraps that we don't want on the clipboard.
        // Detect cc panes via the L1 plugin badge — non-empty entry on
        // this pane's shelld_session_id means cc plugin tagged it.  See
        // `src/cc.rs`.
        let text = text.map(|t| {
            let is_cc = win!(self, wi)
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

    fn key(&mut self, wi: usize, event: MarspotKeyEvent, modifiers: Modifiers) {
        use marspot::input::{KeyState, LogicalKey, NamedKey};

        // F3+9 — Esc closes the context menu if open.  Swallowed so the
        // \e doesn't reach the focused pane.
        if event.state == KeyState::Pressed && win!(self, wi).context_menu.is_some() {
            if let LogicalKey::Named(NamedKey::Escape) = event.logical {
                win!(self, wi).context_menu = None;
                win!(self, wi).needs_render = true;
                return;
            }
        }

        // F3+1.5 — Process Monitor modal eats ESC + arrow keys + Cmd-W
        // when open (modal semantics).  Sits above every other key
        // path so a modal-active terminal still has working pane
        // keys after closing.
        if win!(self, wi).process_panel.is_some() && event.state == KeyState::Pressed {
            let is_esc = matches!(event.logical, LogicalKey::Named(NamedKey::Escape));
            let is_cmd_w = matches!(event.logical, LogicalKey::Char('w'))
                && modifiers.super_;
            if is_esc || is_cmd_w {
                win!(self, wi).process_panel = None;
                win!(self, wi).needs_render = true;
                return;
            }
        }
        // cc — Esc / Cmd-W closes the usage modal, same semantics.
        if win!(self, wi).cc_usage_modal.is_some() && event.state == KeyState::Pressed {
            let is_esc = matches!(event.logical, LogicalKey::Named(NamedKey::Escape));
            let is_cmd_w = matches!(event.logical, LogicalKey::Char('w'))
                && modifiers.super_;
            if is_esc || is_cmd_w {
                win!(self, wi).cc_usage_modal = None;
                win!(self, wi).needs_render = true;
                return;
            }
        }

        // cc — Cmd+Shift+C toggles the usage modal.
        //
        // Placement is load-bearing twice over.  It sits ahead of the
        // LOCK_KEYS routing below because this is a window-global
        // overlay, not pane content — it has to work even while a
        // plugin-held session (claudecode) owns the keyboard.  And it
        // sits ahead of the Cmd-C copy handler because that one tests
        // only `super_key()` and would happily swallow the shifted
        // chord as a copy.
        if event.state == KeyState::Pressed
            && modifiers.super_
            && modifiers.shift
            && matches!(event.logical, LogicalKey::Char(c) if c.eq_ignore_ascii_case(&'c'))
        {
            self.toggle_cc_usage_modal(wi);
            return;
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
            if let Some(active_sid) = self.focused_pane_active_session(wi) {
                let has_lock = self
                    .pane_session_for(active_sid)
                    .is_some_and(|s| s.has(marspot::shell_proto::PANE_SESSION_CAP_LOCK_KEYS));
                let is_esc = matches!(
                    event.logical,
                    LogicalKey::Named(NamedKey::Escape)
                );
                if is_esc {
                    let force_end = self
                        .note_escape_for_pane_session(active_sid, std::time::Instant::now());
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
            if self.handle_cmd_f(wi) {
                return;
            }
        }
        if event.state == KeyState::Pressed && self.search_consume_key(wi, &event, modifiers) {
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
            if self.copy_selection_to_clipboard(wi) {
                return;
            }
        }

        // Cmd-B: toggle the sidebar (VSCode / Cursor convention).
        if event.state == KeyState::Pressed
            && modifiers.super_key()
            && matches!(event.logical, LogicalKey::Char(c) if c.eq_ignore_ascii_case(&'b'))
        {
            win!(self, wi).sidebar_collapsed = !win!(self, wi).sidebar_collapsed;
            self.rebuild_layout(wi);
            return;
        }

        // Title-edit mode intercepts the keyboard before the PTY
        // mapper sees anything.  Enter commits, Esc cancels,
        // Backspace pops a char, printable text appends.
        if win!(self, wi).editing_title.is_some() {
            if event.state == KeyState::Pressed && !modifiers.super_key() {
                match &event.logical {
                    LogicalKey::Named(NamedKey::Enter) => {
                        self.commit_title_edit(wi);
                        win!(self, wi).needs_render = true;
                        return;
                    }
                    LogicalKey::Named(NamedKey::Escape) => {
                        self.cancel_title_edit(wi);
                        win!(self, wi).needs_render = true;
                        return;
                    }
                    LogicalKey::Named(NamedKey::Backspace) => {
                        win!(self, wi).title_edit_buffer.pop();
                        win!(self, wi).needs_render = true;
                        return;
                    }
                    _ => {
                        if let Some(t) = &event.text {
                            for ch in t.chars() {
                                if !ch.is_control() {
                                    win!(self, wi).title_edit_buffer.push(ch);
                                }
                            }
                            win!(self, wi).needs_render = true;
                            return;
                        }
                    }
                }
            }
            return;
        }

        let Some(pane) = win!(self, wi).try_focused_pane() else { return };

        // RFC-003 Phase 4 (frozen reattach minimum): an exited L3 pane
        // shows whatever was last published; on key press we revive it
        // by spawning a fresh L3 at the same session id.  The bytelog
        // opens in append mode so the new shell's output continues the
        // same on-disk record.  Only revive on a key press the user
        // would actually mean as "wake up" (any printable / Enter /
        // arrow / Tab etc); modifier-only key transitions don't fire.
        //
        // RFC-004 B.2 — vacant slots (boot-time assembly failures)
        // revive through the exact same path: they hold their sid and
        // report is_exited() = true.  A vacant slot with sid 0 (id
        // allocation itself failed at boot) allocates one now.
        if (pane.is_l3() || pane.is_vacant())
            && pane.is_exited()
            && event.state == KeyState::Pressed
        {
            let sid_opt = pane
                .session()
                .shelld_session_id()
                .filter(|&s| s != 0)
                .or_else(|| allocate_next_session_id().ok());
            if let Some(sid) = sid_opt {
                // Layout already sized the pane — keep its current
                // cell dims so the new L3 boots at the same shape.
                let (cols, rows) = win!(self, wi)
                    .layout
                    .cells
                    .get(win!(self, wi).focused_idx)
                    .map(|c| (c.cols, c.rows))
                    .unwrap_or((INITIAL_COLS, INITIAL_ROWS));
                // Off-loop, same as [+]: a keystroke into a dead slot
                // must not freeze the other fifteen panes while the
                // replacement boots.
                *win!(self, wi).focused_pane_mut() =
                    spawn_l3_pane_async(cols, rows, sid, &self.event_tx);
                win!(self, wi).needs_render = true;
                lx_event!(
                    "L3_REVIVING",
                    "user keystroke started an off-loop respawn at same id",
                    session_id = sid
                );
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
            if win!(self, wi).focused_pane_mut()
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
                    if win!(self, wi).focused_pane_mut().snap_to_live() {
                        win!(self, wi).needs_render = true;
                    }
                    win!(self, wi).focused_pane_mut().session_mut().forward_paste(&text);
                }
                return;
            }
            if win!(self, wi).focused_pane_mut().snap_to_live() {
                win!(self, wi).needs_render = true;
            }
            if win!(self, wi).selection.is_some() {
                win!(self, wi).selection = None;
                win!(self, wi).selection_dragging = false;
                win!(self, wi).needs_render = true;
            }
            win!(self, wi).focused_pane_mut()
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
            if win!(self, wi).focused_pane_mut().snap_to_live() {
                win!(self, wi).needs_render = true;
            }
            // Typing into the PTY clears any text selection.
            if win!(self, wi).selection.is_some() {
                win!(self, wi).selection = None;
                win!(self, wi).selection_dragging = false;
                win!(self, wi).needs_render = true;
            }
            let session = win!(self, wi).focused_pane_mut().session_mut();
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
                win!(self, wi).needs_render = true;
            }
            // F3+5 — Enter pressed → shell about to execute a line
            // (potentially `cd`).  Debounced refresh keeps a multi-
            // line paste collapsed to one syscall.  Carriage return
            // OR linefeed both count (modes may emit either).
            if bytes.iter().any(|&b| b == b'\r' || b == b'\n') {
                let focused = win!(self, wi).focused_idx;
                self.refresh_pane_cwd_for(wi, focused, false);
            }
        }
    }

    /// Hit-test an auto-detected link span on pane `idx`.  Scans the
    /// pane's visible grid (cheap — bounded by visible cells) and
    /// returns the first span containing the clicked (col, row).
    /// Returns None when the click missed every detected link.
    fn hit_test_pane_link(
        &self, wi: usize,
        idx: usize,
        col: u16,
        row: u16,
    ) -> Option<marspot::grid_links::LinkRange> {
        let pane = win!(self, wi).panes.get(idx)?;
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
    /// renderer-aware caller would otherwise repeat.  Every LinkKind
    /// is actionable (URL / File / Ip open, Email mails, Uuid
    /// copies), so a `Some` always warrants a menu.  Returns owned
    /// text + kind so the result can be stashed in
    /// `ContextMenuState.link` and survive the menu's
    /// clear-on-dispatch.
    fn hit_test_link_at_xy(
        &self, wi: usize,
        x_phys: f64,
        y_phys: f64,
    ) -> Option<LinkContext> {
        let (cw, ch) = self.renderer.cell_dims();
        let (idx, col, row) = win!(self, wi).layout.hit_test_cell_pos(x_phys, y_phys, cw, ch)?;
        let link = self.hit_test_pane_link(wi, idx, col, row)?;
        Some(LinkContext {
            text: link.text,
            kind: link.kind,
        })
    }

    /// Hit-test the right-side plugin badge's clickable prefix (text
    /// before the first space).  Returns the pane index when a click
    /// at (x_phys, y_phys) hits the underlined prefix; None
    /// otherwise.  Mirrors the geometry the renderer uses in
    /// `render_metal::build_instances` so a visual hit lines up with
    /// the logical one.
    fn hit_test_pane_badge_prefix(
        &self, wi: usize,
        x_phys: f64,
        y_phys: f64,
    ) -> Option<usize> {
        let (cell_w, _) = self.renderer.cell_dims();
        let cell_w = cell_w as f64;
        let padding = win!(self, wi).layout.padding;
        let title_h = win!(self, wi).layout.cell_title_h;
        let cell_count = win!(self, wi).layout.cells.len();
        for (i, p) in win!(self, wi).panes.iter().enumerate().take(cell_count) {
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
            let rect = &win!(self, wi).layout.cells[i];
            // Match the renderer's `reserved` carve-out for the
            // refresh affordance on the focused pane with a staged
            // update.
            let reserved = if p.update_pending() && i == win!(self, wi).focused_idx {
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

    /// cc — modal frame rect for the `Cc` usage modal.  Width scales
    /// with the account count (cards row); height with accounts
    /// (timeline rows).  Centered under the title strip.
    /// cc — open the Claude usage modal, or close it if it's already
    /// up.  Shared by the toolbar button and the Cmd+Shift+C binding
    /// so the two can't drift apart.  Opening re-reads the feed, so a
    /// stale panel is never what you get on a fresh open.
    fn toggle_cc_usage_modal(&mut self, wi: usize) {
        win!(self, wi).cc_usage_modal = match win!(self, wi).cc_usage_modal.take() {
            Some(_) => None,
            None => Some(CcUsageModalState {
                data: marspot::cc_usage::read(),
                loaded_at: Instant::now(),
            }),
        };
        win!(self, wi).needs_render = true;
    }

    fn cc_usage_modal_rect(&self, wi: usize) -> marspot_term::layout::Rect {
        let n = win!(self, wi)
            .cc_usage_modal
            .as_ref()
            .and_then(|m| m.data.as_ref())
            .map(|d| d.accounts.len())
            .unwrap_or(1);
        let (cell_w, cell_h) = self.renderer.cell_dims();
        // Geometry lives with the rest of the modal's metrics so the
        // painter and this rect can't disagree about how tall a card is.
        marspot::ui::components::cc_usage_modal::panel_rect(
            n,
            win!(self, wi).w_phys,
            win!(self, wi).h_phys,
            cell_w as f64,
            cell_h as f64,
            win!(self, wi).layout.top_inset,
        )
    }

    /// cc — build the `Cc` usage modal render data.  Re-reads the
    /// feed at most every 5 s while the modal is open.
    fn build_cc_usage_render(&mut self, wi: usize) -> Option<marspot::render_metal::CcUsageRender> {
        use marspot::render_metal::{CcUsageAccountRender, CcUsageRender};
        let modal = win!(self, wi).cc_usage_modal.as_mut()?;
        if modal.loaded_at.elapsed() > std::time::Duration::from_secs(5) {
            modal.data = marspot::cc_usage::read();
            modal.loaded_at = Instant::now();
            win!(self, wi).needs_render = true;
        }
        let rect = self.cc_usage_modal_rect(wi);
        let modal = win!(self, wi).cc_usage_modal.as_ref()?;
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let fmt_md_hm = |unix: i64| -> String {
            let (mo, d, h, mi) = marspot::cc_usage::local_mdhm(unix);
            format!("{mo}/{d} {h:02}:{mi:02}")
        };
        let fmt_hm = |unix: i64| -> String {
            let (_, _, h, mi) = marspot::cc_usage::local_mdhm(unix);
            format!("{h:02}:{mi:02}")
        };
        match modal.data.as_ref() {
            None => Some(CcUsageRender {
                rect,
                updated_label: String::new(),
                accounts: Vec::new(),
                now_unix,
                feed_missing: true,
            }),
            Some(d) => Some(CcUsageRender {
                rect,
                updated_label: format!("updated {}", fmt_md_hm(d.generated_at)),
                now_unix,
                feed_missing: false,
                accounts: d
                    .accounts
                    .iter()
                    .map(|a| CcUsageAccountRender {
                        name: a.name.clone(),
                        email: a.email.clone(),
                        status_label: marspot::cc_usage::CcStatusKind::classify(&a.status)
                            .label(&a.status),
                        status_severity: match marspot::cc_usage::CcStatusKind::classify(&a.status) {
                            marspot::cc_usage::CcStatusKind::Ok => 0,
                            marspot::cc_usage::CcStatusKind::Warn => 1,
                            _ => 2,
                        },
                        util_5h: a.util_5h as f32,
                        util_7d: a.util_7d as f32,
                        reset_5h_unix: a.reset_5h,
                        reset_7d_unix: a.reset_7d,
                        reset_label: format!("reset 5h: {}", fmt_md_hm(a.reset_5h)),
                        reset_5h_hm: fmt_hm(a.reset_5h),
                        reset_7d_hm: fmt_hm(a.reset_7d),
                    })
                    .collect(),
            }),
        }
    }

    /// F3+1.5 — build the centered Process Monitor modal data via the
    /// new UI component kit (ModalFrame, TrafficLights, TabStrip,
    /// ScrollView).  Returns render data + populates parallel hit-test
    /// state.  All rects are physical pixels.
    fn build_process_panel_render(&mut self, wi: usize) -> Option<marspot::render_metal::ProcessPanelRender> {
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
        let scale = win!(self, wi).scale.max(0.1);
        let hovered_title_bar = win!(self, wi)
            .process_panel
            .as_ref()
            .is_some_and(|p| p.title_bar_hovered);
        let panes_len;
        let selected_pane;
        let minimized;
        let maximized;
        let pos_offset;
        let scroll_y_in;
        {
            let panel = win!(self, wi).process_panel.as_mut()?;
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
            win!(self, wi).w_phys as f64,
            win!(self, wi).h_phys as f64,
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
                top_obstruction: win!(self, wi).layout.top_inset,
            },
        );
        let lights = TrafficLights::layout(
            frame.title_bar,
            marspot::ui::system::macos::traffic_lights::LIGHT_SIZE_LOGICAL * scale,
            marspot::ui::system::macos::traffic_lights::LIGHT_GAP_LOGICAL * scale,
            marspot::ui::system::macos::traffic_lights::LIGHT_LEFT_PAD_LOGICAL * scale,
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
            let panel_ref = win!(self, wi).process_panel.as_ref()?;
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
        // Mirror of the painter's geometry (render_metal's
        // `paint_process_panel_content`): same pad, same split.  The two
        // must stay in lockstep — this side places the kill buttons'
        // hit rects, the other draws them.
        let pad = marspot::render_metal::PROCESS_PANEL_SIDE_PAD_LOGICAL as f64 * scale;
        let content_x = frame.body.x + pad;
        let content_w = frame.body.w - pad * 2.0;
        let master_w = content_w * marspot::render_metal::PROCESS_PANEL_MASTER_FRAC;
        let master_rows_top = frame.body.y_top + pad + header_h;
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
        let detail_rows_top = frame.body.y_top + pad + header_h;
        let content_h = (detail_rows.len() as f64) * row_h;
        let detail_body = marspot_term::layout::Rect {
            x: content_x + master_w + pad * 0.5,
            y_top: detail_rows_top,
            w: content_w - master_w - pad * 0.5,
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
            let panel = win!(self, wi).process_panel.as_mut()?;
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
            light_rects: [lights.close, lights.min, lights.max],
            title_bar_hovered: hovered_title_bar,
            scroll_y: scroll_y_clamped,
            draw_backdrop: true,
        })
    }

    /// F3+1.3 — send SIGTERM to `pid` and track it for SIGKILL
    /// escalation 2 s later if it hasn't exited.  Logged at Info so
    /// post-mortem can correlate UI clicks with process deaths.
    fn kill_and_track(&mut self, wi: usize, pid: i32) {
        match marspot::pidtree::kill_pid(pid, libc::SIGTERM) {
            Ok(()) => {
                marspot::lx_event!(
                    "PROCESS_PANEL_SIGTERM",
                    "user clicked panel [×] — SIGTERM sent",
                    pid = pid
                );
                if let Some(panel) = win!(self, wi).process_panel.as_mut() {
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
    fn tick_process_panel_kills(&mut self, wi: usize) {
        let Some(panel) = win!(self, wi).process_panel.as_mut() else { return };
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
    fn refresh_process_panel(&mut self, wi: usize) {
        // F3+4 — walk libproc, sample per-pid stats, compute CPU%
        // deltas vs the previous refresh, build per-pane aggregates.
        // Two-pass: pass A snapshots procs + new stats; pass B walks
        // panes building summaries.  prev_pid_stats is rolled forward
        // (only pids seen this tick survive into next).
        let w = &mut win!(self, wi);
        let Some(panel) = w.process_panel.as_mut() else { return };
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
        for (pane_idx, pane) in w.panes.iter().enumerate() {
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
                let custom = w.panes.get(pane_idx)
                    .and_then(|p| p.custom_title.clone())
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

    fn mouse_moved(&mut self, wi: usize, x_phys: f64, y_phys: f64) {
        // Reveal the traffic-light glyphs while the cursor is anywhere
        // in the panel's title bar, matching the system's affordance.
        let w = &mut win!(self, wi);
        if let Some(panel) = w.process_panel.as_mut() {
            let now = panel.title_bar_rect.contains(x_phys, y_phys);
            if now != panel.title_bar_hovered {
                panel.title_bar_hovered = now;
                w.needs_render = true;
            }
        }
        // F3+9 — drive context menu hover highlight when open.
        if let Some(state) = w.context_menu.as_mut() {
            use marspot::ui::components::ContextMenu;
            let menu = ContextMenu::layout(
                w.layout.window_w, w.layout.window_h, w.scale,
                state.anchor_x, state.anchor_y,
                w.layout.top_inset,
                &state.items,
            );
            let new_hover = menu.hover_index(&state.items, x_phys, y_phys);
            if new_hover != state.hovered_idx {
                state.hovered_idx = new_hover;
                w.needs_render = true;
            }
        }

        let new_hover = if win!(self, wi).layout.hit_test_sidebar_button(x_phys, y_phys) {
            Some(ChromeBtn::Sidebar)
        } else if win!(self, wi).layout.hit_test_layout_button(x_phys, y_phys) {
            Some(ChromeBtn::Layout)
        } else if win!(self, wi).layout.hit_test_process_button(x_phys, y_phys) {
            Some(ChromeBtn::ProcessTree)
        } else if win!(self, wi).layout.hit_test_dev_panel_button(x_phys, y_phys) {
            Some(ChromeBtn::DevPanel)
        } else if win!(self, wi).layout.hit_test_cc_button(x_phys, y_phys) {
            Some(ChromeBtn::CcUsage)
        } else {
            None
        };
        if new_hover != win!(self, wi).hover_chrome_btn {
            win!(self, wi).hover_chrome_btn = new_hover;
            // Published in `render`, not here: the renderer is shared,
            // so pushing it at hit-test time drew window A's hover on
            // window B's toolbar the next time B painted.
            win!(self, wi).needs_render = true;
        }
    }

    fn mouse_down(&mut self, wi: usize, x_phys: f64, y_phys: f64, modifiers: Modifiers) {
        // F3+9 — when context menu is open, a left click first
        // dispatches an item / swallows on frame / closes-on-outside.
        if win!(self, wi).context_menu.is_some() {
            use marspot::ui::components::{ContextMenu, ContextMenuHit};
            let (hit, region) = {
                let state = win!(self, wi).context_menu.as_ref().unwrap();
                let menu = ContextMenu::layout(
                    win!(self, wi).layout.window_w, win!(self, wi).layout.window_h, win!(self, wi).scale,
                    state.anchor_x, state.anchor_y,
                    win!(self, wi).layout.top_inset,
                    &state.items,
                );
                (menu.hit_test(&state.items, x_phys, y_phys), state.region)
            };
            match hit {
                ContextMenuHit::Item(idx) => {
                    let tag = win!(self, wi).context_menu.as_ref().unwrap()
                        .items[idx].action_tag;
                    // Badge menus carry plugin-opaque tags — route the
                    // pick back to L1 instead of mapping through
                    // ContextMenuAction.
                    if let ContextRegion::PaneBadge(sid) = region {
                        self.pending_to_shell.push((
                            MsgType::PaneBadgeMenuAction,
                            marspot::shell_proto::encode_pane_badge_menu_action(
                                sid, tag,
                            ),
                        ));
                        win!(self, wi).context_menu = None;
                        win!(self, wi).needs_render = true;
                    } else if let Some(action) = ContextMenuAction::from_tag(tag) {
                        self.dispatch_context_action(wi, action, region);
                    } else {
                        win!(self, wi).context_menu = None;
                        win!(self, wi).needs_render = true;
                    }
                    return;
                }
                ContextMenuHit::Frame => {
                    // Click on divider / disabled row / padding —
                    // swallow, NSMenu-style.
                    return;
                }
                ContextMenuHit::Outside => {
                    win!(self, wi).context_menu = None;
                    win!(self, wi).needs_render = true;
                    // Fall through: click also triggers normal
                    // focus / selection behaviour.
                }
            }
        }

        let layout = &win!(self, wi).layout;
        let layout_btn_hit = layout.hit_test_layout_button(x_phys, y_phys);
        let sidebar_btn_hit = layout.hit_test_sidebar_button(x_phys, y_phys);
        let close_session_hit = layout.hit_test_close_session(x_phys, y_phys);
        let add_session_hit = layout.hit_test_add_session_button(x_phys, y_phys);
        let refresh_hit = layout.hit_test_cell_refresh(x_phys, y_phys);

        // Sidebar toggle: highest-priority chrome action so a click
        // on the chip never falls through to the cell underneath.
        if sidebar_btn_hit {
            win!(self, wi).sidebar_collapsed = !win!(self, wi).sidebar_collapsed;
            self.rebuild_layout(wi);
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
        if let Some(panel) = win!(self, wi).process_panel.as_ref() {
            // 1) Row kill
            let kill_hit: Option<i32> = panel
                .row_kill_rects
                .iter()
                .find(|(_, rect)| rect.contains(x_phys, y_phys))
                .map(|(pid, _)| *pid);
            if let Some(pid) = kill_hit {
                self.kill_and_track(wi, pid);
                self.refresh_process_panel(wi);
                win!(self, wi).needs_render = true;
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
                if let Some(p) = win!(self, wi).process_panel.as_mut() {
                    p.selected_pane = i;
                    p.scroll_y = 0.0;
                }
                win!(self, wi).needs_render = true;
                return;
            }
            // 3) Traffic lights
            if panel.close_btn_rect.contains(x_phys, y_phys) {
                win!(self, wi).process_panel = None;
                win!(self, wi).needs_render = true;
                return;
            }
            if panel.min_btn_rect.contains(x_phys, y_phys) {
                if let Some(p) = win!(self, wi).process_panel.as_mut() {
                    p.minimized = !p.minimized;
                    if p.minimized { p.maximized = false; }
                }
                win!(self, wi).needs_render = true;
                return;
            }
            if panel.max_btn_rect.contains(x_phys, y_phys) {
                if let Some(p) = win!(self, wi).process_panel.as_mut() {
                    p.maximized = !p.maximized;
                    if p.maximized { p.minimized = false; }
                }
                win!(self, wi).needs_render = true;
                return;
            }
            // 4) Title bar drag — anywhere in title bar that isn't a
            //    traffic light starts a window-drag.
            if panel.title_bar_rect.contains(x_phys, y_phys) {
                if let Some(p) = win!(self, wi).process_panel.as_mut() {
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
            win!(self, wi).process_panel = None;
            win!(self, wi).needs_render = true;
            return;
        }
        // F3+1 — process-tree panel toggle.  Same priority tier as
        // sidebar: a click on the icon never falls through.  Opening
        // forces an immediate libproc walk so the panel paints
        // populated on its first frame.
        if win!(self, wi).layout.hit_test_process_button(x_phys, y_phys) {
            if win!(self, wi).process_panel.is_some() {
                win!(self, wi).process_panel = None;
            } else {
                win!(self, wi).process_panel = Some(ProcessPanelState {
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
                    title_bar_hovered: false,
                    body_rect: marspot_term::layout::Rect::ZERO,
                    modal_rect: marspot_term::layout::Rect::ZERO,
                    minimized: false,
                    maximized: false,
                    scroll_y: 0.0,
                    content_h: 0.0,
                    pos_offset: (0.0, 0.0),
                    drag_grab: None,
                });
                self.refresh_process_panel(wi);
            }
            win!(self, wi).needs_render = true;
            return;
        }
        // UI-system dev panel toggle.  L2 doesn't own dev panel
        // visibility — L1 (marspot-shell) hosts the NSWindow.  Route
        // the click via `DevPanelToggle` wire frame; L1 flips state
        // + drives the AppKit show/hide on its main loop.
        if win!(self, wi).layout.hit_test_dev_panel_button(x_phys, y_phys) {
            self.pending_to_shell.push((MsgType::DevPanelToggle, Vec::new()));
            return;
        }
        // cc — toolbar `Cc` button toggles the Claude usage modal.
        if win!(self, wi).layout.hit_test_cc_button(x_phys, y_phys) {
            self.toggle_cc_usage_modal(wi);
            return;
        }
        // cc — while the usage modal is open, any click outside its
        // frame closes it; clicks inside are swallowed (display-only
        // modal, nothing interactive yet).
        if win!(self, wi).cc_usage_modal.is_some() {
            let rect = self.cc_usage_modal_rect(wi);
            if !rect.contains(x_phys, y_phys) {
                win!(self, wi).cc_usage_modal = None;
            }
            win!(self, wi).needs_render = true;
            return;
        }
        // F3+3.0 — when the LayoutModal is open, intercept ALL
        // clicks: hit-test its controls first, swallow non-control
        // clicks landing inside the frame so the modal feels modal
        // (doesn't punch through to the grid).
        if win!(self, wi).layout_modal_open {
            use marspot::ui::components::{LayoutModal, LayoutModalHit, GRID_MIN, GRID_MAX};
            let modal = LayoutModal::layout(
                win!(self, wi).w_phys, win!(self, wi).h_phys, win!(self, wi).scale,
                marspot::TITLE_STRIP_PT * win!(self, wi).scale,
                win!(self, wi).pending_grid_cols, win!(self, wi).pending_grid_rows,
            );
            // F3+3.3 — card drag start has priority over the
            // generic hit_test below (which would otherwise classify
            // a card click as `LayoutModalHit::Frame` and swallow it).
            if let Some(card_idx) = modal.hit_test_card(x_phys, y_phys) {
                let card = modal.cards[card_idx];
                win!(self, wi).layout_drag = Some(LayoutModalDrag {
                    from_slot: card_idx,
                    grab_offset_phys: (
                        x_phys - card.x,
                        y_phys - card.y_top,
                    ),
                    mouse_phys: (x_phys, y_phys),
                });
                win!(self, wi).needs_render = true;
                return;
            }
            match modal.hit_test(x_phys, y_phys) {
                Some(LayoutModalHit::Close) => {
                    win!(self, wi).layout_modal_open = false;
                    win!(self, wi).needs_render = true;
                    return;
                }
                Some(LayoutModalHit::Apply) => {
                    win!(self, wi).layout_modal_open = false;
                    win!(self, wi).grid_cols = win!(self, wi).pending_grid_cols;
                    win!(self, wi).grid_rows = win!(self, wi).pending_grid_rows;
                    // F3+3.3 — apply card_slots permutation to
                    // win!(self, wi).panes so the modal's drag-reordered
                    // arrangement lands in the actual grid.  Only
                    // the in-cells portion is reordered (panes past
                    // grid cells stay in sidebar order).  Identity
                    // mapping = no-op.
                    let cells = win!(self, wi).grid_cols * win!(self, wi).grid_rows;
                    self.apply_card_slot_permutation(wi, cells);
                    // shrink-guard — if focused pane slot is
                    // beyond the new cell count, jump focus to the
                    // last surviving cell so the user sees a focused
                    // pane in-grid.  Overflowed sessions stay alive
                    // in the sidebar (n_sessions > cells handling
                    // is already preserved by `take(cell_count)`).
                    if cells > 0 && win!(self, wi).focused_idx >= cells {
                        win!(self, wi).focused_idx = cells - 1;
                    }
                    self.rebuild_layout(wi);
                    self.save_session_state();
                    return;
                }
                Some(LayoutModalHit::ColsDec) => {
                    if win!(self, wi).pending_grid_cols > GRID_MIN {
                        win!(self, wi).pending_grid_cols -= 1;
                        self.reset_card_slots(wi);
                        win!(self, wi).needs_render = true;
                    }
                    return;
                }
                Some(LayoutModalHit::ColsInc) => {
                    if win!(self, wi).pending_grid_cols < GRID_MAX {
                        win!(self, wi).pending_grid_cols += 1;
                        self.reset_card_slots(wi);
                        win!(self, wi).needs_render = true;
                    }
                    return;
                }
                Some(LayoutModalHit::RowsDec) => {
                    if win!(self, wi).pending_grid_rows > GRID_MIN {
                        win!(self, wi).pending_grid_rows -= 1;
                        self.reset_card_slots(wi);
                        win!(self, wi).needs_render = true;
                    }
                    return;
                }
                Some(LayoutModalHit::RowsInc) => {
                    if win!(self, wi).pending_grid_rows < GRID_MAX {
                        win!(self, wi).pending_grid_rows += 1;
                        self.reset_card_slots(wi);
                        win!(self, wi).needs_render = true;
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
                    win!(self, wi).layout_modal_open = false;
                    win!(self, wi).needs_render = true;
                    return;
                }
            }
        }
        // F3+3.0 — toolbar layout button toggles the LayoutModal.
        // Re-init pending values from current grid_* every open so
        // the modal always starts in sync with the live grid.
        if layout_btn_hit {
            if !win!(self, wi).layout_modal_open {
                win!(self, wi).pending_grid_cols = win!(self, wi).grid_cols;
                win!(self, wi).pending_grid_rows = win!(self, wi).grid_rows;
                self.reset_card_slots(wi);
                // F3+3.6 — pull-fetch each pane's cwd on the
                // open transition so the modal preview + the
                // title-strip placeholder show fresh values.
                self.refresh_pane_cwds(wi);
            }
            win!(self, wi).layout_modal_open = !win!(self, wi).layout_modal_open;
            win!(self, wi).layout_drag = None;
            win!(self, wi).needs_render = true;
            return;
        }

        // Sidebar close-[×]: refuse to close the last session.
        if let Some(idx) = close_session_hit {
            if win!(self, wi).panes.len() > 1 && idx < win!(self, wi).panes.len() {
                self.close_session(wi, idx);
                self.rebuild_layout(wi);
            }
            return;
        }

        // Sidebar [+] add-session.
        if add_session_hit {
            if win!(self, wi).panes.len() < SESSION_COUNT_HARD_CAP {
                self.spawn_session(wi);
                self.rebuild_layout(wi);
            }
            return;
        }

        // Refresh affordance: click the deferred-update glyph on a pending
        // pane to trigger its silent swap now.  Sits inside the title strip,
        // so it must take priority over the title-edit hit below — but only
        // when that pane actually has an update staged (else fall through to
        // normal title behaviour).
        if let Some(i) = refresh_hit {
            if win!(self, wi).panes.get(i).is_some_and(|p| p.update_pending()) {
                self.begin_pane_swap(wi, i);
                return;
            }
        }

        // Plugin badge prefix click: route to L1 (the plugin owns
        // what the prefix means and what cycling it does).  Sits in
        // the same title strip as title-edit + refresh; check here
        // before title-edit so a click on `P<n>` doesn't drop the
        // pane into rename mode.
        if let Some(i) = self.hit_test_pane_badge_prefix(wi, x_phys, y_phys) {
            if let Some(sid) = win!(self, wi).panes.get(i).and_then(|p| p.shelld_session_id())
            {
                let payload = marspot::shell_proto::encode_pane_badge_clicked(sid);
                self.pending_to_shell
                    .push((MsgType::PaneBadgeClicked, payload));
                return;
            }
        }

        let layout = &win!(self, wi).layout;
        let row_phys = marspot::layout::SIDEBAR_ROW_H_PHYS;
        let top_pad_phys = layout.top_inset + layout.sidebar_top_pad_phys;
        let title_hit = layout.hit_test_cell_title(x_phys, y_phys);
        let sidebar_hit = layout.hit_test_sidebar_row(
            x_phys,
            y_phys,
            top_pad_phys,
            row_phys,
            win!(self, wi).panes.len(),
        );
        let cell_hit = layout.hit_test(x_phys, y_phys);
        let (cw, ch) = self.renderer.cell_dims();
        let cell_pos_hit = layout.hit_test_cell_pos(x_phys, y_phys, cw, ch);

        // Title-strip click → enter edit mode for that cell.
        if let Some(idx) = title_hit {
            if idx < win!(self, wi).panes.len() {
                self.commit_title_edit(wi);
                self.resolve_pending_on_defocus(wi, idx);
                win!(self, wi).focused_idx = idx;
                win!(self, wi).editing_title = Some(idx);
                win!(self, wi).title_edit_buffer = win!(self, wi).panes[idx]
                    .custom_title
                    .clone()
                    .unwrap_or_default();
                let _ = win!(self, wi).focused_pane_mut().snap_to_live();
                win!(self, wi).selection = None;
                win!(self, wi).selection_dragging = false;
                win!(self, wi).needs_render = true;
                return;
            }
        }

        // Click outside the title strip while editing commits first.
        if win!(self, wi).editing_title.is_some() {
            self.commit_title_edit(wi);
            win!(self, wi).needs_render = true;
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
        if let Some(link) = self.hit_test_link_at_xy(wi, x_phys, y_phys) {
            let items = self.build_link_menu_items(&link);
            if !items.is_empty() {
                let region = self.resolve_context_region(wi, x_phys, y_phys);
                win!(self, wi).context_menu = Some(ContextMenuState {
                    items,
                    anchor_x: x_phys,
                    anchor_y: y_phys,
                    region,
                    hovered_idx: None,
                    link: Some(link),
                });
                win!(self, wi).needs_render = true;
            }
            return;
        }

        // Click in cell body → start a fresh selection there AND
        // focus that cell.
        let prior_selection = win!(self, wi).selection;
        win!(self, wi).selection = None;
        win!(self, wi).selection_dragging = false;
        if let Some((idx, col, row)) = cell_pos_hit {
            let pane = &win!(self, wi).panes.get(idx);
            if let Some(pane) = pane {
                let rows = pane.session().grid().rows() as u32;
                let vo = pane.view_offset() as u32;
                let abs = vo + rows.saturating_sub(1).saturating_sub(row as u32);
                win!(self, wi).selection = Some(Selection {
                    session_idx: idx,
                    anchor: (col, abs),
                    focus: (col, abs),
                    mode: if modifiers.alt_key() {
                        SelectionMode::Blockwise
                    } else {
                        SelectionMode::Linewise
                    },
                });
                win!(self, wi).selection_dragging = true;
                if idx != win!(self, wi).focused_idx {
                    self.resolve_pending_on_defocus(wi, idx);
                    win!(self, wi).focused_idx = idx;
                }
                win!(self, wi).needs_render = true;
                return;
            }
        }
        if prior_selection.is_some() {
            win!(self, wi).needs_render = true;
        }

        let new_focus = sidebar_hit.or(cell_hit);
        if let Some(idx) = new_focus {
            // F3+3.0 — click on an empty cell (idx >= panes.len(),
            // which means the grid has more cells than sessions
            // after a layout grow) spawns a new session and focuses
            // it.  Matches the sidebar [+] behaviour but lands the
            // user directly in the cell they clicked, so growing
            // the grid + filling it reads as one motion.
            if idx >= win!(self, wi).panes.len() && cell_hit.is_some() {
                if win!(self, wi).panes.len() < SESSION_COUNT_HARD_CAP {
                    self.spawn_session(wi);
                    win!(self, wi).focused_idx = win!(self, wi).panes.len() - 1;
                    self.rebuild_layout(wi);
                }
                return;
            }
            if idx < win!(self, wi).panes.len() && idx != win!(self, wi).focused_idx {
                self.resolve_pending_on_defocus(wi, idx);
                win!(self, wi).focused_idx = idx;
                let _ = win!(self, wi).focused_pane_mut().snap_to_live();
                // F3+5 — focus change = "user is looking at this pane
                // right now"; refresh its cwd so the title strip stays
                // current.  Debounced per-sid (cheap when same pane is
                // focused twice in a row).
                self.refresh_pane_cwd_for(wi, idx, false);
                win!(self, wi).needs_render = true;
            }
        }
    }

    /// Finder file drop: insert the shell-quoted path(s) into the
    /// pane under the drop point — the "type the path for me" gesture
    /// every macOS terminal supports.  Routing goes through the
    /// existing Paste path (L3 wraps in bracketed-paste when the app
    /// enabled the mode), so shells insert at the prompt and TUI apps
    /// like claudecode see a normal paste in their input box.
    fn file_drop(&mut self, wi: usize, x_phys: f64, y_phys: f64, paths: &[String]) {
        if paths.is_empty() {
            return;
        }
        // Target the pane under the drop point (body cells or its
        // sidebar row); a drop on chrome/padding goes to the focused
        // pane — dropping "at the terminal" should never be a no-op.
        let row_phys = marspot_term::layout::SIDEBAR_ROW_H_PHYS;
        let top_pad_phys = win!(self, wi).layout.top_inset + win!(self, wi).layout.sidebar_top_pad_phys;
        let sidebar_hit = win!(self, wi).layout.hit_test_sidebar_row(
            x_phys, y_phys, top_pad_phys, row_phys, win!(self, wi).panes.len(),
        );
        let idx = sidebar_hit
            .or(win!(self, wi).layout.hit_test(x_phys, y_phys))
            .filter(|i| *i < win!(self, wi).panes.len())
            .unwrap_or(win!(self, wi).focused_idx);
        if idx >= win!(self, wi).panes.len() {
            return;
        }
        // Same focus motion as a click — the pane receiving the text
        // becomes the pane the user is typing into next.
        if idx != win!(self, wi).focused_idx {
            self.resolve_pending_on_defocus(wi, idx);
            win!(self, wi).focused_idx = idx;
            self.refresh_pane_cwd_for(wi, idx, false);
        }
        if win!(self, wi).panes[idx].snap_to_live() {
            win!(self, wi).needs_render = true;
        }
        // Trailing space after each path so the user can keep typing
        // (and multiple files arrive space-separated) — matches the
        // Finder → Terminal.app / iTerm2 convention.
        let mut text = String::new();
        for p in paths {
            text.push_str(&marspot_term::input_core::shell_quote_path(p));
            text.push(' ');
        }
        win!(self, wi).panes[idx].session_mut().forward_paste(&text);
        win!(self, wi).needs_render = true;
    }

    fn mouse_drag(&mut self, wi: usize, x_phys: f64, y_phys: f64) {
        // F3+3.3 — LayoutModal card drag.  Take priority over the
        // process panel drag so a layout modal session never gets
        // captured by chrome elsewhere.
        if let Some(d) = win!(self, wi).layout_drag.as_mut() {
            d.mouse_phys = (x_phys, y_phys);
            win!(self, wi).needs_render = true;
            return;
        }
        // F3+1.5 — modal title bar drag.  Snapshot at mouse_down
        // (drag_grab = Some((grab_x, grab_y, grab_off_x, grab_off_y)))
        // → motion delta translates to pos_offset diff.
        if let Some(panel) = win!(self, wi).process_panel.as_mut() {
            if let Some((gx, gy, gox, goy)) = panel.drag_grab {
                panel.pos_offset = (gox + (x_phys - gx), goy + (y_phys - gy));
                win!(self, wi).needs_render = true;
                return;
            }
        }
        if !win!(self, wi).selection_dragging {
            return;
        }
        let (cw, ch) = self.renderer.cell_dims();
        let target_idx = match win!(self, wi).selection.as_ref() {
            Some(s) => s.session_idx,
            None => return,
        };
        let cell = match win!(self, wi).layout.cells.get(target_idx) {
            Some(c) => c.clone(),
            None => return,
        };
        let inner_x = cell.x + win!(self, wi).layout.padding;
        let inner_y = cell.y_top + win!(self, wi).layout.cell_title_h + win!(self, wi).layout.padding;

        // Past-edge auto-scroll, NSTextView-style (see src/main.rs
        // for the rate-limit rationale).
        let max_row = cell.rows.saturating_sub(1) as i64;
        let raw_row = ((y_phys - inner_y) / ch).floor() as i64;
        if raw_row < 0 {
            win!(self, wi).panes[target_idx].apply_scroll_lines(1);
        } else if raw_row > max_row {
            win!(self, wi).panes[target_idx].apply_scroll_lines(-1);
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
        let pane = &win!(self, wi).panes[target_idx];
        let rows = pane.session().grid().rows() as u32;
        let vo = pane.view_offset() as u32;
        let abs = vo + rows.saturating_sub(1).saturating_sub(row as u32);

        let Some(sel) = win!(self, wi).selection.as_mut() else { return };
        sel.focus = (col as u16, abs);
        win!(self, wi).needs_render = true;
    }

    fn end_modal_drag(&mut self, wi: usize) {
        if let Some(panel) = win!(self, wi).process_panel.as_mut() {
            panel.drag_grab = None;
        }
    }

    fn mouse_up(&mut self, wi: usize) {
        // F3+3.3 — finalize LayoutModal card drag: pick the
        // destination slot under the cursor, swap, redraw.  No
        // animation (V2.0); settle = single-frame jump.
        if let Some(d) = win!(self, wi).layout_drag.take() {
            use marspot::ui::components::LayoutModal;
            let modal = LayoutModal::layout(
                win!(self, wi).w_phys, win!(self, wi).h_phys, win!(self, wi).scale,
                marspot::TITLE_STRIP_PT * win!(self, wi).scale,
                win!(self, wi).pending_grid_cols, win!(self, wi).pending_grid_rows,
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
                    && to_slot < win!(self, wi).card_slots.len()
                    && d.from_slot < win!(self, wi).card_slots.len()
                {
                    win!(self, wi).card_slots.swap(d.from_slot, to_slot);
                }
            }
            win!(self, wi).needs_render = true;
            // Don't continue into selection / process-panel paths.
            return;
        }
        self.end_modal_drag(wi);
        // A click without movement leaves anchor == focus → treat as
        // "no selection" so a stray single-click doesn't ghost a
        // single-cell highlight.
        if win!(self, wi).selection_dragging {
            win!(self, wi).selection_dragging = false;
            if let Some(sel) = win!(self, wi).selection {
                if sel.anchor == sel.focus {
                    win!(self, wi).selection = None;
                }
            }
        }
    }

    fn scroll(&mut self, wi: usize, dy_phys: f64, precise: bool) {
        // F3+1.5 — when the Process Monitor modal is open, the wheel
        // belongs to it (assuming the cursor is over the modal — and
        // since the modal swallows clicks anyway, treating ALL scroll
        // as modal scroll while it's open is the simpler, more
        // predictable mapping).
        if let Some(panel) = win!(self, wi).process_panel.as_mut() {
            if !panel.minimized {
                let _ = precise;
                panel.scroll_y += dy_phys;
                // Clamp using last-frame content_h.
                let max = (panel.content_h - panel.body_rect.h).max(0.0);
                if panel.scroll_y < 0.0 { panel.scroll_y = 0.0; }
                if panel.scroll_y > max { panel.scroll_y = max; }
                win!(self, wi).needs_render = true;
            }
            return;
        }
        let (_, cell_h) = self.renderer.cell_dims();
        let lines = scroll_lines(dy_phys, precise, cell_h);
        if lines == 0 {
            return;
        }
        let idx = win!(self, wi).focused_idx;
        // A mouse-tracking TUI (claudecode etc.) owns its own scroll: the
        // wheel is injected to the app, which redraws the whole screen in
        // place — marspot only ever sees the new grid and cannot re-anchor
        // an existing selection to it (there is no marspot scrollback for
        // these panes; `scrollback_len` stays 0). So a selection left in
        // place after such a scroll highlights *different* bytes than the
        // user drew it over — the "选区一滚就变" report. Clear it.
        //
        // This is deliberately narrower than "clear on any grid change":
        // a claudecode pane redraws autonomously (spinner) several times a
        // second, and the selection MUST survive that so the user can
        // Cmd-C what they picked. Only a user-initiated scroll of a
        // mouse-tracking pane clears — a real scroll can't keep a valid
        // selection, autonomous output can.
        let tui_scroll = win!(self, wi).panes[idx].session().is_l3()
            && win!(self, wi).panes[idx].session().l3_mouse_tracking_active();
        if win!(self, wi).panes[idx].apply_scroll_lines(lines) {
            if tui_scroll {
                if let Some(sel) = win!(self, wi).selection {
                    if sel.session_idx == idx {
                        win!(self, wi).selection = None;
                        win!(self, wi).selection_dragging = false;
                    }
                }
            }
            win!(self, wi).needs_render = true;
        }
    }

    fn preedit(&mut self, wi: usize, text: String) {
        if win!(self, wi).ime_preedit != text {
            win!(self, wi).ime_preedit = text;
            win!(self, wi).needs_render = true;
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
        // Snapshot which session ids are frozen by an L1 PaneSession:
        // the loop holds `&mut` on the window's panes, and reading the
        // map through `self` inside it would overlap that borrow.
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
        // Every window's panes, not just the key window's.  A pane
        // does not stop being live because its window lost focus —
        // windows are peers, and a pane skipped here would sit on
        // unread PTY bytes until its window happened to become key.
        for wi in 0..self.windows.len() {
            let mut window_total = 0usize;
            let w = &mut win!(self, wi);
            for (i, p) in w.panes.iter_mut().enumerate() {
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
                window_total += n;
                let pushed = p.drain_scroll_push_delta();
                // Auto-pin the viewport when a row scrolled into
                // scrollback while the user is reading history.  L3 runs
                // the symmetric bump in its publish loop so view_offset
                // stays consistent across the L2↔L3 boundary without a
                // round-trip.  Without this, every line the shell emits
                // while the user is scrolled back slides the visible
                // content downward by one row — the "老内容被新内容覆盖"
                // symptom.
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
                if let Some(sel) = w.selection.as_mut() {
                    if sel.session_idx == i && pushed > 0 {
                        let bump = pushed as u32;
                        sel.anchor.1 = sel.anchor.1.saturating_add(bump);
                        sel.focus.1 = sel.focus.1.saturating_add(bump);
                    }
                }
            }
            // Only the window that actually took bytes owes a repaint.
            if window_total > 0 {
                w.needs_render = true;
            }
            total += window_total;
        }
        // The app exits when every pane of every window has exited —
        // one window still holding a live shell keeps marspot up.
        let has_panes = self.windows.iter().any(|w| !w.panes.is_empty());
        let all_dead = self
            .windows
            .iter()
            .flat_map(|w| w.panes.iter())
            .all(|p| p.is_exited());
        if has_panes && all_dead {
            for p in self.windows.iter_mut().flat_map(|w| w.panes.iter_mut()) {
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
        wi: usize,
        target_tex: &objc2::rc::Retained<ProtocolObject<dyn MTLTexture>>,
    ) -> Option<(f64, f64, f64, f64)> {
        let focused = win!(self, wi).focused_idx;
        // Everything below borrows the window — the pane grids, the
        // preedit string, the layout — while the renderer wants `&mut`
        // on that same window's render state.  They are disjoint
        // fields, but the borrow checker cannot see through the
        // `windows[key_window]` index, so move the render state out
        // for the duration of the frame and put it back at the end.
        let mut wr = std::mem::take(&mut win!(self, wi).render);

        let labels: Vec<String> = (1..=win!(self, wi).panes.len()).map(|n| n.to_string()).collect();
        let states: Vec<SessionState> =
            win!(self, wi).panes.iter().map(|p| p.session().state()).collect();

        // F3+5 — title placeholder = basename of the cwd cached in
        // `pane_cwds`.  Population strategy is hybrid passive:
        // (1) pane spawn, (2) focus change, (3) Enter key in focused
        // pane, (4) LayoutModal open (all panes), (5) `lazy_fill_missing_cwds`
        // here at the top of build_views as a tail-of-conditions
        // fallback — if anything else missed it, this catches it on
        // the first paint.  Cost: HashMap.contains_key per pane (no
        // syscall) on the steady-state hot path; one proc_pidinfo
        // syscall only on a miss.
        self.lazy_fill_missing_cwds(wi);
        let cwd_basenames: Vec<Option<&str>> = (0..win!(self, wi).panes.len())
            .map(|i| {
                let sid = win!(self, wi).panes[i].shelld_session_id()?;
                let path = self.pane_cwds.get(&sid)?;
                std::path::Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
            })
            .collect();

        // Resolved label per cell: edit-mode buffer → user-set custom
        // title → plugin-set title (MsgType::PaneTitle, cc/...) →
        // cwd basename (dynamic placeholder) → ordinal fallback.
        let resolved_labels: Vec<String> = (0..win!(self, wi).panes.len())
            .map(|i| {
                if win!(self, wi).editing_title == Some(i) {
                    win!(self, wi).title_edit_buffer.clone()
                } else if let Some(custom) = win!(self, wi).panes[i].custom_title.as_ref() {
                    custom.clone()
                } else if let Some(plugin_title) = win!(self, wi).panes[i]
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
        let titles: Vec<String> = (0..win!(self, wi).panes.len())
            .map(|i| {
                let mut s = resolved_labels.get(i).cloned().unwrap_or_default();
                if win!(self, wi).editing_title == Some(i) {
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
        // borrow `win!(self, wi).panes` into `views`.  Renderer holds the
        // panel data via `set_process_panel`, freeing `&self` for
        // the render call below.
        let panel_data = self.build_process_panel_render(wi);
        self.renderer.set_process_panel(panel_data);
        // cc — publish `Cc` usage modal render state (refreshing the
        // feed at most every 5 s while open; zero I/O when closed).
        let cc_data = self.build_cc_usage_render(wi);
        self.renderer.set_cc_usage(cc_data);
        // F3+9 — publish ContextMenu render state every frame.
        // Dev panel renders into its own NSWindow, owned by L1
        // (marspot-shell), not by L2.  L2's only job re: dev panel
        // is to (a) hit-test the toolbar toggle icon and (b) emit
        // a `DevPanelToggle` wire frame on click; L1 takes it from
        // there.  No call here.
        self.renderer.set_dev_panel(None);
        // Per-window, like every overlay below it: the renderer holds
        // one copy and each window sets its own right before painting.
        self.renderer
            .set_hover_chrome_btn(map_hover_to_u8(win!(self, wi).hover_chrome_btn));

        self.renderer.set_context_menu(win!(self, wi).context_menu.as_ref().map(|state| {
            use marspot::render_metal::{ContextMenuRender, ContextMenuRow};
            ContextMenuRender {
                scale: win!(self, wi).scale,
                anchor_phys: (state.anchor_x, state.anchor_y),
                top_inset: win!(self, wi).layout.top_inset,
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
        self.renderer.set_layout_modal(if win!(self, wi).layout_modal_open {
            use marspot::render_metal::{LayoutModalRender, LayoutModalDragRender};
            let cells = win!(self, wi).pending_grid_cols * win!(self, wi).pending_grid_rows;
            let slot_titles: Vec<String> = (0..cells)
                .map(|slot| {
                    let pane_idx = win!(self, wi).card_slots.get(slot).copied().unwrap_or(usize::MAX);
                    resolved_labels
                        .get(pane_idx)
                        .cloned()
                        .unwrap_or_default()
                })
                .collect();
            Some(LayoutModalRender {
                cols: win!(self, wi).pending_grid_cols,
                rows: win!(self, wi).pending_grid_rows,
                scale: win!(self, wi).scale,
                slot_titles,
                drag: win!(self, wi).layout_drag.map(|d| LayoutModalDragRender {
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
        let cell_count = win!(self, wi).layout.cells.len();
        let views: Vec<SessionView> = win!(self, wi)
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
                    v.ime_preedit = win!(self, wi).ime_preedit.as_str();
                }
                v.selection = win!(self, wi)
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
        self.renderer.render_layout_to_texture(
            &mut wr,
            target_tex,
            &win!(self, wi).layout,
            &views,
            &entries,
            focused,
        );
        win!(self, wi).render = wr;
        win!(self, wi).needs_render = false;
        // One INFO per window, the first time it paints.  Emitted here
        // rather than in the render pass because the attach branch
        // renders too — and that is precisely how the boot window gets
        // its first frame after a core swap, so a check placed in the
        // pass alone reports a perfectly healthy window as frozen.
        if !win!(self, wi).painted_once {
            win!(self, wi).painted_once = true;
            let surface_id = win!(self, wi)
                .surfaces
                .as_ref()
                .map(|s| s.writing_surface_id())
                .unwrap_or(0);
            lx_event!(
                "WINDOW_FIRST_FRAME",
                "window painted its first frame",
                window_id = win!(self, wi).window_id,
                surface_id = surface_id,
                panes = win!(self, wi).panes.len()
            );
        }

        win!(self, wi).panes.get(focused).and_then(|pane| {
            if !pane.session().cursor_visible() {
                return None;
            }
            let (col, row) = pane.session().grid().cursor();
            win!(self, wi).layout
                .caret_view_phys_rect(focused, col, row, cell_w, cell_h)
        })
    }
}

/// RFC-004 B.1 — identity-keyed boot assembly.  Extracted from
/// `main()` so the slot semantics (order preserved, never
/// compacted, sid-stable, vacant on failure) are testable with a
/// sandbox state dir + real L3 binaries (`MARSPOT_SESSION_BIN`).
/// Returns the assembled panes (one per slot + adopted orphans)
/// and the reattached session ids (execv fanout drives off them).
fn assemble_panes_at_boot(
    saved_state: Option<&marspot::state::SavedWindowLayout>,
    n_sessions: usize,
    boot_cols: u16,
    boot_rows: u16,
    event_tx: &Sender<CoreEvent>,
    sweeps_registry: bool,
    reserved_sids: &std::collections::HashSet<u64>,
) -> (Vec<Pane>, Vec<u64>) {
    // entry.toml titles, collected during the registry scan below.
    // Last resort of the title fallback chain at the end of assembly.
    let mut session_titles: std::collections::HashMap<u64, String> =
        std::collections::HashMap::new();
    let mut panes: Vec<Pane> = Vec::with_capacity(n_sessions);
    // RFC-004 B.1 — identity-keyed slot assembly.  Every saved
    // slot is processed IN ORDER and ALWAYS yields a pane:
    //
    //   sid alive (identity-verified) → reattach; reattach failure
    //     downgrades to resurrection (never delete)
    //   sid dead + dir on disk       → resurrect same id (L3 cold
    //     boot applies state.bin + reopens scrollback.bin)
    //   sid without dir / sid == 0   → fresh spawn (same sid when
    //     the slot names one, so identity survives a lost dir)
    //   any spawn failure            → vacant placeholder pane
    //     carrying the sid (revive-on-keystroke retries it)
    //
    // The old assembly compacted failed slots away (panes shifted,
    // titles/cwd mis-bound by index, and the first
    // save_session_state locked the damage in) and resurrected
    // dead ids in readdir order (错位 after a full crash).
    let raw_list = list_session_entries();
    for e in &raw_list {
        if !e.title.is_empty() {
            session_titles.insert(e.id, e.title.clone());
        }
    }
    let entry_by_id: std::collections::HashMap<
        u64,
        &marspot_term::session_registry::SessionEntry,
    > = raw_list.iter().map(|e| (e.id, e)).collect();
    // RFC-004 A.2 — liveness = pid signalable AND its executable
    // is a marspot-session image.  A recycled pid can no longer
    // impersonate a live session (nor eat a SIGKILL meant for one).
    let alive_ids: std::collections::HashSet<u64> = raw_list
        .iter()
        .filter(|e| session_registry::pid_is_live_session(e.pid))
        .map(|e| e.id)
        .collect();
    let dir_ids: std::collections::HashSet<u64> =
        raw_list.iter().map(|e| e.id).collect();

    // Slot specs come from the saved layout; without one, adopt
    // whatever the registry holds (numeric order), padded with
    // anonymous slots up to the grid size.
    struct SlotSpec {
        sid: u64,
        cwd: String,
    }
    let slot_specs: Vec<SlotSpec> = match saved_state {
        Some(s) => s
            .panes
            .iter()
            .take(n_sessions)
            .map(|p| SlotSpec { sid: p.sid, cwd: p.last_cwd.clone() })
            .collect(),
        None => {
            let mut ids: Vec<u64> = dir_ids.iter().copied().collect();
            ids.sort();
            ids.truncate(n_sessions);
            let mut specs: Vec<SlotSpec> = ids
                .into_iter()
                .map(|sid| SlotSpec { sid, cwd: String::new() })
                .collect();
            while specs.len() < n_sessions {
                specs.push(SlotSpec { sid: 0, cwd: String::new() });
            }
            specs
        }
    };

    let mut claimed: std::collections::HashSet<u64> =
        std::collections::HashSet::new();
    let mut reattached_ids: Vec<u64> = Vec::new();
    // Fresh-id allocation that can't collide with a claimed sid:
    // the allocator floors at max(existing dir id) (A.1), but a
    // claimed sid whose dir hasn't been recreated yet is invisible
    // to that floor — skip by hand.  64 tries is unreachable in
    // practice (each miss burns one monotonic id).
    let allocate_fresh = |claimed: &std::collections::HashSet<u64>| -> Option<u64> {
        for _ in 0..64 {
            match allocate_next_session_id() {
                Ok(id) if !claimed.contains(&id) => return Some(id),
                Ok(_) => continue,
                Err(e) => {
                    lx_error!(
                        "core.session_registry.allocate_failed",
                        &format!("{e}")
                    );
                    return None;
                }
            }
        }
        None
    };

    for spec in &slot_specs {
        let mut sid = spec.sid;
        if sid != 0 && claimed.contains(&sid) {
            // Duplicate sid in saved state (corrupt / hand-edited)
            // — assemble as an anonymous slot instead of
            // double-binding one session to two panes.
            sid = 0;
        }
        // 1) Live session → reattach in place.
        if sid != 0 && alive_ids.contains(&sid) {
            match reattach_l3_pane(sid, &event_tx) {
                Ok(pane) => {
                    panes.push(pane);
                    claimed.insert(sid);
                    reattached_ids.push(sid);
                    continue;
                }
                Err(e) => {
                    // Live-but-unreachable (shm wiped / UDS dead).
                    // Identity is verified (A.2) so this SIGKILL
                    // cannot hit a foreign process.  The dir is
                    // KEPT and we fall through to resurrection —
                    // the old path delete_session'd right here.
                    lx_warn!(
                        "core.reattach.l3_failed",
                        &format!("{e} — SIGKILL verified L3, resurrect in place"),
                        session = sid
                    );
                    if let Some(entry) = entry_by_id.get(&sid) {
                        unsafe { libc::kill(entry.pid, libc::SIGKILL) };
                    }
                }
            }
        }
        // 2) Dead (or just-killed) with a dir → resurrect same id.
        // 3) No dir → fresh spawn, keeping the slot's sid.
        let spawn_sid = if sid != 0 {
            sid
        } else {
            match allocate_fresh(&claimed) {
                Some(id) => id,
                None => {
                    panes.push(Pane::new_vacant(0, boot_cols, boot_rows));
                    continue;
                }
            }
        };
        // Stale shm from a previous life can't be reattached —
        // tear it down so the fresh L3 publishes a new region.
        if let Some(entry) = entry_by_id.get(&spawn_sid) {
            if !entry.shm_name.is_empty() {
                if let Ok(c) = std::ffi::CString::new(entry.shm_name.clone()) {
                    grid_shm::delete_region(&c);
                }
            }
        }
        // RFC-004 C.2 belt+braces — a resurrect dir still holds the
        // DEAD process's entry.toml; `wait_for_entry` would read it
        // instantly and start connecting before the fresh child has
        // even bound.  Unlink the stale entry + sock (KEEPING
        // scrollback/bytelog/state.bin) so the post-spawn wait
        // synchronises on the fresh child's own registry write.
        let _ = std::fs::remove_file(
            marspot_term::session_registry::session_entry_path(spawn_sid),
        );
        marspot_term::session_registry::cleanup_stale_socket(spawn_sid);
        match spawn_l3_pane_with_cwd(
            boot_cols, boot_rows, spawn_sid, &spec.cwd, &event_tx,
        ) {
            Ok(pane) => {
                panes.push(pane);
                claimed.insert(spawn_sid);
            }
            Err(e) => {
                lx_error!(
                    "core.spawn.l3_boot_failed",
                    &format!("{e} — slot held vacant, revive retries this sid"),
                    session = spawn_sid
                );
                claimed.insert(spawn_sid);
                panes.push(Pane::new_vacant(spawn_sid, boot_cols, boot_rows));
            }
        }
    }

    // Live sessions no slot claimed = real user work whose slot
    // record was lost — append as extra panes rather than kill
    // (the old path SIGKILLed + deleted these when the layout
    // shrank).  Over the hard cap: stop the process but RETIRE
    // the dir (recoverable) instead of deleting.
    //
    // Only the sweeping assembly does this.  RFC-005 step 6b restores
    // the other windows through this same function, and a restored
    // window knows only its own saved sids: were it to sweep, it would
    // adopt the boot window's panes a second time and retire the dirs
    // of every session it simply never heard of.
    for id in raw_list.iter().map(|e| e.id).filter(|_| sweeps_registry) {
        if !alive_ids.contains(&id) || claimed.contains(&id) {
            continue;
        }
        // Spoken for by ANOTHER window's saved record.  Without this,
        // a core swap with two windows open had the boot window adopt
        // the second window's live panes as orphans — and then the
        // second window's own restore tried to reattach the very same
        // sessions.  They are not orphans; their window just has not
        // been announced yet.
        if reserved_sids.contains(&id) {
            continue;
        }

        if panes.len() >= SESSION_COUNT_HARD_CAP {
            if let Some(entry) = entry_by_id.get(&id) {
                unsafe { libc::kill(entry.pid, libc::SIGKILL) };
                if !entry.shm_name.is_empty() {
                    if let Ok(c) = std::ffi::CString::new(entry.shm_name.clone()) {
                        grid_shm::delete_region(&c);
                    }
                }
            }
            match session_registry::retire_session_dir(id) {
                Ok(dst) => lx_warn!(
                    "core.boot.surplus_retired",
                    "over-cap live session stopped + dir retired",
                    session = id,
                    retired_to = dst.display()
                ),
                Err(e) => lx_warn!(
                    "core.boot.surplus_retire_failed",
                    &format!("{e}"),
                    session = id
                ),
            }
            continue;
        }
        match reattach_l3_pane(id, &event_tx) {
            Ok(pane) => {
                lx_event!(
                    "core.boot.orphan_adopted",
                    "live session outside saved layout appended as extra pane",
                    session = id
                );
                panes.push(pane);
                claimed.insert(id);
                reattached_ids.push(id);
            }
            Err(e) => lx_warn!(
                "core.reattach.orphan_failed",
                &format!("{e} — left on disk for next boot"),
                session = id
            ),
        }
    }

    // RFC-004 B.4 — recycle-bin GC.  Any session dir that is
    // neither claimed by a slot nor alive moves to retired/
    // (kept RETIRED_TTL_SECS = 14 days), and expired retired
    // entries purge.  Scans dir names directly (not raw_list) so
    // dirs with corrupt entry.toml — invisible to discovery —
    // stop accumulating too.  delete_session (真删) stays
    // reserved for the user's explicit pane close.
    let mut retired = 0usize;
    for id in session_registry::list_session_dir_ids() {
        // Same reason as the orphan loop above: a restored window's
        // `claimed` set covers only its own saved sids, so sweeping
        // here would retire the live dirs of every other window.  And
        // a dead session another window intends to resurrect must keep
        // its dir — retiring it would destroy that pane's history one
        // moment before its window asked for it back.
        if !sweeps_registry
            || claimed.contains(&id)
            || alive_ids.contains(&id)
            || reserved_sids.contains(&id)
        {
            continue;
        }
        if let Some(entry) = entry_by_id.get(&id) {
            if !entry.shm_name.is_empty() {
                if let Ok(c) = std::ffi::CString::new(entry.shm_name.clone()) {
                    grid_shm::delete_region(&c);
                }
            }
        }
        match session_registry::retire_session_dir(id) {
            Ok(_) => retired += 1,
            Err(e) => lx_warn!(
                "core.boot.retire_failed",
                &format!("{e}"),
                session = id
            ),
        }
    }
    let purged = if sweeps_registry {
        session_registry::purge_expired_retired()
    } else {
        0
    };
    lx_event!(
        "core.session_registry.inventory",
        "RFC-004 identity-keyed boot assembly",
        total = raw_list.len(),
        alive = alive_ids.len(),
        reattached = reattached_ids.len(),
        slots = slot_specs.len(),
        panes = panes.len(),
        retired = retired,
        purged_expired = purged
    );

    // Per-pane custom titles.  F3+6 — `saved_state.panes` wins over
    // `session_titles` (the entry.toml-derived source) because the
    // bin file reflects the user's last interactive state, including
    // titles set after the last L3 reattach hop.
    //
    // RFC-004 B.3 — titles bind by IDENTITY, not index: the
    // positional entry is only trusted when its recorded sid matches
    // the pane actually sitting in that slot (or the slot was
    // anonymous, sid == 0, and just received a fresh id).  On
    // mismatch, search the saved panes for the sid — a title follows
    // its session wherever the session lands.  Fallback chain:
    // saved-by-slot → saved-by-sid → entry.toml title → None.
    for (i, p) in panes.iter_mut().enumerate() {
        let pane_sid = p.shelld_session_id().unwrap_or(0);
        p.custom_title = (|| {
            if let Some(s) = saved_state {
                if let Some(entry) = s.panes.get(i) {
                    let slot_matches =
                        entry.sid == pane_sid || entry.sid == 0;
                    if slot_matches && !entry.custom_title.is_empty() {
                        return Some(entry.custom_title.clone());
                    }
                }
                if pane_sid != 0 {
                    if let Some(entry) =
                        s.panes.iter().find(|e| e.sid == pane_sid)
                    {
                        if !entry.custom_title.is_empty() {
                            return Some(entry.custom_title.clone());
                        }
                    }
                }
            }
            (pane_sid != 0)
                .then(|| session_titles.get(&pane_sid).cloned())
                .flatten()
                .filter(|t| !t.is_empty())
        })();
    }
    (panes, reattached_ids)
}

fn main() {
    // RFC-004 D.1 — must precede logx / any path computation.
    marspot::paths::migrate_legacy_state_root();
    marspot::logx::init("core");

    // Must run before any env is read — see `parse_log_event`.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some((tag, detail)) = parse_log_event(&args) {
        lx_event!("SUP_LOG", &detail, tag = tag);
        return;
    }

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

    let renderer = MetalRenderer::new_headless().expect("[core] MetalRenderer::new_headless");
    // Double-buffer: the boot window owns a (surface, texture) pair.
    // Per-frame render alternates its `writing_idx`; the shell's
    // presenter listens for `SurfaceReady(id)` and points at whichever
    // slot is freshly done.  Eliminates the cross-process mid-render
    // race that was the dominant flash source (handoff 2026-06-15).
    //
    // Stale env IDs are normal during a dual-core install-local swap
    // window: L1 spawns this core with a `front_id`/`back_id` pair,
    // then a few ms later decides the previous core is unrecoverable
    // and rotates the pair before we get here.  Exit cleanly instead
    // of panicking so L1's "crash/hang detect → respawn" path picks
    // the next slot without a backtrace storm in marspot.log.
    let Some(boot_surfaces) = WindowSurfaces::attach(front_id, back_id, renderer.device()) else {
        lx_warn!(
            "core.surface.lookup_nil",
            "IOSurface pair stale (L1 rotated mid-spawn); exiting for respawn",
            front_id = front_id,
            back_id = back_id
        );
        std::process::exit(2);
    };

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
    // fresh-spawn cwds, focused_idx and per-pane titles below.
    // RFC-005 step 6 — the file holds every window.  The boot window
    // takes windows[0]; the rest are restored as L1 opens them (each
    // `SurfaceAttachWindow` for an unseen id pops the next record).
    let saved_state = marspot::state::read();
    let mut saved_windows: std::collections::VecDeque<marspot::state::SavedWindowLayout> =
        saved_state.map(|s| s.windows.into()).unwrap_or_default();
    let boot_window = saved_windows.pop_front();
    let (grid_cols, grid_rows): (usize, usize) = match boot_window.as_ref() {
        Some(s) if s.grid_cols > 0 && s.grid_rows > 0 => (
            (s.grid_cols as usize).clamp(1, 6),
            (s.grid_rows as usize).clamp(1, 6),
        ),
        _ => (3, 3),
    };
    let n_sessions = match boot_window.as_ref() {
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
        // RFC-004 B.1 — identity-keyed slot assembly (extracted to
        // `assemble_panes_at_boot`; see its doc for the semantics).
        // The boot assembly is the sweeping one: it adopts orphaned
        // live sessions and retires unclaimed dirs.  Restored windows
        // (step 6b) run the same function with that turned off.
        //
        // Every sid the OTHER saved windows own is reserved: those
        // sessions are neither orphans to adopt nor junk to retire,
        // they are simply waiting for their own window to be opened.
        let reserved_sids: std::collections::HashSet<u64> = saved_windows
            .iter()
            .flat_map(|w| w.panes.iter())
            .map(|p| p.sid)
            .filter(|sid| *sid != 0)
            .collect();
        let (assembled, reattached_ids) = assemble_panes_at_boot(
            boot_window.as_ref(),
            n_sessions,
            boot_cols,
            boot_rows,
            &event_tx,
            true,
            &reserved_sids,
        );
        panes = assembled;
        // RFC-003 §6 Amendment 16 — L3 self-execv silent update
        // fanout (unchanged): freshly-promoted session binary →
        // SIGTERM every reattached L3 so it execv's into the new
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

    let initial_focused_idx = boot_window.as_ref()
        .map(|s| (s.focused_idx as usize).min(panes.len().saturating_sub(1)))
        .unwrap_or(0);
    let mut app = CoreApp {
        renderer,
        pane_badges: std::collections::HashMap::new(),
        pane_titles: std::collections::HashMap::new(),
        pane_cwds: std::collections::HashMap::new(),
        pending_to_shell: Vec::new(),
        pane_sessions: std::collections::HashMap::new(),
        esc_history: std::collections::HashMap::new(),
        last_cwd_refresh: std::collections::HashMap::new(),
        reconnecting: std::collections::HashSet::new(),
        cwd_unresolvable: std::collections::HashMap::new(),
        all_exited: false,
        saw_window_aware_attach: false,
        l3_mode,
        event_tx: event_tx.clone(),
        drag_window: None,
        saved_windows,
        windows: vec![{
            let mut w = WindowState::new(
                FIRST_WINDOW_ID,
                panes,
                initial_focused_idx,
                grid_cols,
                grid_rows,
                w_phys,
                h_phys,
                scale,
            );
            // The boot window's pair comes from the env handshake, so
            // it has a paint target before the first frame; every
            // later window gets one from its `SurfaceAttachWindow`.
            w.surfaces = Some(boot_surfaces);
            w
        }],
        key_window: 0,
    };
    app.rebuild_layout(0);
    // RFC-005 step 6b — ask L1 to reopen the windows the last session
    // had past this one.  Frame index i+1 because the boot window is
    // entry 0 of `window-state.bin`.  Queued rather than written here:
    // the control socket is not up yet, and `pending_to_shell` drains
    // on the first loop iteration.
    for i in 0..app.saved_windows.len() {
        app.pending_to_shell.push((
            MsgType::WindowOpenRequest,
            marspot::shell_proto::encode_window_open_request(i as u32 + 1),
        ));
    }
    if !app.saved_windows.is_empty() {
        lx_event!(
            "WINDOW_RESTORE_REQUESTED",
            "asked L1 to reopen the rest of the saved windows",
            windows = app.saved_windows.len()
        );
    }
    // F3+6 — first save right after boot so a hard kill before any
    // user action still leaves the file populated.  Costs one write
    // (~50us); idempotent if shell-state.bin already matched.
    app.save_session_state();
    lx_info!(
        "core.layout.ready",
        "initial layout built",
        windows = app.windows.len(),
        window_id = win!(app, 0).window_id,
        mode = format!("{}x{}", win!(app, 0).grid_cols, win!(app, 0).grid_rows),
        panes = win!(app, 0).panes.len(),
        cols = win!(app, 0).layout.cells[0].cols,
        rows = win!(app, 0).layout.cells[0].rows
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
    // L2 → L1 frames go through a writer thread, never inline.  L1 is
    // an AppKit main loop; whenever it is busy it stops draining, and
    // the kernel's 8 KiB socket buffer then parks a blocking write —
    // which would freeze *every* pane, since this one loop drives them
    // all.  Same fix L3 got for its end of the wire.
    let control_writer = marspot_term::frame_writer::FrameWriter::new(
        "l2-shell-writer",
        control_stream,
        marspot_term::frame_writer::cap::L2_TO_L1,
    );
    let reader_tx = event_tx.clone();
    std::thread::spawn(move || reader_loop(reader_stream, reader_tx));

    // SIGUSR2 → per-session silent-update trigger (behind MARSPOT_L3=1 it
    // swaps idle L3 panes; a no-op otherwise).
    install_swap_trigger(event_tx.clone());

    lx_event!(
        "CORE_LOOP",
        "entering event loop (event-driven, no fixed cadence)"
    );
    // Self-reporting stalls.  A blocked iteration here freezes *every*
    // pane, so the threshold is tighter than L3's: this loop is
    // supposed to turn a frame around, and anything past ~80 ms has
    // already cost the user a visible hitch.  Silent below it.
    let mut watch = marspot_term::loop_watch::LoopWatch::new(
        L2_LOOP_STALL_THRESHOLD,
    );
    // A live report, not a post-mortem: this loop drives every pane, so
    // while it is wedged the whole window is frozen and the user is
    // looking at it right then.  `end()` cannot speak until the
    // iteration completes.
    watch.spawn_watchdog(|elapsed, phase| {
        lx_warn!(
            "l2.loop.stalling",
            "main loop iteration STILL running — every pane is frozen right now",
            phase = phase,
            elapsed_ms = elapsed.as_millis()
        );
    });

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
    'main: loop {
        // `begin` goes after the wait below, not here — see the
        // `watch.begin()` call once an event is in hand.
        let first = if first_tick {
            first_tick = false;
            event_rx.try_recv().ok()
        } else {
            // When any window's render is gated by the frame-interval
            // cap, wake the loop in ≤ FRAME_MIN_INTERVAL to flush the
            // deferred frame; otherwise stay event-driven at the 1 s
            // idle timeout so CPU at rest stays near zero.  The
            // deadline is the soonest across the dirty windows — one
            // window's cap must not delay another's frame.
            let recv_timeout = app
                .windows
                .iter()
                .filter(|w| w.needs_render)
                .map(|w| match w.last_render_at {
                    Some(t) if t.elapsed() < frame_min_interval => {
                        frame_min_interval - t.elapsed()
                    }
                    _ => Duration::from_millis(0),
                })
                .min()
                .unwrap_or(Duration::from_secs(1));
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
        // The process panel is per-window: two windows can each have
        // one open, and a kill pending in an unfocused window still has
        // to escalate on schedule.
        for wi in 0..app.windows.len() {
            app.tick_process_panel_kills(wi);
            let due = win!(app, wi)
                .process_panel
                .as_ref()
                .is_some_and(|p| p.last_refresh.elapsed() >= PROCESS_PANEL_REFRESH_INTERVAL);
            if due {
                app.refresh_process_panel(wi);
                win!(app, wi).needs_render = true;
            }
        }
        // Drain pending control-socket events.  Attach coalescing:
        // keep only the latest SurfaceAttach *per window* (resize
        // fires fast in a live drag — old attach payloads are stale by
        // the time we get to render, but window A's stale payload must
        // never displace window B's fresh one); liveness frames are
        // echoed within the same drain pass.
        let mut pending_attach: Vec<(u32, (u32, u32, f64, f64, f64))> = Vec::new();
        let mut to_ack: Vec<(MsgType, Vec<u8>)> = Vec::new();
        let mut closed = false;
        let process = |app: &mut CoreApp,
                           ev: CoreEvent,
                           pending_attach: &mut Vec<(u32, (u32, u32, f64, f64, f64))>,
                           to_ack: &mut Vec<(MsgType, Vec<u8>)>,
                           closed: &mut bool| {
            /// Latest-wins, keyed by window: replace this window's
            /// queued attach if it has one, else append.
            fn queue_attach(
                q: &mut Vec<(u32, (u32, u32, f64, f64, f64))>,
                window_id: u32,
                payload: (u32, u32, f64, f64, f64),
            ) {
                match q.iter_mut().find(|(id, _)| *id == window_id) {
                    Some(slot) => slot.1 = payload,
                    None => q.push((window_id, payload)),
                }
            }
            match ev {
                // RFC-005 step 4e — each input kind names its window in
                // its own way, and the differences are the point:
                //
                //   press / right-press / drop  the window it landed
                //                               in, which also takes
                //                               focus (clicking focuses)
                //   scroll / move               the window under the
                //                               cursor, focus untouched
                //   drag / release              the window that took
                //                               the press, so a drag
                //                               that crosses windows
                //                               still finishes where it
                //                               started
                //   key / preedit               the window it was typed
                //                               into
                //
                // A frame for a window the core no longer has is a
                // normal race (it closed first), and every arm below
                // drops it rather than falling back to the key window
                // — silently acting on the wrong window is worse than
                // dropping one event.
                CoreEvent::Key(event, mods, win) => {
                    if let Some(wi) = app.window_index(win) {
                        app.key(wi, event, mods)
                    }
                }
                CoreEvent::MouseDown(x, y, mods, win) => {
                    if let Some(wi) = app.focus_window(win) {
                        app.drag_window = Some(win);
                        app.mouse_down(wi, x, y, mods)
                    }
                }
                CoreEvent::MouseRightDown(x, y, mods, win) => {
                    if let Some(wi) = app.focus_window(win) {
                        app.mouse_right_down(wi, x, y, mods)
                    }
                }
                CoreEvent::MouseDrag(x, y, win) => {
                    if let Some(wi) = app.drag_target(win) {
                        app.mouse_drag(wi, x, y)
                    }
                }
                CoreEvent::MouseUp(win) => {
                    if let Some(wi) = app.drag_target(win) {
                        app.mouse_up(wi)
                    }
                    app.drag_window = None;
                }
                CoreEvent::MouseMove(x, y, win) => {
                    if let Some(wi) = app.window_index(win) {
                        app.mouse_moved(wi, x, y)
                    }
                }
                CoreEvent::Scroll(dy, precise, win) => {
                    if let Some(wi) = app.window_index(win) {
                        app.scroll(wi, dy, precise)
                    }
                }
                CoreEvent::FileDrop(x, y, paths, win) => {
                    if let Some(wi) = app.focus_window(win) {
                        app.file_drop(wi, x, y, &paths)
                    }
                }
                CoreEvent::WindowFocus(win) => {
                    app.focus_window(win);
                }
                CoreEvent::Preedit(text, win) => {
                    if let Some(wi) = app.window_index(win) {
                        app.preedit(wi, text)
                    }
                }
                CoreEvent::WindowRestoreFinished(win, panes) => {
                    app.adopt_restored_panes(win, panes.0)
                }
                CoreEvent::WindowClosed(win) => app.close_window(win),
                CoreEvent::SurfaceAttachWindow(fr, bk, w, h, sc, win) => {
                    // Same staging as the legacy frame — the id only
                    // says which window it is about.  From the first
                    // one of these onward the legacy frame is ignored,
                    // so a shell that sends both (for the benefit of
                    // cores that predate RFC-005) does not attach twice.
                    app.saw_window_aware_attach = true;
                    if app.window_index(win).is_none() {
                        // A window id we have never seen IS the birth
                        // event for that window — RFC-005 deliberately
                        // has no separate "create window" frame, so a
                        // window that appears after a core swap is
                        // adopted by the same path that created it.
                        app.adopt_window(win, w, h, sc);
                    }
                    queue_attach(pending_attach, win, (fr, bk, w, h, sc));
                }
                CoreEvent::Focus(focused) => {
                    // Application-level, not window-level: the whole
                    // app gained or lost focus, so every window's
                    // chrome changes and every window owes a frame.
                    app.renderer.set_window_focused(focused);
                    // No mouseMoved deliveries while the app isn't
                    // active — drop any latched hover so the
                    // affordance doesn't linger when the user
                    // alt-tabs away mid-hover.
                    for w in app.windows.iter_mut() {
                        if !focused {
                            w.hover_chrome_btn = None;
                        }
                        w.needs_render = true;
                    }
                }
                CoreEvent::Closed => *closed = true,
                CoreEvent::Resize(_new_id, _new_w, _new_h, _new_scale) => {
                    // Legacy PROTO_VERSION=1 path — kept as a tolerance
                    // hook but the dual-buffer shell only sends
                    // SurfaceAttach now.  Silently drop.
                }
                CoreEvent::SurfaceAttach(f_id, b_id, new_w, new_h, new_scale) => {
                    // Legacy, window-blind frame.  A shell that knows
                    // about windows sends the window-aware form too;
                    // once we have seen one, this is the duplicate.
                    // Window-blind means the boot window by definition
                    // — a shell that can't name windows only has one.
                    if !app.saw_window_aware_attach {
                        queue_attach(
                            pending_attach,
                            FIRST_WINDOW_ID,
                            (f_id, b_id, new_w, new_h, new_scale),
                        );
                    }
                }
                CoreEvent::L3ControlEof(sid) => {
                    // L3's reader EOF'd — typically silent-update
                    // execv before manifest v2 carried control_stream_fd
                    // closed the inherited stream; possibly a hard
                    // crash; possibly a dual-core swap race.
                    //
                    // Reconnect on a thread, not here.  `wait_and_connect`
                    // polls the filesystem for entry.toml and then
                    // retries `connect` with backoff — up to 2× the
                    // timeout — and a silent update EOFs every pane at
                    // once, so inline this froze the whole window for
                    // tens of seconds.
                    if app.reconnecting.insert(sid) {
                        let tx = app.event_tx.clone();
                        std::thread::spawn(move || {
                            // Off the loop, retries are free, so take
                            // several.  The old single attempt left the
                            // pane permanently mute when it lost the
                            // race: the reader thread had already
                            // returned, so no further EOF would ever
                            // arrive to trigger another try.
                            let mut got = None;
                            for attempt in 0..L3_RECONNECT_ATTEMPTS {
                                match marspot::uds_session_client::wait_and_connect(
                                    sid,
                                    // 4 s, explicitly — see the note on
                                    // the reattach path.  Off-loop now,
                                    // so the wait costs nobody a frame.
                                    std::time::Duration::from_secs(4),
                                ) {
                                    Ok(s) => {
                                        got = Some(s);
                                        break;
                                    }
                                    Err(e) => lx_warn!(
                                        "core.l3.control_reconnect_attempt_failed",
                                        &format!("{e}"),
                                        session = sid,
                                        attempt = attempt + 1,
                                        of = L3_RECONNECT_ATTEMPTS
                                    ),
                                }
                            }
                            let _ = tx.send(CoreEvent::L3ControlReconnected(sid, got));
                        });
                    }
                }
                CoreEvent::L3ControlReconnected(sid, stream) => {
                    app.reconnecting.remove(&sid);
                    let Some(new_control) = stream else {
                        lx_warn!(
                            "core.l3.control_reconnect_failed",
                            "every attempt failed; pane stays on its dead stream",
                            session = sid
                        );
                        return;
                    };
                    let found = app.windows.iter().enumerate().find_map(|(wi, w)| {
                        w.panes
                            .iter()
                            .position(|p| p.session().l3_session_id() == Some(sid))
                            .map(|pi| (wi, pi))
                    });
                    let Some((wi, idx)) = found else { return };
                    match new_control.try_clone() {
                        Ok(rh) => {
                            // Both halves of the new connection move
                            // together: the reader thread keeps the
                            // sender, the pane takes the receiver.
                            // Dropping the receiver here is what used to
                            // kill Cmd-C after a reconnect (see
                            // `L3Conn::swap_control`).
                            let (selection_tx, selection_rx) =
                                std::sync::mpsc::channel::<(u32, String)>();
                            win!(app, wi).panes[idx]
                                .session_mut()
                                .swap_l3_control(new_control, selection_rx);
                            let tx = app.event_tx.clone();
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
                CoreEvent::L3SpawnFinished(sid, outcome) => {
                    let Some((wi, idx)) = app.find_pane_by_sid(sid) else {
                        // The slot was closed while the spawn ran.  The
                        // child is dropped with the L3Spawn, which kills
                        // it — nothing to reap here.
                        return;
                    };
                    match outcome.0 {
                        Ok(spawn) => {
                            win!(app, wi).panes[idx].adopt_backend(
                                marspot::pane::PaneBackend::L3(
                                    marspot::pane::L3Conn::new(spawn, sid),
                                ),
                            );
                            lx_event!(
                                "L3_SPAWN_ADOPTED",
                                "off-loop spawn landed; pending slot is now live",
                                session_id = sid,
                                pane = idx
                            );
                        }
                        Err(e) => {
                            // Fall back to a plain vacant slot: its
                            // revive-on-keystroke path is exactly the
                            // retry affordance this needs.  Swap the
                            // backend rather than replacing the whole
                            // `Pane`, so both outcomes go through the
                            // same door — replacing wholesale would
                            // quietly drop any pane-level state the day
                            // someone adds a field that outlives a
                            // failed spawn.
                            let (c, r) = {
                                let g = win!(app, wi).panes[idx].session().grid();
                                (g.cols(), g.rows())
                            };
                            win!(app, wi).panes[idx].adopt_backend(
                                marspot::pane::PaneBackend::Vacant(
                                    marspot::pane::VacantPane::new(sid, c, r),
                                ),
                            );
                            lx_error!(
                                "core.spawn.failed",
                                &format!("{e}"),
                                session_id = sid,
                                pane = idx
                            );
                        }
                    }
                    win!(app, wi).needs_render = true;
                }
                CoreEvent::L3Ready => {
                    // A bare wake — the poke carries no session id, so
                    // which window changed is not knowable here.
                    // `pump_all` reads every window's mirror and marks
                    // only the ones that actually took bytes; setting
                    // needs_render here would repaint all of them for
                    // one pane's output.
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
                CoreEvent::PaneBadgeMenu(sid, x, y, items) => {
                    // The menu belongs over the badge that was clicked,
                    // which is in whichever window holds that session.
                    if let Some((wi, _)) = app.find_pane_by_sid(sid) {
                        app.open_pane_badge_menu(wi, sid, x, y, items);
                    }
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
        // Time from here: the wait above is the loop doing its job,
        // not stalling.
        watch.begin();
        watch.phase("events");
        if let Some(ev) = first {
            process(&mut app, ev, &mut pending_attach, &mut to_ack, &mut closed);
        }
        while let Ok(ev) = event_rx.try_recv() {
            process(&mut app, ev, &mut pending_attach, &mut to_ack, &mut closed);
        }
        // Everything from here to the next marker used to be one
        // "shell-io" phase, which turned out to be a catch-all that
        // also swallowed pump + render + the GPU wait — a 2 s stall
        // reported as "shell-io" told us nothing.  Split so the next
        // report points at something.
        watch.phase("shell-writes");
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
                n_panes = app.total_panes(),
                windows = app.windows.len()
            );
            break 'main;
        }
        for (ty, payload) in to_ack.drain(..) {
            let which = format!("liveness:{ty:?}");
            send_to_shell(&control_writer, Frame::new(ty, payload), &which);
        }
        // Drain frames queued from inside CoreApp event handlers
        // (mouse_down → PaneBadgeClicked, future similar paths).
        for (ty, payload) in app.pending_to_shell.drain(..) {
            send_to_shell(&control_writer, Frame::new(ty, payload), "pending_to_shell");
        }
        // Apply each window's freshly-attached pair.  Per window:
        // swap the paint target, re-lay out, render one frame into
        // slot 0 and ack `SurfaceReady` so the shell can install the
        // new pair and swap its presenter onto it.  A stale id for one
        // window leaves every other window's attach untouched.
        for (window_id, (new_front, new_back, new_w, new_h, new_scale)) in
            pending_attach.drain(..)
        {
            let Some(wi) = app.attach_window_surfaces(
                window_id, new_front, new_back, new_w, new_h, new_scale,
            ) else {
                continue;
            };
            // Render the latest content into slot 0 so the
            // SurfaceReady ack reflects a real frame.  Distinct phase
            // names: this branch and the main render path can both run
            // in one iteration, and a breakdown listing "pump" twice
            // reads like double bookkeeping.
            watch.phase("attach-pump");
            app.pump_all();
            watch.phase("attach-render");
            let tex = win!(app, wi)
                .surfaces
                .as_ref()
                .map(|s| s.writing_tex())
                .expect("attach just installed a pair");
            let _ = app.render(wi, &tex);
            watch.phase("attach-post");
            if let Some(s) = win!(app, wi).surfaces.as_mut() {
                let ack = Frame::new(
                    MsgType::SurfaceReady,
                    encode_surface_ready(s.writing_surface_id()),
                );
                // Next render writes the other half.
                s.flip();
                send_to_shell(&control_writer, ack, "surface_ready");
            }
        }
        watch.phase("pump");
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
                n_panes = app.total_panes(),
                windows = app.windows.len()
            );
            break 'main;
        }

        // Frame-interval cap, per window: defer a window's frame if
        // it rendered < FRAME_MIN_INTERVAL ago.  `needs_render` stays
        // true so the next loop iteration tries again — and the loop's
        // recv_timeout above is set to wake us inside the cap window,
        // so the deferred frame lands within ~8 ms, not 1 s.  The cap
        // is per window because a window streaming build output must
        // not gate the repaint of a quiet one next to it.
        for wi in 0..app.windows.len() {
            if !win!(app, wi).needs_render {
                continue;
            }
            let gated = win!(app, wi)
                .last_render_at
                .is_some_and(|t| t.elapsed() < frame_min_interval);
            if gated {
                continue;
            }
            // A window with no paint target (stale ids at attach) is
            // skipped, not fatal: its peers keep painting and the next
            // attach gives it one.
            let Some(tex) = win!(app, wi).surfaces.as_ref().map(|s| s.writing_tex()) else {
                continue;
            };
            // Double-buffer: render into the back slot.
            // `render_layout_to_texture` calls `waitUntilCompleted`, so
            // the moment we return here the surface bytes are settled
            // and safe for the shell to sample — that's what makes
            // `SurfaceReady` the dual-buffer race fix: we only ever
            // flip to a slot the GPU has already finished.
            let render_t0 = Instant::now();
            win!(app, wi).last_render_at = Some(render_t0);
            watch.phase("render");
            let caret = app.render(wi, &tex);
            watch.phase("post-render");
            let (surface_id, writing_idx) = {
                let Some(s) = win!(app, wi).surfaces.as_mut() else { continue };
                let ids = (s.writing_surface_id(), s.writing_idx);
                // Flip: next render writes the other half.
                s.flip();
                ids
            };
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
                window_id = win!(app, wi).window_id,
                writing_idx = writing_idx,
                surface_id = surface_id,
                dur_us = render_t0.elapsed().as_micros() as u64,
                n_panes = win!(app, wi).panes.len(),
                focused_idx = win!(app, wi).focused_idx
            );
            // Per-frame ack — the just-completed surface ID.  In v=2
            // this replaces the empty-payload `FrameRendered` poke:
            // shell uses the id to flip its presenter's `current_idx`,
            // then presents.  Same one frame round-trip the old path
            // had, but the present now samples a guaranteed-finished
            // surface instead of racing the writing one.  The shell
            // resolves which window an id belongs to by looking it up
            // in its per-window pairs, so no window tag is needed.
            let ack = Frame::new(MsgType::SurfaceReady, encode_surface_ready(surface_id));
            send_to_shell(&control_writer, ack, "surface_ready");
            // FrameRendered is the legacy v=1 wake.  A v=2 shell
            // already woke on SurfaceReady, so this is redundant for
            // a same-version shell.  An OLD shell paired with this
            // NEW core (rare — possible after a botched silent
            // update) only listens for FrameRendered, though, so we
            // keep emitting it for compatibility.  No-op on the v=2
            // shell side (handler just sets frame_pending again).
            let fr = Frame::new(MsgType::FrameRendered, Vec::new());
            send_to_shell(&control_writer, fr, "frame_rendered");
            // Publish the focused-pane caret so the shell can anchor
            // the IME candidate window.  Dedupe — an idle cursor must
            // not stream identical frames at render cadence.
            if win!(app, wi).last_caret_sent != Some(caret) {
                win!(app, wi).last_caret_sent = Some(caret);
                let f = Frame::new(
                    MsgType::CaretRect,
                    encode_caret_rect(caret, win!(app, wi).window_id),
                );
                send_to_shell(&control_writer, f, "caret_rect");
            }
        }

        if let Some(r) = watch.end() {
            lx_warn!(
                "l2.loop.stall",
                &r.summary(),
                slowest = r.slowest,
                slowest_ms = r.slowest_took.as_millis(),
                breakdown = r.breakdown(),
                panes = app.total_panes(),
                stalls_total = watch.stall_count()
            );
        }

        frame += 1;
        if frame.is_multiple_of(300) {
            let t = start.elapsed().as_secs_f64();
            // The focused pane of the key window — indexing blindly
            // would panic on a window that is mid-restore and holds no
            // panes yet.
            let kw = app.key_window;
            let (state, cols, rows) = match win!(app, kw).try_focused_pane() {
                Some(p) => (
                    match p.session().state() {
                        SessionState::Active => "active",
                        SessionState::Idle => "idle",
                        SessionState::Exited => "exited",
                    },
                    p.session().grid().cols(),
                    p.session().grid().rows(),
                ),
                None => ("none", 0, 0),
            };
            lx_debug!(
                "core.heartbeat",
                "periodic heartbeat",
                frame = frame,
                t_s = format!("{t:.1}"),
                windows = app.windows.len(),
                panes = app.total_panes(),
                key_window = win!(app, kw).window_id,
                focused = win!(app, kw).focused_idx,
                state = state,
                cols = cols,
                rows = rows
            );
        }
    }
}

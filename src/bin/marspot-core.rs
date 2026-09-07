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
use marspot::ui::components::{GRID_MAX, GRID_MIN};
use marspot::session::SessionState;
use marspot::session_registry::{
    self, allocate_next_session_id, list_session_entries,
};
use marspot::shell_proto::{
    decode_file_drop, decode_focus, decode_hello, decode_key_event, decode_mouse, decode_ping,
    decode_preedit, decode_resize, decode_scroll, decode_selection_text, decode_surface_attach,
    decode_surface_attach_window, decode_window_chrome, decode_window_closed,
    decode_window_focus,
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
/// What `open(1)` should be handed for a clicked link, or `None` when
/// the kind has no open action.
///
/// A free function so it can be tested: the caller is buried in the
/// context-menu event path, and the one case that actually broke was
/// invisible from there.
fn open_arg_for(kind: marspot::grid_links::LinkKind, text: &str) -> Option<String> {
    let link_text = text;
    match kind {
        marspot::grid_links::LinkKind::Email => {
            Some(format!("mailto:{}", link_text))
        }
        // Bare IP (`47.96.114.231`, `::1`,
        // `2001:db8::1`, `[fe80::1]:8080/foo`) needs an
        // `http://` scheme so `open(1)` routes it —
        // defaults to port 80.  Bare IPv6 without
        // brackets gets wrapped so URL parsers accept
        // it (colons in a hostname are ambiguous with
        // `host:port` otherwise).
        marspot::grid_links::LinkKind::Ip => {
            Some(if link_text.starts_with('[')
                || !ip_text_is_bare_ipv6(&link_text)
            {
                format!("http://{}", link_text)
            } else {
                format!("http://[{}]", link_text)
            })
        }
        marspot::grid_links::LinkKind::Url => {
            let head: String = link_text
                .chars()
                .take(8)
                .flat_map(char::to_lowercase)
                .collect();
            Some(
                if head.starts_with("http://")
                    || head.starts_with("https://")
                {
                    link_text.to_string()
                } else {
                    format!("http://{}", link_text)
                },
            )
        }
        // UUID has no Open action (menu doesn't offer
        // it); if a stale click reaches here, no-op.
        marspot::grid_links::LinkKind::Uuid => None,
        // Expand `~` here, not at the call site: `open(1)` does not do
        // shell expansion, so it read `~` as a directory name under the
        // process cwd and reported the file missing — silently, since
        // `spawn` only fails when the process cannot START.  Every
        // `~/…` file link underlined correctly (linkify expands for its
        // stat) and then did nothing when clicked.
        marspot::grid_links::LinkKind::File => {
            // A `file://` link carries the scheme in its text so the
            // selection matches what is drawn; `open(1)` wants the
            // path, and percent escapes have to come back off first
            // (`file://…/a%20b` names `a b`).
            let raw = link_text
                .strip_prefix("file://")
                .map(percent_decode)
                .unwrap_or_else(|| link_text.to_string());
            marspot::grid_links::expand_user_path(&raw)
                .map(|p| p.to_string_lossy().into_owned())
        }
    }
}

/// Undo percent-escapes in a `file://` URL's path.
///
/// Only `%XX` is handled, and an invalid escape is left as written —
/// a filename may legitimately contain a bare `%`, and mangling it
/// would turn a working link into a missing file.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

fn spawn_open(arg: &str) {
    // `spawn` only reports that the process could not START.  When
    // `open(1)` itself fails — the usual reason being a path that no
    // longer exists — it exits non-zero and prints to a stderr nobody
    // reads, so a click that does nothing leaves no trace at all.  A
    // link is only drawn after linkify stats the path, so a miss here
    // means the file moved between the frame and the click; say so.
    if !arg.starts_with("http://")
        && !arg.starts_with("https://")
        && !arg.starts_with("mailto:")
        && std::fs::symlink_metadata(arg).is_err()
    {
        lx_warn!(
            "core.link_open_missing",
            "link target is gone — the pane still shows it because \
             nothing has redrawn that row since",
            arg = arg
        );
    }
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
    /// The settings panel.
    Settings,
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
        Some(ChromeBtn::Settings) => Some(5),
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

/// Shortest L2 main-loop iteration worth reporting as a stall.
///
/// Tighter than L3's 150 ms because this loop owes a frame: anything
/// past ~80 ms has already cost the user a visible hitch, and a stall
/// here freezes every pane rather than one.
const L2_LOOP_STALL_THRESHOLD: Duration = Duration::from_millis(80);

/// How often the main loop re-reads every pane's cwd.
///
/// The trigger set this replaces (Enter in the focused pane, focus
/// change, modal open) could not keep a title current: the Enter that
/// runs `cd` fires the syscall *before* the shell has chdir'd, so a
/// pane's title was always one command behind, and a pane the user
/// stopped typing in — `cd x && claude`, a script that cds, the
/// non-key window — never updated at all.  A pane's cwd is not
/// something the keyboard knows about; it needs its own clock.
///
/// Cost is a `proc_pidinfo(PROC_PIDVNODEPATHINFO)` per pane per
/// sweep, measured at 0.58 us on M-series: 18 panes = ~11 us/s, and
/// the pid comes from `shell_child_pids` so entry.toml is not re-read.
/// The sweep rides the loop's existing 1 s idle wake — no new timer,
/// and a sweep that finds nothing changed marks nothing dirty, so
/// idle stays frame-free.
const CWD_SWEEP_INTERVAL: Duration = Duration::from_millis(1000);

/// Shortest gap between two `save_session_state` calls triggered by
/// the cwd sweep.  A pane whose cwd flaps (a build script cd-ing in a
/// loop) would otherwise write + fsync `shell-state.bin` once per
/// sweep forever; `last_cwd` only needs to be roughly current, since
/// it is read on cold boot.
const CWD_SAVE_MIN_GAP: Duration = Duration::from_secs(5);

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
    ToggleSidebar,
    OpenLayout,
    /// Pass the link text (URL / file path) to `/usr/bin/open`.
    /// Reads `ContextMenuState.link` for the actual string.
    OpenLink,
    /// Copy the link text to the clipboard verbatim.
    CopyLink,
}

/// Dynamic menu-tag band for "Move to Window N": tag = BASE + target
/// window index.  Plain enum tags live far below; plugin badge tags
/// are routed before decoding.  Checked before `from_tag`.
const MOVE_TO_WINDOW_TAG_BASE: u32 = 0x4000_0000;
/// "Move to New Window" — a single fixed tag just above the band.
const MOVE_TO_NEW_WINDOW_TAG: u32 = 0x4FFF_FFFF;

impl ContextMenuAction {
    fn tag(self) -> u32 { self as u32 }
    fn from_tag(t: u32) -> Option<Self> {
        match t {
            x if x == Self::CopySelection.tag() => Some(Self::CopySelection),
            x if x == Self::Paste.tag() => Some(Self::Paste),
            x if x == Self::ClearScrollback.tag() => Some(Self::ClearScrollback),
            x if x == Self::ClosePane.tag() => Some(Self::ClosePane),
            x if x == Self::SplitNewPane.tag() => Some(Self::SplitNewPane),
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
        WindowState::new(id, panes, 0, 1, 1, 800.0, 600.0)
    }

    /// RFC-005's load-bearing claim: a pane carries everything that is
    /// "its own" inside the `Pane` value, so moving it between windows
    /// is a `Vec` move and nothing else.  Anything that regresses to a
    /// window-side parallel array keyed by pane index breaks here.
    #[test]
    fn moving_a_pane_between_windows_carries_its_state() {
        let mut a = win(1, vec![Pane::new_vacant(7, 80, 24)]);
        let mut b = win(2, Vec::new());
        a.panes[0].set_view_offset(42);

        let moved = a.panes.remove(0);
        b.panes.push(moved);

        assert!(a.panes.is_empty(), "source window must give the pane up");
        assert_eq!(b.panes[0].shelld_session_id(), Some(7));
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
            pane_wheel_keys: std::collections::HashMap::new(),
            pane_wheel_enter_at: std::collections::HashMap::new(),
            pane_wheel_open: std::collections::HashMap::new(),
            pane_titles: std::collections::HashMap::new(),
            pane_cwds: std::collections::HashMap::new(),
            pending_to_shell: Vec::new(),
            last_focus_notified: std::collections::HashMap::new(),
            pane_sessions: std::collections::HashMap::new(),
            esc_history: std::collections::HashMap::new(),
            shell_child_pids: std::collections::HashMap::new(),
            last_cwd_sweep: Instant::now() - CWD_SWEEP_INTERVAL,
            last_cwd_save: Instant::now() - CWD_SAVE_MIN_GAP,
            cwd_save_pending: false,
            reconnecting: std::collections::HashSet::new(),
            all_exited: false,
            saw_window_aware_attach: false,
            l3_mode: false,
            event_tx,
            drag_window: None,
            pane_drag: None,
            drop_target: None,
            pending_move_sid: None,
            saved_windows: std::collections::VecDeque::new(),
            parked_windows: Vec::new(),
            dev_close_panes: 0,
            dev_close_panes_at: None,
            dev_open_settings_at: None,
            layout_discarded: false,
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

    /// A layout wider than the old six-column ceiling has to come
    /// back the width it was saved at.
    ///
    /// The picker went to 9 columns, but two `clamp(1, 6)` literals
    /// stayed behind in the restore paths — copies of a bound that
    /// had moved.  The save side was fine, so the file on disk said
    /// 7 while every core swap put 6 on screen; nothing looked
    /// broken except the number of columns, and only the user could
    /// see that.  Parameterised over the whole range so the next
    /// change to `GRID_MAX` cannot leave a copy behind again.
    #[test]
    fn a_restored_window_keeps_every_column_it_was_saved_with() {
        use marspot::ui::components::{GRID_MAX, GRID_MIN};
        // SAFETY: nextest runs one test per process.
        unsafe {
            std::env::set_var("MARSPOT_SESSION_BIN", "/nonexistent/marspot-session");
        }
        for cols in GRID_MIN..=GRID_MAX {
            let record = marspot::state::SavedWindowLayout {
                grid_cols: cols as u16,
                grid_rows: 2,
                focused_idx: 0,
                panes: vec![marspot::state::SavedPane { sid: 11, ..Default::default() }],
            };
            let mut app = app_with(vec![win(1, vec![Pane::new_vacant(1, 80, 24)])]);
            app.saved_windows.push_back((1, record));
            app.adopt_window(2, 1600.0, 900.0, 2.0, Some(1));
            assert_eq!(
                app.windows[1].grid_cols, cols,
                "saved {cols} columns, restored {} — a bound was copied instead of shared",
                app.windows[1].grid_cols,
            );
        }
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
        app.saved_windows.push_back((1, record));

        app.adopt_window(2, 800.0, 600.0, 2.0, Some(1));

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

        app.adopt_restored_panes(2, vec![Pane::new_vacant(11, 80, 24)]);

        let w = &app.windows[1];
        assert_eq!(w.panes.len(), 1);
        assert_eq!(w.panes[0].shelld_session_id(), Some(11));
        assert_eq!(w.focused_idx, 0, "focus clamped to the real pane count");
        assert!(w.selection.is_none(), "placeholder-era selection dropped");
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

    /// RFC-006 §3 — a move-out leaves a dormant placeholder: the
    /// source layout holds still (nothing shifts), the placeholder is
    /// not live, and a window whose last LIVE pane left asks L1 to
    /// close it.  The target still appends + focuses + becomes key.
    #[test]
    fn moving_a_pane_repairs_both_windows() {
        let mut app = app_with(vec![
            win(1, vec![
                Pane::new_vacant(10, 80, 24),
                Pane::new_vacant(11, 80, 24),
                Pane::new_vacant(12, 80, 24),
            ]),
            win(2, vec![Pane::new_vacant(20, 80, 24)]),
        ]);
        app.windows[0].focused_idx = 1;
        app.windows[0].selection = Some(Selection {
            session_idx: 1,
            anchor: (0, 0),
            focus: (1, 0),
            mode: marspot::ui::SelectionMode::Linewise,
        });

        // Move the middle pane (11): its slot becomes a placeholder,
        // panes 10 and 12 do not move, the selection on the moved
        // pane is dropped, focus lands on the nearest live pane.
        app.move_pane_to_window(0, 1, 1);
        assert_eq!(app.windows[0].panes.len(), 3, "source layout holds still");
        assert!(app.windows[0].panes[1].is_dormant(), "slot 1 is a placeholder");
        assert_eq!(app.windows[0].panes[0].shelld_session_id(), Some(10));
        assert_eq!(app.windows[0].panes[2].shelld_session_id(), Some(12));
        assert!(
            !app.windows[0].panes[app.windows[0].focused_idx].is_dormant(),
            "focus lands on a live pane, never the placeholder"
        );
        assert!(app.windows[0].selection.is_none(), "moved pane's selection dropped");
        assert_eq!(app.windows[1].panes.len(), 2);
        assert_eq!(app.windows[1].panes[1].shelld_session_id(), Some(11));
        assert_eq!(app.windows[1].focused_idx, 1, "moved pane takes focus");
        assert_eq!(app.key_window, 1, "target window becomes key");
        assert!(app.pending_to_shell.is_empty(), "no close while live panes remain");

        // Drain window 2's live panes back out — placeholders alone
        // keep nothing alive: the window must request its own close.
        app.move_pane_to_window(1, 1, 0);
        app.move_pane_to_window(1, 0, 0);
        assert!(
            app.windows[1].panes.iter().all(|p| p.is_dormant()),
            "only placeholders remain"
        );
        assert!(
            app.pending_to_shell
                .iter()
                .any(|(ty, _)| *ty == MsgType::WindowCloseRequest),
            "a window with zero live panes must request its own close"
        );
    }

    /// Closing a window is putting it away, not throwing it out.
    ///
    /// The 2026-08-03 report: two windows, close either one, quit,
    /// come back — and the closed window's sessions were gone, while
    /// the other window's survived.  Whichever window you closed first
    /// was the one you lost, which is not a rule anybody can hold in
    /// their head.  A closed window's layout is parked and saved, so
    /// the next launch reattaches it exactly like the window that was
    /// still open at quit.
    #[test]
    fn closing_a_window_parks_it_for_the_next_launch() {
        let mut app = app_with(vec![
            win(1, vec![Pane::new_vacant(10, 80, 24)]),
            win(2, vec![Pane::new_vacant(20, 80, 24), Pane::new_vacant(21, 80, 24)]),
        ]);
        app.windows[1].frame_index = 1;

        app.close_window(2);

        assert_eq!(app.windows.len(), 1, "the window is gone from the live set");
        assert_eq!(app.parked_windows.len(), 1, "…and parked, not discarded");
        assert_eq!(app.parked_windows[0].0, 1, "parked at the slot it held");

        let saved = marspot::state::read().expect("state written");
        assert_eq!(saved.windows.len(), 2, "both windows are still saved");
        let sids: Vec<Vec<u64>> = saved
            .windows
            .iter()
            .map(|w| w.panes.iter().map(|p| p.sid).collect())
            .collect();
        assert_eq!(sids, vec![vec![10], vec![20, 21]], "in slot order");
    }

    /// The slot, not the position: a parked window holds its entry in
    /// the saved list, so the windows that stayed open keep their own
    /// geometry (`window-state.bin` is paired by index).
    #[test]
    fn a_parked_window_holds_its_slot() {
        let mut app = app_with(vec![
            win(1, vec![Pane::new_vacant(10, 80, 24)]),
            win(2, vec![Pane::new_vacant(20, 80, 24)]),
            win(3, vec![Pane::new_vacant(30, 80, 24)]),
        ]);
        app.windows[1].frame_index = 1;
        app.windows[2].frame_index = 2;

        app.close_window(2);

        let saved = marspot::state::read().expect("state written");
        let sids: Vec<u64> =
            saved.windows.iter().map(|w| w.panes[0].sid).collect();
        assert_eq!(sids, vec![10, 20, 30], "the middle window keeps slot 1");
        assert_eq!(
            app.next_frame_index(),
            3,
            "a new window goes past the parked one, never on top of it"
        );
    }

    /// Closing panes is the destructive gesture — that is how a window
    /// is actually got rid of.  A window whose last pane closes is not
    /// parked, and asks L1 to close it.
    #[test]
    fn closing_the_last_pane_closes_the_window() {
        let mut app = app_with(vec![
            win(1, vec![Pane::new_vacant(10, 80, 24)]),
            win(2, vec![Pane::new_vacant(20, 80, 24)]),
        ]);
        app.windows[1].frame_index = 1;

        app.close_session(1, 0);

        assert!(
            app.pending_to_shell
                .iter()
                .any(|(ty, _)| *ty == MsgType::WindowCloseRequest),
            "the emptied window asks L1 to close it"
        );
        assert!(
            app.windows[1].panes.iter().all(|p| p.is_dormant()),
            "a placeholder holds the window together until L1 answers"
        );

        // L1 answers.  Nothing is parked: the user dismantled it.
        app.close_window(2);
        assert!(app.parked_windows.is_empty(), "an emptied window is not parked");
        let saved = marspot::state::read().expect("state written");
        assert_eq!(saved.windows.len(), 1, "only the surviving window is saved");
    }

    /// A discarded window's slot is closed up, not left as a hole.
    /// `shell-state.bin` and `window-state.bin` are paired by position,
    /// so a hole in one would hand every window above it its
    /// neighbour's geometry on the next launch.
    #[test]
    fn discarding_a_window_compacts_the_slots_above_it() {
        let mut app = app_with(vec![
            win(1, vec![Pane::new_vacant(10, 80, 24)]),
            win(2, vec![Pane::new_vacant(20, 80, 24)]),
            win(3, vec![Pane::new_vacant(30, 80, 24)]),
        ]);
        app.windows[1].frame_index = 1;
        app.windows[2].frame_index = 2;

        // Empty the middle window, then let L1's close land.
        app.close_session(1, 0);
        app.close_window(2);

        assert_eq!(app.windows.len(), 2);
        assert_eq!(app.windows[0].frame_index, 0);
        assert_eq!(app.windows[1].frame_index, 1, "window 3 moved down a slot");
        let saved = marspot::state::read().expect("state written");
        let sids: Vec<u64> =
            saved.windows.iter().map(|w| w.panes[0].sid).collect();
        assert_eq!(sids, vec![10, 30], "no hole left behind");
    }

    /// …and the last pane of the last window takes marspot with it.
    /// The saved layout goes too, so the next launch opens a fresh
    /// default window instead of restoring what was just dismantled.
    #[test]
    fn closing_the_last_pane_of_the_last_window_discards_the_layout() {
        let mut app = app_with(vec![win(1, vec![Pane::new_vacant(10, 80, 24)])]);
        app.save_session_state();
        assert!(marspot::state::read().is_some(), "precondition: a layout exists");

        app.close_session(0, 0);

        assert!(
            app.pending_to_shell
                .iter()
                .any(|(ty, _)| *ty == MsgType::WindowCloseRequest),
            "L1 is asked to close the last window, which quits the app"
        );
        assert!(app.layout_discarded);
        assert!(
            marspot::state::read().is_none(),
            "the saved layout is deleted, not left behind"
        );

        // Anything that saves afterwards must not put it back.
        app.save_session_state();
        assert!(marspot::state::read().is_none(), "and stays deleted");
    }

    /// "Move to New Window": the pane is parked by sid, and the next
    /// unseen `SurfaceAttachWindow` builds the window around the MOVED
    /// pane — no fresh spawn, and it outranks the restore queue.
    #[test]
    fn a_parked_move_claims_the_new_window_before_the_restore_queue() {
        let mut app = app_with(vec![win(1, vec![
            Pane::new_vacant(10, 80, 24),
            Pane::new_vacant(11, 80, 24),
        ])]);
        // A stale restore record is also waiting — the user action wins.
        app.saved_windows.push_back((1, marspot::state::SavedWindowLayout {
            grid_cols: 2, grid_rows: 2, focused_idx: 0,
            panes: vec![marspot::state::SavedPane { sid: 99, ..Default::default() }],
        }));
        app.pending_move_sid = Some(11);

        app.adopt_window(7, 800.0, 600.0, 2.0, Some(1));

        assert_eq!(app.windows.len(), 2);
        let nw = &app.windows[1];
        assert_eq!(nw.window_id, 7);
        assert_eq!(nw.panes.len(), 1);
        assert_eq!(
            nw.panes[0].shelld_session_id(),
            Some(11),
            "the MOVED pane, not a fresh spawn or the restore record"
        );
        assert_eq!(app.windows[0].panes.len(), 2, "source layout holds still");
        assert!(
            app.windows[0].panes[1].is_dormant(),
            "the moved pane's slot is a placeholder"
        );
        assert_eq!(app.key_window, 1);
        assert_eq!(
            app.saved_windows.len(),
            1,
            "restore queue untouched — the user action claimed this window"
        );
        assert!(app.pending_move_sid.is_none(), "park is consumed");
    }

    /// RFC-005 step 5 (drag half) — the title-press state machine.
    /// One press, three endings: stay put = the click it looked like
    /// (title edit); travel + release over another window = the pane
    /// moves there; travel + release anywhere else = cancelled.
    #[test]
    fn a_title_press_is_a_click_a_move_or_nothing() {
        let mk = || {
            let mut app = app_with(vec![
                win(1, vec![Pane::new_vacant(10, 80, 24), Pane::new_vacant(11, 80, 24)]),
                win(2, vec![Pane::new_vacant(20, 80, 24)]),
            ]);
            app.pane_drag = Some(PaneDrag {
                from_wi: 0,
                idx: 1,
                start: (100.0, 10.0),
                active: false,
            });
            app
        };

        // Ending 1 — no travel: the click stands, and moves nothing.
        // (It focused the pane on mouse-down; a pane's name is its
        // directory, so there is nothing for the click to open.)
        let mut app = mk();
        app.mouse_up(0, 1, 0.0, 0.0);
        assert_eq!(app.windows[0].panes.len(), 2, "nothing moved");

        // Ending 2 — travel past the slop, release over window 2.
        let mut app = mk();
        app.mouse_drag(0, 100.0 + PANE_DRAG_SLOP_PHYS + 1.0, 10.0, 0, 0.0, 0.0);
        assert!(app.pane_drag.unwrap().active, "slop exceeded arms the drag");
        app.mouse_up(0, 2, 0.0, 0.0);
        assert_eq!(app.windows[0].panes.len(), 2, "placeholder holds the slot");
        assert!(app.windows[0].panes[1].is_dormant());
        assert_eq!(app.windows[1].panes.len(), 2);
        assert_eq!(app.windows[1].panes[1].shelld_session_id(), Some(11));
        assert_eq!(app.key_window, 1, "landing window becomes key");

        // Ending 3 — active drag released over nothing (drop id 0)
        // or over its own window: cancelled, everything stays.
        for drop in [0u32, 1u32] {
            let mut app = mk();
            app.mouse_drag(0, 100.0, 40.0, 0, 0.0, 0.0);
            app.mouse_up(0, drop, 0.0, 0.0);
            assert_eq!(app.windows[0].panes.len(), 2, "drop={drop}: no move");
            assert!(app.pane_drag.is_none(), "drop={drop}: state cleared");
        }

        // Sub-slop wiggle stays a click.
        let mut app = mk();
        app.mouse_drag(0, 104.0, 12.0, 0, 0.0, 0.0);
        assert!(!app.pane_drag.unwrap().active, "inside slop = still a click");
        app.mouse_up(0, 1, 0.0, 0.0);
        assert_eq!(app.windows[0].panes.len(), 2, "a click moves nothing");
    }

    /// RFC-006 §1 — zone geometry: 25 % edge bands (≥48 px, ≤⅓),
    /// center elsewhere, corners to the deeper penetration.
    #[test]
    fn drop_zones_carve_the_pane_as_specified() {
        use marspot::layout::CellRect;
        let r = CellRect { x: 0.0, y_top: 0.0, w: 400.0, h: 400.0, cols: 10, rows: 10 };
        assert_eq!(drop_zone_at(&r, 10.0, 200.0), DropZone::Left);
        assert_eq!(drop_zone_at(&r, 390.0, 200.0), DropZone::Right);
        assert_eq!(drop_zone_at(&r, 200.0, 10.0), DropZone::Top);
        assert_eq!(drop_zone_at(&r, 200.0, 390.0), DropZone::Bottom);
        assert_eq!(drop_zone_at(&r, 200.0, 200.0), DropZone::Center);
        // Corner: deeper penetration wins — 5 px from the left,
        // 30 px from the top → Left.
        assert_eq!(drop_zone_at(&r, 5.0, 30.0), DropZone::Left);
        // Slim pane: the 48 px floor keeps bands usable, the ⅓ cap
        // keeps a center alive.
        let slim = CellRect { x: 0.0, y_top: 0.0, w: 90.0, h: 400.0, cols: 3, rows: 10 };
        // floor lifts 22.5px→48, cap trims to w/3 = 30.
        assert_eq!(drop_zone_at(&slim, 29.0, 200.0), DropZone::Left, "inside the 30px band");
        assert_eq!(drop_zone_at(&slim, 31.0, 200.0), DropZone::Center, "cap = w/3 keeps a center");
    }

    /// RFC-006 §2 flagship — a 1×1 window splits into 1×2 / 2×1 when
    /// a pane is dropped on its only pane's edge, and the dragged
    /// pane lands exactly where the preview said.
    #[test]
    fn dropping_on_a_1x1_window_splits_it() {
        let mk = || app_with(vec![
            win(1, vec![Pane::new_vacant(10, 80, 24), Pane::new_vacant(11, 80, 24)]),
            win(2, vec![Pane::new_vacant(20, 80, 24)]),
        ]);

        // Right band → 1×2, dragged pane in the right slot.
        let mut app = mk();
        app.split_insert(0, 1, 1, 0, DropZone::Right);
        assert_eq!((app.windows[1].grid_cols, app.windows[1].grid_rows), (2, 1));
        assert_eq!(app.windows[1].panes.len(), 2);
        assert_eq!(app.windows[1].panes[0].shelld_session_id(), Some(20));
        assert_eq!(app.windows[1].panes[1].shelld_session_id(), Some(11));
        assert_eq!(app.windows[1].focused_idx, 1);
        assert_eq!(app.key_window, 1);
        assert!(app.windows[0].panes[1].is_dormant(), "source keeps a placeholder");

        // Bottom band → 2×1, dragged pane below.
        let mut app = mk();
        app.split_insert(0, 1, 1, 0, DropZone::Bottom);
        assert_eq!((app.windows[1].grid_cols, app.windows[1].grid_rows), (1, 2));
        assert_eq!(app.windows[1].panes[1].shelld_session_id(), Some(11));

        // Left band → dragged pane takes the LEFT slot.
        let mut app = mk();
        app.split_insert(0, 1, 1, 0, DropZone::Left);
        assert_eq!(app.windows[1].panes[0].shelld_session_id(), Some(11));
        assert_eq!(app.windows[1].panes[1].shelld_session_id(), Some(20));
    }

    /// Same-window edge drop is a REARRANGE: no placeholder, the pane
    /// count is unchanged, only the order moves.
    #[test]
    fn splitting_within_a_window_rearranges_without_placeholders() {
        let mut app = app_with(vec![win(1, vec![
            Pane::new_vacant(10, 80, 24),
            Pane::new_vacant(11, 80, 24),
            Pane::new_vacant(12, 80, 24),
        ])]);
        app.windows[0].grid_cols = 2;
        app.windows[0].grid_rows = 2;
        // Drag pane 12 to pane 10's left band.
        app.split_insert(0, 2, 0, 0, DropZone::Left);
        let sids: Vec<_> = app.windows[0].panes.iter()
            .map(|p| p.shelld_session_id()).collect();
        assert_eq!(sids, vec![Some(12), Some(10), Some(11)]);
        assert!(app.windows[0].panes.iter().all(|p| !p.is_dormant()));
    }

    /// RFC-006 §1 — center drop swaps: symmetric, no reshape, no
    /// placeholder, focus + key follow the dragged pane's landing.
    #[test]
    fn center_drop_swaps_across_windows() {
        let mut app = app_with(vec![
            win(1, vec![Pane::new_vacant(10, 80, 24), Pane::new_vacant(11, 80, 24)]),
            win(2, vec![Pane::new_vacant(20, 80, 24)]),
        ]);
        app.swap_panes(0, 1, 1, 0);
        assert_eq!(app.windows[0].panes[1].shelld_session_id(), Some(20));
        assert_eq!(app.windows[1].panes[0].shelld_session_id(), Some(11));
        assert_eq!(app.windows[0].panes.len(), 2);
        assert_eq!(app.windows[1].panes.len(), 1);
        assert!(app.windows.iter().flat_map(|w| w.panes.iter()).all(|p| !p.is_dormant()));
        assert_eq!(app.key_window, 1);
        assert_eq!(app.windows[1].focused_idx, 0);
    }

    /// 2026-07-29 field report — the ghost promised a split on the
    /// pane's OWN slot while release (correctly) did nothing.  The
    /// contract is "what lights up is what release does", enforced by
    /// both sides reading ONE resolution.  Every no-op shape must
    /// resolve to Nothing:
    #[test]
    fn releases_that_change_nothing_preview_nothing() {
        let mut app = app_with(vec![win(1, vec![
            Pane::new_vacant(10, 80, 24),
            Pane::new_vacant(11, 80, 24),
        ])]);
        app.windows[0].grid_cols = 2;
        app.windows[0].grid_rows = 1;
        let drag = PaneDrag { from_wi: 0, idx: 0, start: (0.0, 0.0), active: true };
        let t = |idx, zone| Some(DropTarget { wi: 0, pane_idx: idx, zone });

        // Own slot, every zone: remove+reinsert lands back at 0.
        for zone in [DropZone::Left, DropZone::Top, DropZone::Center] {
            assert_eq!(
                app.resolve_drop_outcome(&drag, t(0, zone), 1),
                DropOutcome::Nothing,
                "own slot {zone:?}"
            );
        }
        // The neighbour's NEAR edge re-inserts at the old slot too.
        assert_eq!(
            app.resolve_drop_outcome(&drag, t(1, DropZone::Left), 1),
            DropOutcome::Nothing,
            "pane 0 on pane 1's left band = back where it was"
        );
        // …but the FAR edge is a real rearrange.
        assert!(matches!(
            app.resolve_drop_outcome(&drag, t(1, DropZone::Right), 1),
            DropOutcome::Split { .. }
        ));
        // Own window, no pane under the pointer: nothing.
        assert_eq!(app.resolve_drop_outcome(&drag, None, 1), DropOutcome::Nothing);

        // And the ghost obeys the same resolution: hovering the own
        // slot draws nothing.
        app.pane_drag = Some(drag);
        let rect = app.windows[0].layout.cells[0];
        app.update_drop_target(1, rect.x + rect.w / 2.0, rect.y_top + rect.h / 2.0);
        assert!(
            app.windows.iter().all(|w| w.drop_preview.is_none()),
            "a Nothing outcome must light no ghost"
        );
    }

    /// A grid at both caps cannot reshape: the edge-band drop
    /// degrades to append, and the resolution says so — previewing a
    /// half-pane split there would promise an impossible shape.
    #[test]
    fn capped_grids_resolve_edge_drops_to_append() {
        let mut panes: Vec<Pane> = (0..36).map(|i| Pane::new_vacant(100 + i, 80, 24)).collect();
        let extra = Pane::new_vacant(10, 80, 24);
        let mut app = app_with(vec![
            win(1, vec![extra]),
            win(2, panes.drain(..).collect()),
        ]);
        app.windows[1].grid_cols = 6;
        app.windows[1].grid_rows = 6;
        let drag = PaneDrag { from_wi: 0, idx: 0, start: (0.0, 0.0), active: true };
        let t = Some(DropTarget { wi: 1, pane_idx: 0, zone: DropZone::Right });
        assert_eq!(
            app.resolve_drop_outcome(&drag, t, 2),
            DropOutcome::Append { to_wi: 1 },
            "6×6 full: split is impossible, say append"
        );
    }

    /// Dropping on a dormant placeholder fills it — any zone.  The
    /// placeholder is consumed, the source slot goes dormant, no
    /// reshape anywhere.
    #[test]
    fn dropping_on_a_placeholder_fills_it() {
        let mut app = app_with(vec![
            win(1, vec![Pane::new_vacant(10, 80, 24), Pane::new_vacant(11, 80, 24)]),
            win(2, vec![Pane::new_vacant(20, 80, 24), Pane::new_dormant(80, 24)]),
        ]);
        let drag = PaneDrag { from_wi: 0, idx: 1, start: (0.0, 0.0), active: true };
        for zone in [DropZone::Left, DropZone::Center, DropZone::Bottom] {
            let t = Some(DropTarget { wi: 1, pane_idx: 1, zone });
            assert_eq!(
                app.resolve_drop_outcome(&drag, t, 2),
                DropOutcome::Fill { to_wi: 1, idx: 1 },
                "{zone:?} on a placeholder = fill it"
            );
        }
        app.fill_placeholder(0, 1, 1, 1);
        assert_eq!(app.windows[1].panes.len(), 2, "no new slot");
        assert_eq!(app.windows[1].panes[1].shelld_session_id(), Some(11));
        assert!(!app.windows[1].panes[1].is_dormant(), "placeholder consumed");
        assert!(app.windows[0].panes[1].is_dormant(), "source slot went dormant");
        assert_eq!(app.key_window, 1);
    }

    /// Chrome is laid out in its own unit, never in the window's
    /// backing scale — and no code path may put the display's number
    /// there.
    ///
    /// Measured 2026-08-11 on one menu rendered at both scales: the
    /// box went 180×184 → 356×368 while the label inside it stayed
    /// 12 px of ink.  The chrome constants are physical pixels, like
    /// the text they hold; multiplying them by the display factor only
    /// looked right where that factor happened to be 1.
    #[test]
    fn chrome_is_never_laid_out_in_the_display_scale() {
        let w = WindowState::new(3, Vec::new(), 0, 1, 1, 800.0, 600.0);
        assert_eq!(w.scale, marspot::ui::chrome_scale());
        assert_eq!(marspot::ui::chrome_scale(), 1.0, "chrome is authored in px");
        // The header the layout actually built follows the same unit.
        assert!(
            (w.layout.top_inset - HEADER_PT * marspot::ui::chrome_scale()).abs() < 1e-6,
            "header {} vs {}",
            w.layout.top_inset,
            HEADER_PT * marspot::ui::chrome_scale(),
        );
    }

    /// The modal's slot map is sized from the window's own grid, so a
    /// window created with a non-default shape starts consistent.
    #[test]
    fn card_slots_match_the_grid_the_window_was_built_with() {
        let w = WindowState::new(3, Vec::new(), 0, 4, 2, 800.0, 600.0);
        assert_eq!(w.card_slots.len(), 8);
        assert_eq!(w.pending_grid_cols, 4);
        assert_eq!(w.pending_grid_rows, 2);
    }

    /// Fake a registry entry for `sid` whose `shell_child_pid` is `pid`,
    /// which is all `resolve_pane_cwd` reads out of it.
    fn write_entry_with_child_pid(sid: u64, pid: i32) {
        sandbox_state_dir();
        let dir = marspot_term::session_registry::session_dir(sid);
        std::fs::create_dir_all(&dir).expect("session dir");
        std::fs::write(
            dir.join("entry.toml"),
            format!("id = {sid}\nshell_child_pid = {pid}\n"),
        )
        .expect("entry.toml");
    }

    fn cwd_string() -> String {
        std::env::current_dir()
            .expect("cwd")
            .to_string_lossy()
            .into_owned()
    }

    /// First resolve reports a change (nothing was cached); the second
    /// one must report *none*.  The sweep turns that bool into
    /// `needs_render`, so a `true` on an unchanged cwd would mean a
    /// frame per second forever with the user's hands off the keyboard.
    #[test]
    fn resolving_an_unchanged_cwd_reports_no_change() {
        let sid = 7701;
        write_entry_with_child_pid(sid, std::process::id() as i32);
        let mut app = app_with(vec![win(1, vec![Pane::new_vacant(sid, 80, 24)])]);

        assert!(app.resolve_pane_cwd(sid), "first resolve = new value");
        assert_eq!(app.pane_cwds.get(&sid), Some(&cwd_string()));
        assert_eq!(
            app.shell_child_pids.get(&sid).copied(),
            Some(std::process::id() as i32),
            "resolved pid gets cached so the sweep skips entry.toml"
        );
        assert!(!app.resolve_pane_cwd(sid), "unchanged cwd = no repaint owed");
    }

    /// A pid that no longer resolves must not blank the cached cwd (an
    /// L3 mid-execv briefly has no child), and must be dropped from the
    /// cache so the next attempt re-reads entry.toml.
    #[test]
    fn a_dead_cached_pid_is_dropped_and_the_entry_reread() {
        let sid = 7702;
        write_entry_with_child_pid(sid, std::process::id() as i32);
        let mut app = app_with(vec![win(1, vec![Pane::new_vacant(sid, 80, 24)])]);
        app.pane_cwds.insert(sid, "/somewhere/known".into());
        // Above any plausible pid: proc_pidinfo says "no such process".
        app.shell_child_pids.insert(sid, i32::MAX);

        assert!(
            app.resolve_pane_cwd(sid),
            "falls back to entry.toml and lands on the live cwd"
        );
        assert_eq!(app.pane_cwds.get(&sid), Some(&cwd_string()));
        assert_eq!(
            app.shell_child_pids.get(&sid).copied(),
            Some(std::process::id() as i32)
        );

        // Now make the entry unreadable too: the last known cwd has to
        // survive rather than degrade into an empty title.
        let _ = std::fs::remove_file(
            marspot_term::session_registry::session_dir(sid).join("entry.toml"),
        );
        app.shell_child_pids.insert(sid, i32::MAX);
        assert!(!app.resolve_pane_cwd(sid), "unresolvable = no change");
        assert_eq!(
            app.pane_cwds.get(&sid),
            Some(&cwd_string()),
            "an unresolvable pane keeps its last known cwd"
        );
        assert!(
            !app.shell_child_pids.contains_key(&sid),
            "a pid that stopped resolving must not stay cached"
        );
    }

    /// The sweep repaints a window only when one of its panes actually
    /// moved, and does nothing at all inside `CWD_SWEEP_INTERVAL`.
    #[test]
    fn sweep_repaints_only_on_a_real_move_and_respects_its_interval() {
        let sid = 7703;
        write_entry_with_child_pid(sid, std::process::id() as i32);
        let mut app = app_with(vec![win(1, vec![Pane::new_vacant(sid, 80, 24)])]);

        app.sweep_pane_cwds();
        assert_eq!(app.pane_cwds.get(&sid), Some(&cwd_string()));
        assert!(app.windows[0].needs_render, "first fill owes a frame");

        // Steady state: nothing moved, so nothing repaints.
        app.windows[0].needs_render = false;
        app.last_cwd_sweep = Instant::now() - CWD_SWEEP_INTERVAL;
        app.sweep_pane_cwds();
        assert!(
            !app.windows[0].needs_render,
            "an unchanged sweep must not wake the renderer"
        );

        // A pane that moved: the sweep notices and owes a frame.
        app.pane_cwds.insert(sid, "/moved/away".into());
        app.last_cwd_sweep = Instant::now() - CWD_SWEEP_INTERVAL;
        app.sweep_pane_cwds();
        assert_eq!(app.pane_cwds.get(&sid), Some(&cwd_string()));
        assert!(app.windows[0].needs_render, "a moved cwd owes a frame");

        // Same move again, but the interval has not elapsed: the sweep
        // is a no-op, poisoned value and all.
        app.windows[0].needs_render = false;
        app.pane_cwds.insert(sid, "/moved/away".into());
        app.sweep_pane_cwds();
        assert_eq!(
            app.pane_cwds.get(&sid).map(String::as_str),
            Some("/moved/away"),
            "sweep inside the interval must not issue syscalls"
        );
        assert!(!app.windows[0].needs_render);
    }

    /// The save rate limit may delay a persist, never drop one.  Pinned
    /// because dropping it is not hypothetical: the first live install
    /// of this sweep wrote `last_cwd = ""` for the second window's two
    /// panes and then never corrected it — their cwds were filled one
    /// sweep after boot, inside the gap that had just been consumed by
    /// the boot save, and no later sweep had anything new to report.
    #[test]
    fn a_move_inside_the_save_gap_is_deferred_not_dropped() {
        let sid = 7704;
        write_entry_with_child_pid(sid, std::process::id() as i32);
        let mut app = app_with(vec![win(1, vec![Pane::new_vacant(sid, 80, 24)])]);
        // Something else just saved (boot, spawn, focus change).
        app.last_cwd_save = Instant::now();

        app.sweep_pane_cwds();
        assert_eq!(app.pane_cwds.get(&sid), Some(&cwd_string()));
        assert!(
            app.cwd_save_pending,
            "the fill still owes a persist once the gap elapses"
        );

        // Later sweeps find nothing new; the debt must survive them.
        app.last_cwd_sweep = Instant::now() - CWD_SWEEP_INTERVAL;
        app.sweep_pane_cwds();
        assert!(app.cwd_save_pending, "an idle sweep must not clear the debt");

        app.last_cwd_sweep = Instant::now() - CWD_SWEEP_INTERVAL;
        app.last_cwd_save = Instant::now() - CWD_SAVE_MIN_GAP;
        app.sweep_pane_cwds();
        assert!(!app.cwd_save_pending, "gap elapsed → debt settled");
        let saved = marspot::state::read().expect("state written");
        assert_eq!(
            saved.windows[0].panes[0].last_cwd,
            cwd_string(),
            "the deferred save persists the cwd it was holding"
        );
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
            // POSIX shm names (`/msp-s-<id>`) are a GLOBAL namespace:
            // the sandbox isolates the filesystem but not shm.  With
            // every sandbox allocating ids from 1, two tests spawning
            // concurrently raced on `/msp-s-1` — create_region_named
            // unlink+O_EXCL either detached the other test's live
            // region or lost the O_EXCL race, and the spawn `expect`
            // blew up (the suite's one flaky).  Seeding the counter
            // with a per-process base keeps every test's id space —
            // and therefore its shm names — disjoint.  Production is
            // untouched: one state root, one monotonic counter.
            let sessions = dir.join("sessions");
            std::fs::create_dir_all(&sessions).unwrap();
            std::fs::write(
                sessions.join(".next_id"),
                format!("{}", std::process::id() as u64 * 100_000),
            )
            .unwrap();
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
                    flags: 0,
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

    /// Assembly binds panes to sessions, and nothing else: a pane's
    /// name is derived from its directory at render time
    /// (`marspot::pane_name`), so there is no title to carry across a
    /// boot.  What used to be tested here — saved title beats
    /// entry.toml — described a feature that no longer exists.
    #[test]
    fn assembly_binds_the_saved_slots_to_their_sessions() {
        let _sb = Sandbox::new("titles");
        for id in [4u64, 6] {
            write_session_entry(&dead_entry(id)).unwrap();
        }
        let s = saved(&[(4, ""), (6, "")]);
        let (tx, _rx) = mpsc::channel();
        let (panes, _) = assemble_panes_at_boot(Some(&s), 2, 60, 16, &tx, true, &Default::default());
        assert_eq!(pane_sids(&panes), vec![4, 6]);
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

    /// 2026-07-28 incident — the machine died with 176 live session
    /// processes.  The population cap turns "grow until the OS falls
    /// over" into "one spawn refuses, loudly".  Cap lowered via env so
    /// proving the refusal doesn't itself need a process storm.
    #[test]
    fn spawning_past_the_session_population_cap_refuses() {
        let _sb = Sandbox::new("popcap");
        // SAFETY: nextest is process-per-test.
        unsafe { std::env::set_var("MARSPOT_SESSION_CAP", "1") };
        let (tx, _rx) = mpsc::channel();
        let first_sid = reg::allocate_next_session_id().unwrap();
        let _first = spawn_l3_pane_with_cwd(60, 16, first_sid, "", &tx)
            .expect("first spawn fits under the cap");
        let second_sid = reg::allocate_next_session_id().unwrap();
        let err = match spawn_l3_pane_with_cwd(60, 16, second_sid, "", &tx) {
            Err(e) => e,
            Ok(_) => panic!("second spawn must refuse at cap 1"),
        };
        assert!(
            err.to_string().contains("hard cap"),
            "refusal must name the cap, got: {err}"
        );
        unsafe { std::env::remove_var("MARSPOT_SESSION_CAP") };
    }

    /// RFC-006 — a dormant slot restores AS a dormant slot: boot
    /// assembly must not reattach, resurrect, or spawn for it.
    #[test]
    fn a_dormant_slot_boots_as_a_placeholder() {
        let sb = Sandbox::new("dormant-boot");
        sb.break_session_bin(); // any spawn attempt would fail loudly
        let s = SavedWindowLayout {
            grid_cols: 2,
            grid_rows: 1,
            focused_idx: 0,
            panes: vec![
                SavedPane { sid: 700, ..Default::default() },
                SavedPane {
                    flags: marspot::state::PANE_FLAG_DORMANT,
                    ..Default::default()
                },
            ],
        };
        let (tx, _rx) = mpsc::channel();
        let (panes, reattached) =
            assemble_panes_at_boot(Some(&s), 2, 60, 16, &tx, true, &Default::default());
        assert_eq!(panes.len(), 2);
        assert!(!panes[0].is_dormant(), "live slot stays a session slot");
        assert!(panes[1].is_dormant(), "dormant slot is a placeholder again");
        assert!(reattached.is_empty());
    }

    /// 2026-07-28 incident — a session whose registry entry is gone
    /// must exit on its own.  L3s outlive their L2/L1 by design, so
    /// the entry is the only ownership record; every reaper works by
    /// removing it.  Before the deadman, a test script (or anything)
    /// that deleted the registry out from under live sessions created
    /// processes NOTHING could ever find or kill — 176 of them helped
    /// push the machine into a forced reboot.
    #[test]
    fn a_session_whose_registry_entry_is_deleted_exits_by_itself() {
        let _sb = Sandbox::new("deadman");
        let (tx, _rx) = mpsc::channel();
        let sid = reg::allocate_next_session_id().unwrap();
        let live = spawn_l3_pane_with_cwd(60, 16, sid, "", &tx)
            .expect("real L3 spawn (is marspot-session built?)");
        let pid = live.session().l3_pid().expect("spawned L3 has a pid");
        std::mem::forget(live); // Drop would SIGKILL it — the deadman must do the work
        assert!(reg::pid_is_live_session(pid));

        std::fs::remove_file(reg::session_entry_path(sid)).expect("remove entry");

        // Check cadence is 5 s; give it that plus scheduling margin.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            if !reg::pid_is_live_session(pid) {
                return; // exited on its own — the leak class is closed
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        unsafe { libc::kill(pid, libc::SIGKILL) };
        panic!("session {pid} outlived its deleted registry entry");
    }

    /// The whole `PaneResetMouseReporting` chain, against a real L3.
    ///
    /// Every piece of this was unit-tested separately — the codec, the
    /// terminal's reset, the pty_op step that sends it — and the thing
    /// those cannot tell you is whether the frame reaches the terminal
    /// that matters.  What went wrong on 2026-09-01 lived between the
    /// layers, not inside one.
    ///
    /// Drives a shell in a real session process into mouse tracking,
    /// checks the mirror L2 renders from agrees, then sends the frame
    /// and waits for the mirror to say it is off.
    #[test]
    fn a_real_session_gives_up_mouse_reporting_when_told() {
        // Short tag on purpose: it lands in the sandbox path, and the
        // L3's socket under it has to fit macOS's 104-byte `sun_path`.
        // `mouse-reset` overran it by a few bytes and the session died
        // at `uds_bind_failed` before it could register.
        let _sb = Sandbox::new("mreset");
        let (tx, _rx) = mpsc::channel();
        let sid = reg::allocate_next_session_id().unwrap();
        let mut pane = spawn_l3_pane_with_cwd(60, 16, sid, "", &tx)
            .expect("real L3 spawn (is marspot-session built?)");

        // The shell has to *print* the mode set for the terminal to
        // parse it: modes are set by the program's output, and
        // anything injected here is its input.  Retried because the
        // first bytes land before the shell is reading.
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut on = false;
        while std::time::Instant::now() < deadline {
            pane.session_mut()
                .forward_inject_input(b"printf '\\033[?1002h\\033[?1006h'\r");
            for _ in 0..40 {
                pane.pump();
                if pane.session().l3_mouse_tracking_active() {
                    on = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            if on {
                break;
            }
        }
        assert!(on, "shell never got as far as turning mouse tracking on");
        assert!(
            pane.session().l3_mouse_sgr_active(),
            "1006 rides with 1002; without it the wheel encodes the old way"
        );

        // What L1 sends once it has taken a pane's foreground program
        // down.  Nothing here kills anything — the point is that the
        // terminal drops the mode on the frame alone, because a signal
        // gives it nothing else to go on.
        pane.session_mut().forward_pane_reset_mouse_reporting();

        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            pane.pump();
            if !pane.session().l3_mouse_tracking_active() {
                assert!(
                    !pane.session().l3_mouse_sgr_active(),
                    "SGR encoding must go with it, or the next wheel is \
                     still a mouse report — just in the older form"
                );
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("L3 kept reporting mouse tracking after being told to stop");
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

    /// A `~/…` file link must reach `open(1)` already expanded.
    ///
    /// `open(1)` does no shell expansion — handed `~/x`, it looks for a
    /// directory literally named `~` under its cwd and reports the file
    /// missing.  linkify DOES expand `~` for its existence check, so
    /// such links underlined normally and then did nothing when
    /// clicked (2026-08-21 report).  Nothing surfaced the failure
    /// either: `spawn` succeeds as long as the process starts.
    #[test]
    fn a_tilde_file_link_is_expanded_before_open() {
        let home = std::env::var("HOME").expect("HOME");
        let arg = open_arg_for(marspot::grid_links::LinkKind::File, "~/Downloads/x.pdf")
            .expect("file links have an open action");
        assert!(
            !arg.contains('~'),
            "open(1) gets no shell: `~` must already be gone, got {arg:?}"
        );
        assert_eq!(arg, format!("{home}/Downloads/x.pdf"));

        // An absolute path is passed through untouched.
        let abs = open_arg_for(marspot::grid_links::LinkKind::File, "/tmp/y.log").unwrap();
        assert_eq!(abs, "/tmp/y.log");
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

/// A plugin's description of how the wheel reaches its program.
///
/// There is deliberately no "did we open it" flag.  The program leaves
/// its scrollable view on its own as well as by the user's key, and
/// `enter` is typically a toggle — measured, a second `Ctrl+T` closes
/// codex's transcript — so a remembered flag going stale would CLOSE
/// the view instead of opening it.  The state is read off the screen
/// instead: `marker` is text the program shows while the view is open.
#[derive(Clone, Debug)]
struct WheelKeys {
    /// Opens the program's scrollable view.
    enter: Vec<u8>,
    up: Vec<u8>,
    down: Vec<u8>,
    /// On-screen text meaning that view is currently open.
    marker: Vec<u8>,
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
    MouseDrag(f64, f64, u32, u32, f64, f64),
    /// `(window, drop_window)` — release ends the drag; the second id
    /// is the marspot window under the pointer at release (0 = none),
    /// resolved by L1.  RFC-005 step 5's pane drag lands there.
    MouseUp(u32, u32, f64, f64),
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
    SurfaceAttachWindow(u32, u32, f64, f64, f64, u32, Option<u32>, Option<f64>),
    /// `(window_id, lights_right_phys)` — chrome moved, pixels did not.
    WindowChrome(u32, f64),
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
    /// L1 → L2: how the wheel reaches this pane's program (RFC-008).
    PaneWheelKeys(u64, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>),
    /// L1 → L2: how far this pane has receded from active use.
    PaneRecede(u64, u32),
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
    /// L1 asks a pane to hold (or release) its picture.  Forwarded
    /// straight to the pane's L3 — the hold lives there so it survives
    /// this process being replaced by a silent update.
    PaneHoldGrid(u64, bool),
    /// A plugin says this pane's program prints unrendered markup.
    PaneRenderMarkup(u64, bool),
    /// L1 asks a pane to receive text as a paste.  Forwarded to the
    /// pane's L3, which is the only layer that knows whether the
    /// program in it has bracketed paste on.
    PaneInjectPaste(u64, String),
    /// L1 took a pane's foreground program down and is telling its L3
    /// to stop reporting mouse tracking as on.  Forwarded, not acted
    /// on here: the terminal whose modes these are lives in L3, and
    /// L2's `mouse_tracking_active` is only a mirror of what L3
    /// publishes.
    PaneResetMouseReporting(u64),
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

/// RFC-005 step 5 — an in-flight pane drag (see `CoreApp.pane_drag`).
#[derive(Clone, Copy, Debug)]
struct PaneDrag {
    from_wi: usize,
    idx: usize,
    /// Press position, physical px in the source window's space.
    start: (f64, f64),
    /// Slop exceeded: this press is a drag, not a click.
    active: bool,
}

/// How far (physical px) the pointer must travel from the press
/// before a title-bar hold becomes a pane drag.  Generous enough
/// that an ordinary click never trips it on a shaky hand.
const PANE_DRAG_SLOP_PHYS: f64 = 10.0;

/// RFC-006 §1 — where inside a hovered pane a drop would land.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DropZone {
    Left,
    Right,
    Top,
    Bottom,
    /// Center — swap with the hovered pane.
    Center,
}

/// The live drop target while a pane drag hovers a window.
#[derive(Clone, Copy, Debug, PartialEq)]
struct DropTarget {
    wi: usize,
    pane_idx: usize,
    zone: DropZone,
}

/// RFC-006 — what a release would DO, resolved from (drag, hover).
///
/// The single source of truth for both the ghost and the release:
/// `update_drop_target` renders it, `mouse_up` executes it, so the
/// preview can never promise what the release won't deliver.  The
/// field report that forced this: dragging a pane onto its own slot
/// showed a split ghost, but release (correctly) did nothing — the
/// ghost and the outcome were computed by two different pieces of
/// code with two different ideas of "no-op".
#[derive(Clone, Copy, Debug, PartialEq)]
enum DropOutcome {
    /// Insert into the target window beside the hovered pane.
    Split { to_wi: usize, at_idx: usize, zone: DropZone },
    /// Trade slots with the hovered pane.
    Swap { to_wi: usize, idx: usize },
    /// Land in the hovered dormant placeholder's slot.
    Fill { to_wi: usize, idx: usize },
    /// In a window but over no pane / grid at both caps: append.
    Append { to_wi: usize },
    /// Outside every marspot window: a new window at the point.
    NewWindow,
    /// Release changes nothing — and therefore previews nothing.
    Nothing,
}

/// RFC-006 §1 — zone geometry.  Edge bands are 25 % of the rect's
/// span on their axis, clamped to at least 48 physical px (slim panes
/// keep usable bands) and at most a third of the span (so the center
/// never vanishes).  Corners resolve to the axis with the deeper
/// penetration.
fn drop_zone_at(rect: &marspot::layout::CellRect, px: f64, py: f64) -> DropZone {
    let band_w = (rect.w * 0.25).max(48.0).min(rect.w / 3.0);
    let band_h = (rect.h * 0.25).max(48.0).min(rect.h / 3.0);
    let from_l = px - rect.x;
    let from_r = rect.x + rect.w - px;
    let from_t = py - rect.y_top;
    let from_b = rect.y_top + rect.h - py;
    // Penetration depth into each band; ≤ 0 = not in that band.
    let pen_l = band_w - from_l;
    let pen_r = band_w - from_r;
    let pen_t = band_h - from_t;
    let pen_b = band_h - from_b;
    let best_h = pen_l.max(pen_r);
    let best_v = pen_t.max(pen_b);
    if best_h <= 0.0 && best_v <= 0.0 {
        return DropZone::Center;
    }
    if best_h >= best_v {
        if pen_l >= pen_r { DropZone::Left } else { DropZone::Right }
    } else if pen_t >= pen_b {
        DropZone::Top
    } else {
        DropZone::Bottom
    }
}

/// The preview rect a zone paints: the half of the hovered pane a
/// split would occupy, or the whole pane for a swap.
fn drop_preview_rect(rect: &marspot::layout::CellRect, zone: DropZone) -> (f64, f64, f64, f64) {
    match zone {
        DropZone::Left => (rect.x, rect.y_top, rect.w / 2.0, rect.h),
        DropZone::Right => (rect.x + rect.w / 2.0, rect.y_top, rect.w / 2.0, rect.h),
        DropZone::Top => (rect.x, rect.y_top, rect.w, rect.h / 2.0),
        DropZone::Bottom => (rect.x, rect.y_top + rect.h / 2.0, rect.w, rect.h / 2.0),
        DropZone::Center => (rect.x, rect.y_top, rect.w, rect.h),
    }
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
        MsgType::MouseDrag => marspot::shell_proto::decode_mouse_drag(&f.payload)
            .ok()
            .map(|(x, y, _, win, hw, hx, hy)| CoreEvent::MouseDrag(x, y, win, hw, hx, hy)),
        MsgType::MouseUp => marspot::shell_proto::decode_mouse_up(&f.payload)
            .ok()
            .map(|(_, _, _, win, drop, dx, dy)| CoreEvent::MouseUp(win, drop, dx, dy)),
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
            .map(|(fr, bk, w, h, sc, win, slot, lights)| {
                CoreEvent::SurfaceAttachWindow(fr, bk, w, h, sc, win, slot, lights)
            }),
        MsgType::WindowChrome => decode_window_chrome(&f.payload)
            .ok()
            .map(|(win, lights)| CoreEvent::WindowChrome(win, lights)),
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
        MsgType::PaneRecede => marspot::shell_proto::decode_pane_recede(&f.payload)
            .ok()
            .map(|(sid, level)| CoreEvent::PaneRecede(sid, level)),
        MsgType::PaneBadge => marspot::shell_proto::decode_pane_badge(&f.payload)
            .ok()
            .map(|(sid, text)| CoreEvent::PaneBadge(sid, text)),
        MsgType::PaneWheelKeys => marspot::shell_proto::decode_pane_wheel_keys(&f.payload)
            .ok()
            .map(|(sid, enter, up, down, marker)| {
                CoreEvent::PaneWheelKeys(sid, enter, up, down, marker)
            }),
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
        MsgType::PaneInjectPaste => marspot::shell_proto::decode_pane_inject_paste(&f.payload)
            .ok()
            .map(|(sid, text)| CoreEvent::PaneInjectPaste(sid, text)),
        MsgType::PaneHoldGrid => marspot::shell_proto::decode_pane_hold_grid(&f.payload)
            .ok()
            .map(|(sid, on)| CoreEvent::PaneHoldGrid(sid, on)),
        MsgType::PaneRenderMarkup => marspot::shell_proto::decode_pane_render_markup(&f.payload)
            .ok()
            .map(|(sid, on)| CoreEvent::PaneRenderMarkup(sid, on)),
        MsgType::PaneResetMouseReporting => {
            marspot::shell_proto::decode_pane_reset_mouse_reporting(&f.payload)
                .ok()
                .map(CoreEvent::PaneResetMouseReporting)
        }
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

/// Absolute ceiling on live `marspot-session` processes this state
/// root may hold, counted against the registry at spawn time.
///
/// 2026-07-28 incident — the machine went down with 176 of them.
/// `SESSION_COUNT_HARD_CAP` (36) bounds panes per window, but nothing
/// bounded the PROCESS population: sessions outlive their windows by
/// design, so a crash-restore loop grew a new generation every cycle.
/// Refusing here turns "the system dies" into "one spawn fails and a
/// vacant pane says so" — the failure the architecture already knows
/// how to show.
const SESSION_PROCESS_HARD_CAP: usize = 64;

fn spawn_l3_with_cwd(
    cols: u16,
    rows: u16,
    session_id: u64,
    initial_cwd: &str,
    event_tx: &Sender<CoreEvent>,
) -> std::io::Result<L3Spawn> {
    // Population guard.  A readdir + one kill(0) per entry, only on
    // the spawn path — spawns are user-rate (boot, [+], revive), never
    // per-frame.  `MARSPOT_SESSION_CAP` overrides the cap for tests
    // (spawning 64 real processes to prove a refusal would itself be
    // a small process storm).
    let cap = std::env::var("MARSPOT_SESSION_CAP")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(SESSION_PROCESS_HARD_CAP);
    let live = marspot_term::session_registry::list_session_entries()
        .iter()
        .filter(|e| marspot_term::session_registry::pid_is_live_session(e.pid))
        .count();
    if live >= cap {
        lx_error!(
            "core.spawn.session_population_cap",
            "refusing to spawn: live session processes at hard cap",
            live = live,
            cap = cap,
            session = session_id
        );
        return Err(std::io::Error::new(
            std::io::ErrorKind::QuotaExceeded,
            format!("{live} live sessions >= hard cap {cap}"),
        ));
    }
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
                session_id = session_id
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
    /// This window's slot in the persisted lists — entry `frame_index`
    /// of `shell-state.bin`'s window list and of `window-state.bin`'s
    /// frame list, which is what pairs a window's layout with its
    /// geometry across a launch.
    ///
    /// Stable for the window's whole life, deliberately NOT its
    /// position in `self.windows`: closing a window used to shift
    /// every later window's slot down by one, so the survivors
    /// inherited each other's geometry.  L1 keeps the same invariant
    /// on its side (`ShellWindow::frame_index`).
    frame_index: usize,
    /// This window's paint target.  `None` between the window's birth
    /// and its first successful attach (a stale id from a mid-spawn
    /// surface rotation), during which the window simply isn't
    /// rendered — the other windows keep painting.
    surfaces: Option<WindowSurfaces>,
    /// Per-window frame-interval cap.  Global would let a busy window
    /// gate a quiet one's repaint, which is exactly the coupling the
    /// peer model forbids.
    last_render_at: Option<Instant>,
    /// The caret of a frame that is committed but not yet settled.
    ///
    /// It travels with the frame: announcing it before the surface is
    /// shown would point the IME at a position the user cannot see yet.
    pending_caret: Option<(f64, f64, f64, f64)>,
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
    /// Right edge of the OS's traffic-light cluster, physical px from
    /// the window's left edge; `Some(0.0)` when it is not on screen
    /// (full screen takes it away), **`None` until the shell has
    /// measured**.  The toolbar starts after it.
    ///
    /// `None` is not `Some(0.0)`: a window that has not been measured
    /// yet must keep clear of where the buttons normally are, or its
    /// first frames draw the toolbar on top of them.
    lights_right_phys: Option<f64>,
    /// Width the widest layout-modal card label needed, physical px,
    /// as of the last published frame.
    ///
    /// Cards size themselves to their labels, so the hit-test has to
    /// use the same number the painter did — and the number the
    /// painter used is the one on screen right now, which is exactly
    /// what a click is aimed at.  Recomputing it here from a
    /// different pass would be a second opinion about geometry the
    /// user can see.
    layout_modal_label_w: f64,
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
    /// The settings panel, open in this window.  Per window like
    /// every other modal — opening it in one must not blank another.
    settings_modal_open: bool,
    ime_preedit: String,
    /// Window physical dims, updated by Resize frames.
    w_phys: f64,
    h_phys: f64,
    /// Chrome's own unit — always `marspot::ui::chrome_scale()`.
    ///
    /// Named `scale` for the layout API it feeds, but deliberately
    /// **not** the window's `backingScaleFactor`: chrome constants are
    /// physical pixels, like the text inside them.  The shell still
    /// reports the real backing scale; the only thing that needs it is
    /// the window-button alignment, and that is measured.
    scale: f64,
    /// RFC-006 — the drop-preview ghost: the rect (x, y_top, w, h,
    /// physical px) a hovering pane drag would occupy on release,
    /// plus the outline-only flag (Append landings frame the content
    /// area instead of filling a half-pane).  Exactly one window has
    /// it at a time (the hovered one).
    drop_preview: Option<((f64, f64, f64, f64), bool)>,
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
    ) -> Self {
        Self {
            window_id,
            // Callers that know the slot set it right after; 0 is only
            // ever correct for the boot window, which is entry 0.
            frame_index: 0,
            surfaces: None,
            last_render_at: None,
            pending_caret: None,
            painted_once: false,
            render: marspot::render_metal::WindowRender::new(),
            layout: Layout::build(
                w_phys,
                h_phys,
                0.0,
                HEADER_PT * marspot::ui::chrome_scale(),
                CELL_TITLE_PT * marspot::ui::chrome_scale(),
                3,
                3,
                8.0,
                16.0,
            ),
            panes,
            focused_idx,
            selection: None,
            selection_dragging: false,
            grid_cols,
            grid_rows,
            layout_modal_open: false,
            lights_right_phys: None,
            layout_modal_label_w: 0.0,
            context_menu: None,
            pending_grid_cols: grid_cols,
            pending_grid_rows: grid_rows,
            card_slots: (0..(grid_cols * grid_rows)).collect(),
            layout_drag: None,
            sidebar_collapsed: true,
            hover_chrome_btn: None,
            process_panel: None,
            cc_usage_modal: None,
            settings_modal_open: false,
            drop_preview: None,
            ime_preedit: String::new(),
            w_phys,
            h_phys,
            scale: marspot::ui::chrome_scale(),
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
    /// RFC-008 — per-session wheel key declarations from a plugin.
    /// Whether the view is open is read from the screen (`marker`),
    /// never remembered.  Nothing here ever closes it: the user asked
    /// for the wheel to take them in but never to throw them out.
    pane_wheel_keys: std::collections::HashMap<u64, WheelKeys>,
    /// When this pane was last sent the plugin's `enter` key.  The
    /// marker that says the view opened takes a repaint to appear —
    /// 59 ms in the best case measured — and a trackpad delivers ticks
    /// far faster than that, so without this every tick inside the
    /// window sent the toggle again and closed what the one before it
    /// opened.  See `wheel_marker::should_send_enter`.
    pane_wheel_enter_at: std::collections::HashMap<u64, std::time::Instant>,
    /// Last observed open/closed state per pane, so the log carries one
    /// line per transition instead of one per wheel event.
    pane_wheel_open: std::collections::HashMap<u64, bool>,
    /// Per-shelld-session plugin-set title, set via `MsgType::PaneTitle`.
    /// Inserts into the title resolution chain ABOVE cwd basename,
    /// BELOW user-set custom title.  Empty payload removes the entry.
    pane_titles: std::collections::HashMap<u64, String>,
    /// Last-known cwd of each pane's shell, keyed by shelld_session_id.
    /// Read by the title placeholder chain (`Path::file_name` of the
    /// cached path → basename string) and persisted as `last_cwd` so a
    /// pane whose L3 died respawns in the same project.  Written only
    /// by `resolve_pane_cwd`, which the sweep drives — the render path
    /// is a pure reader and never issues a syscall.
    pane_cwds: std::collections::HashMap<u64, String>,
    /// `entry.toml`'s `shell_child_pid` per session, cached so the
    /// once-per-second sweep costs one `proc_pidinfo` per pane instead
    /// of also re-opening + re-parsing the registry entry.  Dropped for
    /// a session as soon as its pid stops resolving (L3 execv, shell
    /// restart), which makes the next sweep re-read the entry.
    shell_child_pids: std::collections::HashMap<u64, i32>,
    /// Frames queued by event handlers (mouse_down etc.) to be
    /// written to the control socket by the main loop.  Avoids
    /// reaching the writer from inside the trait callbacks where
    /// the borrow tree doesn't permit it.
    pending_to_shell: Vec<(MsgType, Vec<u8>)>,
    /// Per window: the focused session id L1 has already been told
    /// about, so `PaneFocused` is emitted on change rather than every
    /// loop iteration.
    last_focus_notified: std::collections::HashMap<usize, u64>,
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
    /// When the last `sweep_pane_cwds` ran.  Gates the sweep to
    /// `CWD_SWEEP_INTERVAL` regardless of how often the loop wakes.
    last_cwd_sweep: Instant,
    /// When the cwd sweep last persisted state — see `CWD_SAVE_MIN_GAP`.
    last_cwd_save: Instant,
    /// A cwd move the sweep saw but could not persist yet, because the
    /// previous save was inside `CWD_SAVE_MIN_GAP`.  Sticky: the rate
    /// limit may delay a save, never drop one.  Without it, a window
    /// adopted one sweep after boot (the second window's restore) had
    /// its panes' cwds filled in memory and then persisted as empty
    /// strings forever, since later sweeps found nothing new to report.
    cwd_save_pending: bool,
    /// Sessions with a reconnect already in flight.  Without this, a
    /// burst of EOFs for one session would spawn a thread each.
    reconnecting: std::collections::HashSet<u64>,
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
    /// RFC-005 step 5 — a title-bar press that may become a pane
    /// drag.  Armed on mouse-down over a pane title; becomes `active`
    /// once the pointer travels past the slop radius; resolved on
    /// mouse-up (active + over another window → move; active
    /// elsewhere → cancel; never active → the click's original
    /// meaning, entering title edit).  One at a time, app-wide — a
    /// drag spans windows by nature.
    pane_drag: Option<PaneDrag>,
    /// Where the active pane drag would land right now (None = over
    /// nothing droppable).  Drives the per-window `drop_preview`.
    drop_target: Option<DropTarget>,
    /// RFC-005 step 5 — a pane parked mid-"Move to New Window": its
    /// sid, waiting for L1 to open the window.  The next
    /// `SurfaceAttachWindow` with an unseen id claims it (checked
    /// before the saved-window restore queue — a user action outranks
    /// a boot leftover).
    pending_move_sid: Option<u64>,
    /// RFC-005 step 6b — saved windows past the boot one, waiting for
    /// L1 to reopen them.  Each `SurfaceAttachWindow` for an unseen id
    /// pops the front record, so the queue is also what distinguishes
    /// "restoring a window" from "the user pressed Cmd-N".
    saved_windows: std::collections::VecDeque<(usize, marspot::state::SavedWindowLayout)>,
    /// Windows the user closed while they still held live panes,
    /// keyed by the slot they occupied.
    ///
    /// Closing a window is *putting it away*, not throwing it out: its
    /// L3s keep running and its record keeps being written to
    /// `shell-state.bin`, so the next launch brings the window back
    /// with its sessions attached.  The way to actually be rid of a
    /// window is to close its panes — a window whose last pane closed
    /// has nothing to park and is discarded.
    parked_windows: Vec<(usize, marspot::state::SavedWindowLayout)>,
    /// Dev seam only (`MARSPOT_DEV_CLOSE_PANES`): how many more panes
    /// to close, and when the next one is due.  Unset in the installed
    /// app; see `dev_drive_close_panes`.
    dev_close_panes: usize,
    dev_close_panes_at: Option<Instant>,
    /// Dev seam only (`MARSPOT_DEV_OPEN_SETTINGS`): when to open the
    /// settings panel, cleared once it has.  See
    /// `dev_drive_open_settings`.
    dev_open_settings_at: Option<Instant>,
    /// Set once the user closed the last pane of the last window.  The
    /// saved layout has been deleted at that point and the app is on
    /// its way out; any later save would resurrect what they just
    /// dismantled.
    layout_discarded: bool,
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
    /// Take back any local-echo guess that has gone unanswered past
    /// its deadline.  Bytes are what normally settle a prediction, and
    /// a program can send none at all — a `sudo` password prompt
    /// echoes nothing until Enter, and the guesses would otherwise
    /// stay painted, showing what was typed.
    fn expire_stale_predictions(&mut self) {
        for w in self.windows.iter_mut() {
            for pane in w.panes.iter_mut() {
                // L3 panes keep their terminal in the session
                // process — only a pane that has one in this address
                // space can be swept here.
                let Some(t) = pane.session_mut().terminal_mut_opt() else {
                    continue;
                };
                if t.expire_predictions() {
                    w.needs_render = true;
                }
            }
        }
    }

    /// True while any pane is still showing an unconfirmed guess —
    /// the loop's cue to wake on a timer rather than sleep for a
    /// second waiting for a byte that may never come.
    fn any_prediction_pending(&self) -> bool {
        self.windows
            .iter()
            .flat_map(|w| w.panes.iter())
            .any(|p| {
                p.session()
                    .terminal_opt()
                    .is_some_and(|t| t.predictions_pending())
            })
    }

    /// the pane may well be in one that is not focused.
    fn inject_input(&mut self, shelld_session_id: u64, bytes: &[u8]) {
        for pane in self.windows.iter_mut().flat_map(|w| w.panes.iter_mut()) {
            if pane.session().l3_session_id() == Some(shelld_session_id) {
                pane.session_mut().forward_inject_input(bytes);
                return;
            }
        }
    }

    /// Pass pasted text down to the pane's own L3.
    fn forward_pane_paste(&mut self, shelld_session_id: u64, text: &str) {
        for pane in self.windows.iter_mut().flat_map(|w| w.panes.iter_mut()) {
            if pane.session().l3_session_id() == Some(shelld_session_id) {
                pane.session_mut().forward_paste(text);
                return;
            }
        }
    }

    /// Pass a grid hold down to the pane's own L3.
    ///
    /// L2 does not act on it: the point of the hold is that it belongs
    /// to the session, not to whichever core is currently drawing it.
    fn forward_pane_hold_grid(&mut self, shelld_session_id: u64, on: bool) {
        for pane in self.windows.iter_mut().flat_map(|w| w.panes.iter_mut()) {
            if pane.session().l3_session_id() == Some(shelld_session_id) {
                pane.session_mut().forward_pane_hold_grid(on);
                return;
            }
        }
    }

    /// Pass a plugin's markup declaration down to the pane's L3.
    ///
    /// L2 does not act on it: the terminal that would draw the markup
    /// lives in L3, and L2's grid is a mirror of what L3 publishes.
    fn forward_pane_render_markup(&mut self, shelld_session_id: u64, on: bool) {
        for pane in self.windows.iter_mut().flat_map(|w| w.panes.iter_mut()) {
            if pane.session().l3_session_id() == Some(shelld_session_id) {
                pane.session_mut().forward_pane_render_markup(on);
                return;
            }
        }
        // Named a pane this core does not have.  Worth a line: the
        // declaration is re-sent every tick, so a steady stream of
        // these means the sid the plugin uses and the one the pane
        // answers to have drifted apart.
        lx_debug!(
            "core.pane_render_markup.no_pane",
            "declaration named a session with no pane here",
            session_id = shelld_session_id
        );
    }

    /// Tell the pane's L3 that whatever had the foreground is gone.
    ///
    /// Same shape as the hold: L2 does not act on it, because the
    /// terminal that holds the mode lives in L3 and L2's copy is a
    /// mirror of L3's next publish.
    fn forward_pane_reset_mouse_reporting(&mut self, shelld_session_id: u64) {
        for pane in self.windows.iter_mut().flat_map(|w| w.panes.iter_mut()) {
            if pane.session().l3_session_id() == Some(shelld_session_id) {
                pane.session_mut().forward_pane_reset_mouse_reporting();
                return;
            }
        }
    }

    /// How far this pane has receded from active use.  Repaints only
    /// when the value actually moves — L1 sends on change, but a
    /// repeated value after a core swap must not cost a frame.
    fn set_pane_recede(&mut self, sid: u64, level: u32) {
        let Some((wi, idx)) = self.find_pane_by_sid(sid) else {
            return;
        };
        let pane = &mut win!(self, wi).panes[idx];
        if pane.recede == level {
            return;
        }
        pane.recede = level;
        win!(self, wi).needs_render = true;
    }

    /// Record (or clear) a plugin's wheel-key declaration.
    ///
    /// An empty `up` clears the declaration — the program has left
    /// that pane, so the wheel goes back to the terminal's routing.
    fn set_pane_wheel_keys(
        &mut self,
        sid: u64,
        enter: Vec<u8>,
        up: Vec<u8>,
        down: Vec<u8>,
        marker: Vec<u8>,
    ) {
        if up.is_empty() {
            if self.pane_wheel_keys.remove(&sid).is_some() {
                lx_info!("core.pane_wheel_keys.cleared", &format!("sid={sid}"));
            }
            return;
        }
        lx_info!(
            "core.pane_wheel_keys.set",
            &format!(
                "sid={sid} enter={enter:?} up={up:?} down={down:?} marker={marker:?}"
            )
        );
        self.pane_wheel_keys
            .insert(sid, WheelKeys { enter, up, down, marker });
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
            win!(self, wi).lights_right_phys,
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
    /// Re-read one session's shell cwd into `pane_cwds`.  Returns
    /// `true` when the stored path actually changed — callers use that
    /// to decide whether a repaint is owed, so a sweep over N unchanged
    /// panes costs N syscalls and zero frames.
    ///
    /// The pid comes from `shell_child_pids`, refilled from entry.toml
    /// only when it is missing or has stopped resolving.  A pid that
    /// fails to resolve leaves the last known cwd in place: an L3
    /// mid-execv has no live child for a moment, and blanking the title
    /// for that moment would be a worse lie than a slightly old one.
    fn resolve_pane_cwd(&mut self, sid: u64) -> bool {
        let cached_pid = self.shell_child_pids.get(&sid).copied();
        let mut path = cached_pid.and_then(marspot::pidtree::proc_cwd);
        if path.is_none() {
            // Either no pid cached yet, or the cached one is gone
            // (shell replaced, L3 execv'd).  One entry.toml read, then
            // retry — and only cache the pid once it has resolved.
            self.shell_child_pids.remove(&sid);
            if let Some(pid) = read_shell_child_pid(sid) {
                path = marspot::pidtree::proc_cwd(pid);
                if path.is_some() {
                    self.shell_child_pids.insert(sid, pid);
                }
            }
        }
        let Some(path) = path else { return false };
        let next = path.to_string_lossy().into_owned();
        if self.pane_cwds.get(&sid).is_some_and(|prev| *prev == next) {
            return false;
        }
        // INFO, not DEBUG: the runtime default level is Info, so a
        // DEBUG line does not exist on a real machine — and this is the
        // first thing worth reading when a title looks wrong.  Rate is
        // capped by construction at one line per pane per sweep.
        lx_info!(
            "core.pane_cwd.changed",
            "pane cwd moved; title placeholder follows",
            sid = sid,
            cwd = next.as_str()
        );
        self.pane_cwds.insert(sid, next);
        true
    }

    /// Re-read every pane's cwd, at most once per `CWD_SWEEP_INTERVAL`.
    /// Driven from the main loop's periodic block, so it rides whatever
    /// wake the loop already had (PTY output while the user works, the
    /// 1 s idle timeout otherwise) rather than owning a timer.
    ///
    /// Only the windows whose titles actually changed are marked dirty.
    /// Emit `PaneFocused` for any window whose focused pane changed
    /// since the last check.  Cheap enough to call every loop
    /// iteration: one map lookup per window, and nothing on the wire
    /// unless focus actually moved.
    fn notify_pane_focus_changes(&mut self) {
        for wi in 0..self.windows.len() {
            let sid = win!(self, wi)
                .panes
                .get(win!(self, wi).focused_idx)
                .and_then(|p| p.shelld_session_id());
            let Some(sid) = sid else { continue };
            if self.last_focus_notified.get(&wi) == Some(&sid) {
                continue;
            }
            self.last_focus_notified.insert(wi, sid);
            self.pending_to_shell.push((
                MsgType::PaneFocused,
                marspot::shell_proto::encode_pane_focused(sid),
            ));
        }
    }

    fn sweep_pane_cwds(&mut self) {
        if self.last_cwd_sweep.elapsed() < CWD_SWEEP_INTERVAL {
            return;
        }
        self.last_cwd_sweep = Instant::now();
        for wi in 0..self.windows.len() {
            let sids: Vec<u64> = win!(self, wi)
                .panes
                .iter()
                .filter_map(|p| p.shelld_session_id())
                .collect();
            let mut window_changed = false;
            for sid in sids {
                if self.resolve_pane_cwd(sid) {
                    window_changed = true;
                }
            }
            if window_changed {
                win!(self, wi).needs_render = true;
                self.cwd_save_pending = true;
            }
        }
        // `last_cwd` in the saved state feeds the respawn cwd on cold
        // boot, so a move wants persisting — but rate-limited, since a
        // cd-ing script must not turn into one fsync per second.  The
        // pending flag makes the limit a delay rather than a drop.
        if self.cwd_save_pending && self.last_cwd_save.elapsed() >= CWD_SAVE_MIN_GAP {
            self.last_cwd_save = Instant::now();
            self.cwd_save_pending = false;
            self.save_session_state();
        }
    }

    /// F3+6 — snapshot every persistable bit of state to
    /// `shell-state.bin`.  Called from spawn / close / focus-change /
    /// layout-apply / title-commit so a hard kill leaves a recent
    /// state on disk.  ~50 us per call (memcpy + atomic rename); no
    /// debounce because we never call this on the render hot path.
    fn save_session_state(&self) {
        use marspot::state::SavedState;
        // The user closed the last pane of the last window — the saved
        // layout is gone on purpose and this process is being torn
        // down.  Writing now would put it back.
        if self.layout_discarded {
            return;
        }
        // RFC-005 step 6 — every window, in slot order.  Saving only
        // the key window is what made opening a second window
        // destructive: the new window became key the instant it
        // appeared, and the next save replaced a 16-pane record with
        // its single pane.
        //
        // Parked windows (closed, but still holding live sessions) are
        // merged back in at the slot they had, so their geometry in
        // `window-state.bin` still lines up and the next launch brings
        // them back where they were.
        let mut slots: Vec<(usize, marspot::state::SavedWindowLayout)> = self
            .windows
            .iter()
            .map(|w| (w.frame_index, self.window_layout_record(w)))
            .chain(self.parked_windows.iter().cloned())
            // Records whose window L1 has not opened yet are part of
            // the layout too.  Without them the boot save — which runs
            // before any restore lands — rewrote the file with just
            // the boot window, and a crash in that gap lost every
            // other window.
            .chain(self.saved_windows.iter().cloned())
            .collect();
        slots.sort_by_key(|(slot, _)| *slot);
        // `key_window` indexes the live list; the saved list is the
        // merged one, so translate through the slot.
        let key_slot = self
            .windows
            .get(self.key_window)
            .map(|w| w.frame_index)
            .unwrap_or(0);
        let key_window = slots
            .iter()
            .position(|(slot, _)| *slot == key_slot)
            .unwrap_or(0) as u16;
        let saved = SavedState {
            windows: slots.into_iter().map(|(_, r)| r).collect(),
            key_window,
        };
        if let Err(e) = marspot::state::write(&saved) {
            lx_warn!(
                "core.state_file.write_failed",
                &format!("{e}; saved state not persisted this tick")
            );
        }
    }

    /// Dev seam (`MARSPOT_DEV_OPEN_SETTINGS`) — see the call site.
    fn dev_drive_open_settings(&mut self) {
        match self.dev_open_settings_at {
            Some(t) if Instant::now() >= t => {}
            _ => return,
        }
        self.dev_open_settings_at = None;
        let wi = self.key_window.min(self.windows.len().saturating_sub(1));
        self.toggle_settings_modal(wi);
    }

    /// Dev seam (`MARSPOT_DEV_CLOSE_PANES=n`) — see the call site.
    fn dev_drive_close_panes(&mut self) {
        if self.dev_close_panes == 0 {
            return;
        }
        match self.dev_close_panes_at {
            Some(t) if Instant::now() < t => return,
            _ => {}
        }
        self.dev_close_panes -= 1;
        self.dev_close_panes_at = Some(Instant::now() + Duration::from_secs(1));
        let wi = self.key_window.min(self.windows.len().saturating_sub(1));
        let idx = win!(self, wi).focused_idx;
        lx_event!(
            "DEV_CLOSE_PANE",
            "closing a pane (dev seam)",
            window_id = win!(self, wi).window_id,
            pane_idx = idx as u32,
            remaining = self.dev_close_panes as u32
        );
        self.close_session(wi, idx);
        if wi < self.windows.len() {
            self.rebuild_layout(wi);
        }
    }

    /// Close the gap a discarded window left: everything above it
    /// moves down one.  Mirrored by `ShellApp::forget_slot`, which
    /// does the same to `window-state.bin` — the two lists are paired
    /// by position, so they compact together or not at all.
    fn forget_slot(&mut self, slot: usize) {
        for w in &mut self.windows {
            if w.frame_index > slot {
                w.frame_index -= 1;
            }
        }
        for (s, _) in &mut self.parked_windows {
            if *s > slot {
                *s -= 1;
            }
        }
        for (s, _) in &mut self.saved_windows {
            if *s > slot {
                *s -= 1;
            }
        }
    }

    /// The slot for a window with no saved record: the one L1 named,
    /// unless something already holds it.
    fn slot_for_new_window(&self, announced: Option<usize>) -> usize {
        let taken = |s: usize| {
            self.windows.iter().any(|w| w.frame_index == s)
                || self.parked_windows.iter().any(|(q, _)| *q == s)
                || self.saved_windows.iter().any(|(q, _)| *q == s)
        };
        match announced {
            Some(s) if !taken(s) => s,
            _ => self.next_frame_index(),
        }
    }

    /// The slot a brand-new window takes: one past every slot spoken
    /// for, whether by an open window, a parked one, or a saved record
    /// still waiting for L1 to reopen it.  Monotonic, so a new window
    /// can never land on top of a window that is merely away.
    fn next_frame_index(&self) -> usize {
        let live = self.windows.iter().map(|w| w.frame_index);
        let parked = self.parked_windows.iter().map(|(slot, _)| *slot);
        let pending = self.saved_windows.iter().map(|(slot, _)| *slot);
        live.chain(parked)
            .chain(pending)
            .max()
            .map(|m| m + 1)
            .unwrap_or(0)
    }

    /// One window's persistable shape — grid, focus, and its panes in
    /// slot order.  Shared by the live save and by the snapshot taken
    /// when a window is parked, so a parked window is byte-identical
    /// to what it would have saved while open.
    fn window_layout_record(
        &self,
        w: &WindowState,
    ) -> marspot::state::SavedWindowLayout {
        use marspot::state::{SavedPane, SavedWindowLayout};
        SavedWindowLayout {
            grid_cols: w.grid_cols as u16,
            grid_rows: w.grid_rows as u16,
            focused_idx: w.focused_idx as u16,
            panes: w
                .panes
                .iter()
                .map(|p| {
                    let sid = p.shelld_session_id().unwrap_or(0);
                    // The format keeps the field so an older build can
                    // still read this file; nothing sets it.
                    let custom_title = String::new();
                    let last_cwd =
                        self.pane_cwds.get(&sid).cloned().unwrap_or_default();
                    let flags = if p.is_dormant() {
                        marspot::state::PANE_FLAG_DORMANT
                    } else {
                        0
                    };
                    SavedPane { sid, flags, custom_title, last_cwd }
                })
                .collect(),
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
                lx_debug!(
                    "core.badge_menu.requested",
                    "badge right-click; asked L1 for the menu",
                    pane = i,
                    session = sid
                );
                self.pending_to_shell
                    .push((MsgType::PaneBadgeMenuRequest, payload));
                return;
            }
            // Hit the badge but the pane has no session to ask about.
            // Silent before: the click fell through to the generic
            // menu and looked like nothing happened.
            lx_warn!(
                "core.badge_menu.no_session",
                "badge hit but pane has no session id; falling through",
                pane = i
            );
        } else if let Some(miss) = self.badge_miss_report(wi, x_phys, y_phys) {
            // A right-click inside a title strip that shows a badge,
            // yet missed it.  Says where the badge was thought to be
            // versus where the click landed, because the geometry is
            // computed twice — here and in the renderer — from the
            // same inputs, and nothing was checking that they agree.
            lx_warn!("core.badge_hit_miss", &miss);
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
            // The plugin that owns the badge had nothing to offer.
            // Silent until now, which made a dead right-click
            // indistinguishable from a missed hit-test.
            lx_warn!(
                "core.badge_menu_empty",
                &format!("shelld_session={sid} replied with no items")
            );
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
                // Never disabled: closing a window's last pane closes
                // the window, and the last window's last pane quits.
                let close = MenuItem::entry(
                    "Close pane", ContextMenuAction::ClosePane.tag(),
                );
                let copy = MenuItem::entry(
                    "Copy", ContextMenuAction::CopySelection.tag(),
                ).with_shortcut("⌘C");
                let mut items = vec![
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
                ];
                items.push(MenuItem::divider());
                self.push_move_to_window_items(wi, &mut items);
                items
            }
            ContextRegion::SidebarSlot(_) => {
                let close = MenuItem::entry(
                    "Close pane", ContextMenuAction::ClosePane.tag(),
                );
                let mut items = vec![
                    MenuItem::divider(),
                    close,
                ];
                items.push(MenuItem::divider());
                self.push_move_to_window_items(wi, &mut items);
                items
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

    /// RFC-005 step 5 — the cross-window entries of a pane's context
    /// menu: one "Move to Window N" per OTHER window, plus "Move to
    /// New Window".  Window numbers are 1-based creation order, the
    /// same order the sidebar of each window implies.
    fn push_move_to_window_items(
        &self,
        wi: usize,
        items: &mut Vec<marspot::ui::components::MenuItem>,
    ) {
        use marspot::ui::components::MenuItem;
        for (ti, w) in self.windows.iter().enumerate() {
            if ti == wi {
                continue;
            }
            items.push(MenuItem::entry(
                &format!("Move to Window {}", ti + 1),
                MOVE_TO_WINDOW_TAG_BASE + ti as u32,
            ));
            let _ = w;
        }
        // Moving the only pane of the only window to a "new" window
        // would just rebuild the same state with a window flash.
        let pointless =
            self.windows.len() == 1 && win!(self, wi).panes.len() <= 1;
        let mi = MenuItem::entry("Move to New Window", MOVE_TO_NEW_WINDOW_TAG);
        items.push(if pointless { mi.disabled() } else { mi });
    }

    /// Move one pane between windows — RFC-005 step 5's whole
    /// mechanism: a `Vec` move.  Everything per-pane travels inside
    /// the `Pane` value (pinned by
    /// `moving_a_pane_between_windows_carries_its_state`); the L3
    /// never notices.  Source window state that referenced the pane
    /// by index (selection, title edit, focus) is repaired; the
    /// target window appends, focuses it, and becomes key.
    fn move_pane_to_window(&mut self, from_wi: usize, idx: usize, to_wi: usize) {
        if from_wi == to_wi
            || from_wi >= self.windows.len()
            || to_wi >= self.windows.len()
            || idx >= win!(self, from_wi).panes.len()
        {
            return;
        }
        // RFC-006 §3 — a move-out leaves a DORMANT PLACEHOLDER, not a
        // hole: the user is arranging space, and the arrangement they
        // left behind holds still.  Nothing shifts, so no index
        // arithmetic; only state that pointed AT the moved pane is
        // dropped.  (Close keeps its compacting semantics — close
        // means "done with this space"; move means "keep my layout".)
        let (pc, pr) = {
            let g = win!(self, from_wi).panes[idx].session().grid();
            (g.cols(), g.rows())
        };
        let pane = std::mem::replace(
            &mut win!(self, from_wi).panes[idx],
            Pane::new_dormant(pc, pr),
        );
        let w = &mut win!(self, from_wi);
        if w.focused_idx == idx {
            // Focus the nearest live pane; a placeholder is space,
            // not a focus target.
            w.focused_idx = (0..w.panes.len())
                .filter(|&i| !w.panes[i].is_dormant())
                .min_by_key(|&i| i.abs_diff(idx))
                .unwrap_or(idx);
        }
        if let Some(sel) = w.selection {
            if sel.session_idx == idx {
                w.selection = None;
                w.selection_dragging = false;
            }
        }
        // Landing: appended (sidebar overflows if the grid is full),
        // focused, and the target becomes the key window.
        let t = &mut win!(self, to_wi);
        t.panes.push(pane);
        t.focused_idx = t.panes.len() - 1;
        self.key_window = to_wi;
        self.rebuild_layout(from_wi);
        self.rebuild_layout(to_wi);
        lx_event!(
            "PANE_MOVED",
            "pane moved between windows",
            from_window = win!(self, from_wi).window_id,
            to_window = win!(self, to_wi).window_id
        );
        self.save_session_state();
        // An empty window closes itself — L1 owns windows, so ask.
        self.request_close_if_empty(from_wi);
    }

    /// Move a pane into a window that does not exist yet.  The pane is
    /// parked by sid; L1 opens a fresh window (user action — bypasses
    /// the restore gates) and the `SurfaceAttachWindow` that follows
    /// lands in `adopt_window`, which sees the parked sid and builds
    /// the window around the MOVED pane instead of spawning one.
    fn move_pane_to_new_window(&mut self, from_wi: usize, idx: usize) {
        let Some(sid) = win!(self, from_wi)
            .panes
            .get(idx)
            .and_then(|p| p.shelld_session_id())
        else {
            return;
        };
        self.pending_move_sid = Some(sid);
        self.pending_to_shell.push((
            MsgType::WindowOpenRequest,
            marspot::shell_proto::encode_window_open_request(
                marspot::shell_proto::WINDOW_OPEN_USER,
            ),
        ));
        lx_event!(
            "PANE_MOVE_NEW_WINDOW_REQUESTED",
            "asked L1 for a fresh window to move a pane into",
            session = sid
        );
    }

    /// RFC-006 — resolve what a release at `target` would do for the
    /// pane being dragged.  Pure decision logic; both the ghost and
    /// the release go through it (single source of truth — a `Nothing`
    /// previews nothing and does nothing).
    fn resolve_drop_outcome(
        &self,
        drag: &PaneDrag,
        target: Option<DropTarget>,
        drop_window_id: u32,
    ) -> DropOutcome {
        let Some(t) = target else {
            // No pane under the pointer.  In a foreign window that is
            // the append landing; in the own window it is a no-op; in
            // no window at all it births one.
            return match self.window_index(drop_window_id) {
                Some(to_wi) if to_wi != drag.from_wi => DropOutcome::Append { to_wi },
                Some(_) => DropOutcome::Nothing,
                None if drop_window_id == 0 => DropOutcome::NewWindow,
                None => DropOutcome::Nothing,
            };
        };
        let same_window = t.wi == drag.from_wi;
        let hovered_dormant = win!(self, t.wi)
            .panes
            .get(t.pane_idx)
            .is_some_and(|p| p.is_dormant());
        // A placeholder is an empty slot asking to be filled — every
        // zone of it means "put the pane HERE" (splitting beside
        // emptiness would be pedantry).
        if hovered_dormant {
            if same_window && t.pane_idx == drag.idx {
                return DropOutcome::Nothing; // cannot happen (dormant isn't draggable), but stay total
            }
            return DropOutcome::Fill { to_wi: t.wi, idx: t.pane_idx };
        }
        match t.zone {
            DropZone::Center => {
                if same_window && t.pane_idx == drag.idx {
                    DropOutcome::Nothing // swapping with yourself
                } else {
                    DropOutcome::Swap { to_wi: t.wi, idx: t.pane_idx }
                }
            }
            zone => {
                if same_window {
                    // Simulate the remove+insert: if the pane would
                    // come back to its own slot, the release changes
                    // nothing — own edges, AND the near edges of the
                    // neighbours (pane i dropped on i+1's Left band
                    // re-inserts at i).
                    let at = if drag.idx < t.pane_idx {
                        t.pane_idx - 1
                    } else {
                        t.pane_idx
                    };
                    let cols = win!(self, t.wi).grid_cols;
                    let n_after_remove = win!(self, t.wi).panes.len() - 1;
                    let insert_at = match zone {
                        DropZone::Left | DropZone::Top => at,
                        DropZone::Right => at + 1,
                        DropZone::Bottom => {
                            let r = at / cols;
                            let c = at % cols;
                            ((r + 1) * cols + c).min(n_after_remove)
                        }
                        DropZone::Center => unreachable!(),
                    };
                    if insert_at == drag.idx {
                        return DropOutcome::Nothing;
                    }
                }
                // At both grid caps a split cannot reshape — the drop
                // is an append, and previewing a half-pane split there
                // would promise a shape the release can't deliver.
                let t_win = &win!(self, t.wi);
                let full = t_win.panes.len() + 1 > t_win.grid_cols * t_win.grid_rows;
                let can_reshape = match zone {
                    DropZone::Left | DropZone::Right => t_win.grid_cols < 6,
                    DropZone::Top | DropZone::Bottom => t_win.grid_rows < 6,
                    DropZone::Center => false,
                };
                if !same_window && full && !can_reshape {
                    return DropOutcome::Append { to_wi: t.wi };
                }
                DropOutcome::Split { to_wi: t.wi, at_idx: t.pane_idx, zone }
            }
        }
    }

    /// RFC-006 — recompute the live drop target from this drag tick's
    /// hover.  The ghost is rendered FROM the resolved outcome, so
    /// what lights up is exactly what release will do — a `Nothing`
    /// (own slot, no-op reinsert) lights nothing.
    fn update_drop_target(&mut self, hover_window_id: u32, hx: f64, hy: f64) {
        let new_target = self.window_index(hover_window_id).and_then(|wi| {
            let idx = win!(self, wi).layout.hit_test(hx, hy)?;
            if idx >= win!(self, wi).panes.len() {
                return None;
            }
            let rect = win!(self, wi).layout.cells.get(idx)?;
            let zone = drop_zone_at(rect, hx, hy);
            Some(DropTarget { wi, pane_idx: idx, zone })
        });
        if new_target == self.drop_target {
            return;
        }
        self.clear_drop_preview();
        self.drop_target = new_target;
        let Some(drag) = self.pane_drag else { return };
        let ghost = match self.resolve_drop_outcome(&drag, new_target, hover_window_id) {
            DropOutcome::Split { to_wi, at_idx, zone } => {
                let rect = win!(self, to_wi).layout.cells[at_idx];
                Some((to_wi, drop_preview_rect(&rect, zone), false))
            }
            DropOutcome::Swap { to_wi, idx } | DropOutcome::Fill { to_wi, idx } => {
                let rect = win!(self, to_wi).layout.cells[idx];
                Some((to_wi, drop_preview_rect(&rect, DropZone::Center), false))
            }
            // Append: no particular slot to promise — frame the whole
            // content area ("into this window"), outline only, so the
            // downgrade is visible BEFORE release (RFC-006 §2).
            DropOutcome::Append { to_wi } => {
                let l = &win!(self, to_wi).layout;
                let x = l.sidebar_w;
                let y = l.top_inset;
                let rect = (x, y, (l.window_w - x).max(0.0), (l.window_h - y).max(0.0));
                Some((to_wi, rect, true))
            }
            // NewWindow / Nothing: no rectangle to promise.
            _ => None,
        };
        if let Some((wi, rect, outline)) = ghost {
            win!(self, wi).drop_preview = Some((rect, outline));
            win!(self, wi).needs_render = true;
        }
    }

    fn clear_drop_preview(&mut self) {
        for w in self.windows.iter_mut() {
            if w.drop_preview.take().is_some() {
                w.needs_render = true;
            }
        }
    }

    /// RFC-006 §2 — split placement: insert the dragged pane beside
    /// the hovered pane, reshaping the grid when it is full.  Within
    /// one window this is a REARRANGE (remove + insert, no
    /// placeholder); across windows the source keeps its layout via
    /// the dormant placeholder, same as every move-out.
    fn split_insert(
        &mut self,
        from_wi: usize,
        from_idx: usize,
        to_wi: usize,
        at_idx: usize,
        zone: DropZone,
    ) {
        if from_wi >= self.windows.len()
            || to_wi >= self.windows.len()
            || from_idx >= win!(self, from_wi).panes.len()
            || at_idx >= win!(self, to_wi).panes.len()
        {
            return;
        }
        let same_window = from_wi == to_wi;
        if same_window && from_idx == at_idx {
            return; // splitting beside yourself is where you already are
        }
        // Take the pane out.
        let (pane, at_idx) = if same_window {
            let p = win!(self, from_wi).panes.remove(from_idx);
            let at = if from_idx < at_idx { at_idx - 1 } else { at_idx };
            (p, at)
        } else {
            let (pc, pr) = {
                let g = win!(self, from_wi).panes[from_idx].session().grid();
                (g.cols(), g.rows())
            };
            let p = std::mem::replace(
                &mut win!(self, from_wi).panes[from_idx],
                Pane::new_dormant(pc, pr),
            );
            let w = &mut win!(self, from_wi);
            if w.focused_idx == from_idx {
                w.focused_idx = (0..w.panes.len())
                    .filter(|&i| !w.panes[i].is_dormant())
                    .min_by_key(|&i| i.abs_diff(from_idx))
                    .unwrap_or(from_idx);
            }
            if let Some(sel) = w.selection {
                if sel.session_idx == from_idx {
                    w.selection = None;
                    w.selection_dragging = false;
                }
            }
            (p, at_idx)
        };
        // Reshape the target grid if it cannot absorb one more pane.
        // Horizontal zones grow a column, vertical ones a row; both at
        // the 6-cap → the drop downgrades to append (previewed as the
        // whole-pane rect, and the sidebar overflow catches it).
        let t = &mut win!(self, to_wi);
        let full = t.panes.len() + 1 > t.grid_cols * t.grid_rows;
        if full {
            match zone {
                DropZone::Left | DropZone::Right if t.grid_cols < 6 => t.grid_cols += 1,
                DropZone::Top | DropZone::Bottom if t.grid_rows < 6 => t.grid_rows += 1,
                _ => {}
            }
        }
        // Landing slot, row-major (RFC-006: exact for the flagship
        // 1×1 cases, predictable in general — the preview showed it).
        let cols = t.grid_cols;
        let insert_at = match zone {
            DropZone::Left | DropZone::Top | DropZone::Center => at_idx,
            DropZone::Right => at_idx + 1,
            DropZone::Bottom => {
                let r = at_idx / cols;
                let c = at_idx % cols;
                ((r + 1) * cols + c).min(t.panes.len())
            }
        };
        let insert_at = insert_at.min(t.panes.len());
        t.panes.insert(insert_at, pane);
        t.focused_idx = insert_at;
        self.key_window = to_wi;
        if !same_window {
            self.rebuild_layout(from_wi);
        }
        self.rebuild_layout(to_wi);
        lx_event!(
            "PANE_SPLIT_IN",
            "pane dropped into a split slot",
            to_window = win!(self, to_wi).window_id,
            slot = insert_at,
            zone = format!("{zone:?}")
        );
        self.save_session_state();
        if !same_window {
            self.request_close_if_empty(from_wi);
        }
    }

    /// RFC-006 §1 — center-zone drop: the dragged pane and the
    /// hovered pane trade slots.  Nothing reshapes, nothing spawns,
    /// no placeholder — a swap is symmetric.
    fn swap_panes(&mut self, wa: usize, ia: usize, wb: usize, ib: usize) {
        if wa >= self.windows.len()
            || wb >= self.windows.len()
            || ia >= win!(self, wa).panes.len()
            || ib >= win!(self, wb).panes.len()
        {
            return;
        }
        if wa == wb {
            if ia == ib {
                return;
            }
            win!(self, wa).panes.swap(ia, ib);
            win!(self, wa).focused_idx = ib;
        } else {
            // Two disjoint &mut windows via split_at_mut.
            let (lo, hi, li, hj) = if wa < wb {
                (wa, wb, ia, ib)
            } else {
                (wb, wa, ib, ia)
            };
            let (left, right) = self.windows.split_at_mut(hi);
            std::mem::swap(&mut left[lo].panes[li], &mut right[0].panes[hj]);
            win!(self, wb).focused_idx = ib;
        }
        // Selections referenced content that just teleported.
        win!(self, wa).selection = None;
        win!(self, wa).selection_dragging = false;
        win!(self, wb).selection = None;
        win!(self, wb).selection_dragging = false;
        self.key_window = wb;
        self.rebuild_layout(wa);
        if wa != wb {
            self.rebuild_layout(wb);
        }
        lx_event!(
            "PANE_SWAPPED",
            "panes traded slots",
            a_window = win!(self, wa).window_id,
            b_window = win!(self, wb).window_id
        );
        self.save_session_state();
    }

    /// RFC-006 — drop onto a dormant placeholder: the pane takes the
    /// empty slot (any zone of a placeholder means "put it HERE").
    /// Source side follows the move-out rules; the placeholder is
    /// consumed, so no reshape and no new slot.
    fn fill_placeholder(&mut self, from_wi: usize, from_idx: usize, to_wi: usize, at_idx: usize) {
        if from_wi >= self.windows.len()
            || to_wi >= self.windows.len()
            || from_idx >= win!(self, from_wi).panes.len()
            || at_idx >= win!(self, to_wi).panes.len()
            || !win!(self, to_wi).panes[at_idx].is_dormant()
        {
            return;
        }
        let same_window = from_wi == to_wi;
        // Either way the vacated slot goes dormant — a fill is a
        // slot-to-slot move, and the shape of the source layout is
        // preserved exactly like every other move-out.
        let (pc, pr) = {
            let g = win!(self, from_wi).panes[from_idx].session().grid();
            (g.cols(), g.rows())
        };
        let pane = std::mem::replace(
            &mut win!(self, from_wi).panes[from_idx],
            Pane::new_dormant(pc, pr),
        );
        {
            let w = &mut win!(self, from_wi);
            if w.focused_idx == from_idx {
                w.focused_idx = (0..w.panes.len())
                    .filter(|&i| !w.panes[i].is_dormant())
                    .min_by_key(|&i| i.abs_diff(from_idx))
                    .unwrap_or(from_idx);
            }
            if let Some(sel) = w.selection {
                if sel.session_idx == from_idx {
                    w.selection = None;
                    w.selection_dragging = false;
                }
            }
        }
        win!(self, to_wi).panes[at_idx] = pane;
        win!(self, to_wi).focused_idx = at_idx;
        self.key_window = to_wi;
        if !same_window {
            self.rebuild_layout(from_wi);
        }
        self.rebuild_layout(to_wi);
        lx_event!(
            "PANE_FILLED_PLACEHOLDER",
            "pane dropped into a dormant slot",
            to_window = win!(self, to_wi).window_id,
            slot = at_idx
        );
        self.save_session_state();
        if !same_window {
            self.request_close_if_empty(from_wi);
        }
    }

    /// RFC-006 §4 — drag-to-desktop: park the pane and ask L1 for a
    /// window centred on the release point (screen pts).
    fn move_pane_to_new_window_at(&mut self, from_wi: usize, idx: usize, at: (f64, f64)) {
        let Some(sid) = win!(self, from_wi)
            .panes
            .get(idx)
            .and_then(|p| p.shelld_session_id())
        else {
            return;
        };
        self.pending_move_sid = Some(sid);
        self.pending_to_shell.push((
            MsgType::WindowOpenRequest,
            marspot::shell_proto::encode_window_open_request_at(
                marspot::shell_proto::WINDOW_OPEN_USER,
                at.0,
                at.1,
            ),
        ));
        lx_event!(
            "PANE_MOVE_NEW_WINDOW_REQUESTED",
            "drag-out: asked L1 for a window at the release point",
            session = sid
        );
    }

    /// Post-move bookkeeping — RFC-006 liveness rule: a window's
    /// population is its NON-DORMANT panes, and a window whose last
    /// live pane left asks L1 to close it.  Placeholders alone keep
    /// nothing alive (they are layout, and their layout dies with the
    /// window); this is also what makes "drag the sole pane of a 1×1
    /// window away" read as the window following its pane.
    fn request_close_if_empty(&mut self, wi: usize) {
        // Same rule, same path as a window emptied by closing its
        // panes: nothing live left ⇒ the window goes.  (A move-out
        // cannot empty the *last* window — the destination window
        // exists by the time this runs — so the quit branch inside is
        // unreachable from here.)
        self.close_window_if_emptied(wi);
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
                // No floor on the pane count: closing the last pane of
                // a window closes the window, and closing the last
                // pane of the last window quits marspot.  Both are
                // driven from `close_session` → `close_window_if_emptied`.
                if idx < win!(self, wi).panes.len() {
                    self.close_session(wi, idx);
                    if wi < self.windows.len() {
                        self.rebuild_layout(wi);
                    }
                }
            }
            ContextMenuAction::SplitNewPane => {
                if win!(self, wi).panes.len() < marspot::ui::SESSION_COUNT_HARD_CAP {
                    self.spawn_session(wi);
                    self.rebuild_layout(wi);
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
                    let arg = open_arg_for(link.kind, &link.text);
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
                    let new_sid = pane.shelld_session_id();
                    win!(self, wi).panes.push(pane);
                    // One-shot resolve so the title strip lands
                    // populated on the new pane's first paint instead
                    // of showing its ordinal until the next sweep.
                    // shell_child_pid may not be written yet on this
                    // very tick — then this is a no-op and the sweep
                    // picks the pane up within CWD_SWEEP_INTERVAL.
                    if let Some(sid) = new_sid {
                        self.resolve_pane_cwd(sid);
                    }
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
        // fresh resolve.
        self.pane_cwds.remove(&id);
        self.shell_child_pids.remove(&id);
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
    fn adopt_window(
        &mut self,
        window_id: u32,
        w_phys: f64,
        h_phys: f64,
        scale: f64,
        slot: Option<usize>,
    ) {
        // RFC-005 step 5 — a parked "Move to New Window" pane claims
        // the window before the restore queue gets a look: the user
        // just asked for this window, a boot leftover did not.
        if let Some(sid) = self.pending_move_sid.take() {
            if let Some((from_wi, idx)) = self.find_pane_by_sid(sid) {
                // RFC-006 §3 — same dormant-placeholder rule as an
                // in-window move: the source layout holds still.
                let (pc, pr) = {
                    let g = win!(self, from_wi).panes[idx].session().grid();
                    (g.cols(), g.rows())
                };
                let pane = std::mem::replace(
                    &mut win!(self, from_wi).panes[idx],
                    Pane::new_dormant(pc, pr),
                );
                let w = &mut win!(self, from_wi);
                if w.focused_idx == idx {
                    w.focused_idx = (0..w.panes.len())
                        .filter(|&i| !w.panes[i].is_dormant())
                        .min_by_key(|&i| i.abs_diff(idx))
                        .unwrap_or(idx);
                }
                if let Some(sel) = w.selection {
                    if sel.session_idx == idx {
                        w.selection = None;
                        w.selection_dragging = false;
                    }
                }
                let mut nw = WindowState::new(
                    window_id, vec![pane], 0, 1, 1, w_phys, h_phys,
                );
                nw.frame_index = self.slot_for_new_window(slot);
                nw.render.mark_bg_clear_required();
                self.windows.push(nw);
                let wi = self.windows.len() - 1;
                self.key_window = wi;
                self.rebuild_layout(from_wi);
                self.rebuild_layout(wi);
                lx_event!(
                    "PANE_MOVED",
                    "pane moved into its own new window",
                    session = sid,
                    to_window = window_id
                );
                self.save_session_state();
                self.request_close_if_empty(from_wi);
                return;
            }
            // The pane closed while the window was opening — fall
            // through and let the fresh-pane path fill the window.
        }
        // RFC-005 step 6b — a queued record means L1 is reopening a
        // window from the last session, not making a new one.  The
        // window comes up with its saved grid and one "starting…"
        // placeholder per saved slot; the assembly worker replaces
        // them with the real panes.
        // Prefer the record L1 named.  Arrival order is only a valid
        // pairing while every saved window is being reopened, which is
        // a cold boot; a core swap re-announces the windows that are
        // already up, so a window merely parked would otherwise hand
        // its record to the next live window that showed up.
        let queued = slot
            .and_then(|s| self.saved_windows.iter().position(|(q, _)| *q == s))
            .and_then(|i| self.saved_windows.remove(i))
            .or_else(|| {
                // No slot on the wire (an older shell), or a slot we
                // hold no record for — the window is new.
                slot.is_none().then(|| self.saved_windows.pop_front()).flatten()
            });
        if let Some((slot, record)) = queued {
            self.adopt_restored_window(window_id, slot, record, w_phys, h_phys, scale);
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
        );
        w.frame_index = self.slot_for_new_window(slot);
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
        slot: usize,
        record: marspot::state::SavedWindowLayout,
        w_phys: f64,
        h_phys: f64,
        scale: f64,
    ) {
        // Clamp to the *shared* bound, not a copy of it.  A literal
        // `6` here outlived the day the picker went to 9: a restored
        // window came back squeezed into 6 columns, and because the
        // save side was fine, the file said 7 while the screen said 6.
        let grid_cols = (record.grid_cols as usize).clamp(GRID_MIN, GRID_MAX);
        let grid_rows = (record.grid_rows as usize).clamp(GRID_MIN, GRID_MAX);
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
        );
        w.frame_index = slot;
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
        // Placeholder-era selection referred to panes that no longer
        // exist.
        win!(self, wi).selection = None;
        win!(self, wi).selection_dragging = false;
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
        // A window that moved to a display of another density reports
        // it here.  Chrome follows from the next layout; the terminal
        // cell has to come out of a rebuilt font cache, because its
        // size is the font's metrics and those were taken at the old
        // density.
        marspot::ui::set_chrome_scale(scale);
        win!(self, wi).scale = marspot::ui::chrome_scale();
        self.adopt_display_scale();
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

    /// A window closed: park its layout (if it still held live panes)
    /// and drop its `WindowState`.
    ///
    /// Closing a window does NOT retire its sessions.  The window is
    /// being put away, so its L3s keep running, its record keeps being
    /// saved, and the next launch reattaches them — which is what
    /// makes "close the windows, quit, come back" lossless regardless
    /// of the order the windows were closed in.  Discarding a window
    /// for good is what closing its *panes* does.
    ///
    /// A window with nothing but dormant placeholders left (its last
    /// live pane was moved out, or its last pane was closed) has
    /// nothing worth restoring and is not parked.
    ///
    /// The last window is left alone — L1 owns app teardown, and
    /// tearing the state down here first would race it.
    fn close_window(&mut self, window_id: u32) {
        let Some(i) = self.window_index(window_id) else { return };
        // Window indices shift below; any in-flight drag is stale.
        self.pane_drag = None;
        if self.windows.len() <= 1 {
            lx_event!(
                "WINDOW_CLOSE_LAST",
                "last window closed — L1 drives app teardown",
                window_id = window_id
            );
            return;
        }
        let gone = self.windows.remove(i);
        let parked = gone.panes.iter().any(|p| !p.is_dormant());
        if parked {
            let record = self.window_layout_record(&gone);
            self.parked_windows.push((gone.frame_index, record));
        } else {
            // Discarded for good, so its slot goes too — both saved
            // lists are positional, and a hole in one of them would
            // hand every window above it the geometry of its
            // neighbour.  L1 compacts its own list to match.
            self.forget_slot(gone.frame_index);
        }
        // Balance the `increment_use` from this window's last attach;
        // without it the IOSurface pair leaks for the life of the core.
        if let Some(s) = gone.surfaces.as_ref() {
            s.release();
        }
        self.key_window = self.key_window.min(self.windows.len() - 1);
        lx_event!(
            "WINDOW_CLOSED",
            "window closed",
            window_id = window_id,
            remaining = self.windows.len(),
            parked = parked as u32
        );
        self.save_session_state();
    }

    fn close_session(&mut self, wi: usize, idx: usize) {
        // A pane closing invalidates any armed drag's index math.
        self.pane_drag = None;
        if idx >= win!(self, wi).panes.len() {
            return;
        }
        if let Some(id) = win!(self, wi).panes[idx].shelld_session_id() {
            let is_l3 = win!(self, wi).panes[idx].is_l3();
            self.retire_pane_session(id, is_l3);
        }
        let (pc, pr) = {
            let g = win!(self, wi).panes[idx].session().grid();
            (g.cols(), g.rows())
        };
        win!(self, wi).panes.remove(idx);
        if win!(self, wi).panes.is_empty() {
            // Closing the window is L1's call and arrives a few frames
            // from now (`close_window_if_emptied`).  Until then the
            // window is still on screen and still being rendered, and
            // every pane lookup here indexes rather than `get`s — so
            // leave the same dormant placeholder a moved-out pane
            // leaves.  It renders as an empty slot, it is not parked,
            // and it is not restored.
            win!(self, wi).panes.push(Pane::new_dormant(pc, pr));
        }
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
        self.save_session_state();
        self.close_window_if_emptied(wi);
    }

    /// A window whose last pane the user just closed goes away with it
    /// — and if it was the last window, so does marspot.
    ///
    /// Closing panes is the deliberate, destructive gesture (closing
    /// the *window* parks it instead), so nothing here is remembered:
    /// the window is not parked, and emptying the last window wipes
    /// the saved layout so the next launch opens a fresh default
    /// window rather than resurrecting what was just dismantled.
    ///
    /// L1 owns windows and app teardown, so both cases are a request.
    fn close_window_if_emptied(&mut self, wi: usize) {
        if wi >= self.windows.len() {
            return;
        }
        if win!(self, wi).panes.iter().any(|p| !p.is_dormant()) {
            return;
        }
        let window_id = win!(self, wi).window_id;
        if self.windows.len() == 1 {
            // The app is going away.  Drop the layout first: L1 tears
            // the core down moments after the request lands, and a
            // save racing that would restore an empty window.
            self.layout_discarded = true;
            if let Err(e) = marspot::state::clear() {
                lx_warn!("core.state_file.clear_failed", &format!("{e}"));
            }
            lx_event!(
                "WINDOW_EMPTY_LAST",
                "last pane of the last window closed — asking L1 to quit",
                window_id = window_id
            );
        } else {
            lx_event!(
                "WINDOW_EMPTY",
                "last pane closed — asking L1 to close the window",
                window_id = window_id
            );
        }
        self.pending_to_shell.push((
            MsgType::WindowCloseRequest,
            marspot::shell_proto::encode_window_close_request(window_id),
        ));
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

        // RFC-006 §7 — Esc cancels an in-flight pane drag outright:
        // state cleared, ghost cleared, the release that follows is a
        // plain mouse-up.  Swallowed; a drag is modal.
        if event.state == KeyState::Pressed
            && self.pane_drag.is_some()
            && matches!(event.logical, LogicalKey::Named(NamedKey::Escape))
        {
            if let Some(d) = self.pane_drag.take() {
                let from = d.from_wi;
                if from < self.windows.len() {
                    win!(self, from).needs_render = true;
                }
            }
            self.drop_target = None;
            self.clear_drop_preview();
            return;
        }

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

        // Esc / Cmd-W closes the settings panel, same semantics.
        if win!(self, wi).settings_modal_open && event.state == KeyState::Pressed {
            let is_esc = matches!(event.logical, LogicalKey::Named(NamedKey::Escape));
            let is_cmd_w = matches!(event.logical, LogicalKey::Char('w'))
                && modifiers.super_;
            if is_esc || is_cmd_w {
                win!(self, wi).settings_modal_open = false;
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
            && !pane.is_dormant()
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
            // No cwd refresh here on purpose: an Enter fires *before*
            // the shell has run the line it submits, so reading the cwd
            // on Enter can only ever report the directory the pane was
            // in before the `cd`.  `sweep_pane_cwds` reads it after the
            // fact instead.
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
        // Same answer the render pass uses — see the `agent_tui`
        // comment there.  A hit-test that disagreed with what was
        // drawn would underline one span and click another.
        let cc_mode = pane.shelld_session_id().is_some_and(|sid| {
            self.pane_wheel_keys.contains_key(&sid)
                || self.pane_badges.get(&sid).is_some_and(|b| !b.is_empty())
        });
        let opts = marspot::grid_links::ScanOpts { cc_mode };
        // The same non-blocking oracle the render pass uses, for two
        // reasons.  The obvious one: this runs on the main loop's
        // `events` phase, and the blocking default turns one click
        // into a full-screen stat sweep — caught in the field at
        // 6.39 s with every pane frozen, `mouse_down →
        // hit_test_link_at_xy → FsOracle::probe → lstat` accounting
        // for 948 of 1556 samples.
        //
        // The less obvious one is correctness: a link is clickable
        // because it is *painted*, and it is painted because the
        // render pass got `Exists` from this cache.  Asking a
        // different oracle here lets the hit-test disagree with what
        // is on screen in both directions — an underline that does
        // nothing, or a click that fires on a row showing no link.
        let links = marspot::grid_links::scan_visible_links_with(
            grid,
            view_offset,
            opts,
            marspot::link_probe::oracle(),
        );
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
    /// Why a right-click inside a badge-bearing title strip missed
    /// the badge — or `None` when it did not land in one at all.
    ///
    /// The badge's box is computed in two places from the same
    /// inputs: here, and in the renderer's `push_session`.  Nothing
    /// checks that the two agree, and when they drift the only
    /// symptom is a badge you can see and cannot click.  This turns
    /// that into a line naming the click, the box, and the text.
    fn badge_miss_report(&self, wi: usize, x_phys: f64, y_phys: f64) -> Option<String> {
        let (cell_w, _) = self.renderer.cell_dims();
        let cell_w = cell_w as f64;
        let padding = win!(self, wi).layout.padding;
        let title_h = win!(self, wi).layout.cell_title_h;
        let cell_count = win!(self, wi).layout.cells.len();
        for (i, p) in win!(self, wi).panes.iter().enumerate().take(cell_count) {
            // `continue`, not `?`.  A pane with no session id yet — one
            // still starting, one whose L3 just died — is a pane to skip,
            // not a reason to abandon the search: with `?` a single such
            // pane ANYWHERE ahead of the clicked one silently suppressed
            // this whole report, which is why `core.badge_hit_miss` never
            // appeared once in 13k log lines while the badge menu was
            // demonstrably failing (2026-09-05).
            let sid = match p.shelld_session_id() {
                Some(s) => s,
                None => continue,
            };
            let rect = &win!(self, wi).layout.cells[i];
            let y_lo = rect.y_top;
            if y_phys < y_lo || y_phys >= y_lo + title_h {
                continue;
            }
            if x_phys < rect.x || x_phys >= rect.x + rect.w {
                continue;
            }
            let badge = self.pane_badges.get(&sid).cloned().unwrap_or_default();
            if badge.is_empty() {
                // No badge on this pane: a plain title-strip click,
                // which is not a miss.
                return None;
            }
            let reserved = if p.update_pending() && i == win!(self, wi).focused_idx {
                cell_w * 1.5
            } else {
                0.0
            };
            let chars = badge.chars().count() as f64;
            let lo = rect.x + rect.w - padding - reserved - chars * cell_w;
            let prefix = badge.split(' ').next().unwrap_or("").chars().count() as f64;
            return Some(format!(
                "sid={sid} click=({x_phys:.1},{y_phys:.1}) \
                 badge={badge:?} chars={chars} prefix_chars={prefix} \
                 box=[{lo:.1},{:.1}) cell_w={cell_w:.2} reserved={reserved:.1} \
                 pane=[{:.1},{:.1}) focused_idx={} update_pending={}",
                lo + prefix * cell_w,
                rect.x,
                rect.x + rect.w,
                win!(self, wi).focused_idx,
                p.update_pending(),
            ));
        }
        None
    }

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

    fn toggle_settings_modal(&mut self, wi: usize) {
        win!(self, wi).settings_modal_open = !win!(self, wi).settings_modal_open;
        if win!(self, wi).settings_modal_open {
            // Pick up anything edited by hand since the last look, so
            // the panel never shows a value the file disagrees with.
            marspot::settings::reload_if_changed();
        }
        win!(self, wi).needs_render = true;
    }

    /// The settings panel's rect.
    ///
    /// Needs the font: the panel sizes itself to measured text, and
    /// the hit-test measures through this same path — a segment
    /// clickable somewhere other than where it is drawn is exactly
    /// what a second, approximate measurement would produce.
    /// Re-derive everything that was sized for the old display.
    ///
    /// A MacBook Pro and a 4K panel run without HiDPI are two
    /// densities, and a window gets dragged between them — so this is
    /// a normal event, not a corner case.  The font cache holds the
    /// terminal cell, so it is rebuilt first; then every window
    /// relayouts, which resizes its panes, which is what tells each
    /// PTY its new size.  No-op when the scale did not actually move,
    /// which is the common case (this runs on every attach).
    fn adopt_display_scale(&mut self) {
        if !self.renderer.rebuild_fonts_if_scale_changed() {
            return;
        }
        let (cell_w, cell_h) = self.renderer.cell_dims();
        lx_event!(
            "DISPLAY_SCALE_ADOPTED",
            "fonts rebuilt for a new display density; grids reflow",
            chrome_scale = format!("{:.2}", marspot::ui::chrome_scale()),
            cell_w = format!("{cell_w:.2}"),
            cell_h = format!("{cell_h:.2}"),
            windows = self.windows.len() as u64
        );
        for wi in 0..self.windows.len() {
            self.rebuild_layout(wi);
            win!(self, wi).needs_render = true;
        }
    }

    /// Adopt a new window-button cluster edge and relay it out.
    ///
    /// Shared by the attach path and the chrome-only frame so the two
    /// cannot disagree about what a measurement means.
    fn apply_window_chrome(&mut self, window_id: u32, right_phys: f64) {
        let Some(wi) = self.window_index(window_id) else { return };
        let changed = win!(self, wi)
            .lights_right_phys
            .map(|old| (old - right_phys).abs() > 0.5)
            .unwrap_or(true);
        lx_event!(
            "TRAFFIC_LIGHTS",
            "window-button cluster edge as measured by the shell",
            window_id = window_id as u64,
            right_phys = format!("{right_phys:.1}"),
            was = win!(self, wi)
                .lights_right_phys
                .map(|v| format!("{v:.1}"))
                .unwrap_or_else(|| "unmeasured".into()),
            changed = changed as u32
        );
        if changed {
            win!(self, wi).lights_right_phys = Some(right_phys);
            self.rebuild_layout(wi);
            win!(self, wi).needs_render = true;
        }
    }

    fn settings_modal_rect(&mut self, wi: usize) -> marspot_term::layout::Rect {
        let (w_phys, h_phys, top_inset) = (
            win!(self, wi).w_phys,
            win!(self, wi).h_phys,
            win!(self, wi).layout.top_inset,
        );
        let settings = marspot::settings::get();
        let font = self.renderer.font_mut();
        let mut measure = |s: &str, pt: f64, weight: u16| {
            font.measure_ui_text_at_size(
                s, weight, marspot::font_shape::ShapeOptions::default(), pt,
            )
        };
        marspot::ui::components::settings_modal::panel_rect(
            w_phys, h_phys, &settings, &mut measure, top_inset,
        )
    }

    fn cc_usage_modal_rect(&self, wi: usize) -> marspot_term::layout::Rect {
        let n = win!(self, wi)
            .cc_usage_modal
            .as_ref()
            .and_then(|m| m.data.as_ref())
            .map(|d| d.accounts.len())
            .unwrap_or(1);
        let extra_bar_rows = win!(self, wi)
            .cc_usage_modal
            .as_ref()
            .and_then(|m| m.data.as_ref())
            .and_then(|d| d.accounts.iter().map(|a| a.model_limits.len()).max())
            .unwrap_or(0);
        let (cell_w, cell_h) = self.renderer.cell_dims();
        // Geometry lives with the rest of the modal's metrics so the
        // painter and this rect can't disagree about how tall a card is.
        marspot::ui::components::cc_usage_modal::panel_rect(
            n,
            extra_bar_rows,
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
                        model_rows: a
                            .model_limits
                            .iter()
                            .map(|m| (m.label.to_uppercase(), m.util as f32))
                            .collect(),
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
            // Resolve the pane's name the same way the title strip
            // does: its directory, numbered when shared.
            let name = {
                if let Some(p) = self.pane_cwds.get(&sid) {
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
        } else if win!(self, wi).layout.hit_test_settings_button(x_phys, y_phys) {
            Some(ChromeBtn::Settings)
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
                    } else if tag == MOVE_TO_NEW_WINDOW_TAG
                        || (MOVE_TO_WINDOW_TAG_BASE..MOVE_TO_NEW_WINDOW_TAG).contains(&tag)
                    {
                        let idx = match region {
                            ContextRegion::Pane(i)
                            | ContextRegion::SidebarSlot(i) => i,
                            _ => win!(self, wi).focused_idx,
                        };
                        win!(self, wi).context_menu = None;
                        win!(self, wi).needs_render = true;
                        if tag == MOVE_TO_NEW_WINDOW_TAG {
                            self.move_pane_to_new_window(wi, idx);
                        } else {
                            let to = (tag - MOVE_TO_WINDOW_TAG_BASE) as usize;
                            self.move_pane_to_window(wi, idx, to);
                        }
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
        // Toolbar button #6 — the settings panel.
        if win!(self, wi).layout.hit_test_settings_button(x_phys, y_phys) {
            self.toggle_settings_modal(wi);
            return;
        }
        // Settings panel: hit-test its controls, swallow anything else
        // inside the frame, close on a click outside.
        if win!(self, wi).settings_modal_open {
            let rect = self.settings_modal_rect(wi);
            if !rect.contains(x_phys, y_phys) {
                win!(self, wi).settings_modal_open = false;
                win!(self, wi).needs_render = true;
                return;
            }
            let cur_settings = marspot::settings::get();
            let hit = {
                let font = self.renderer.font_mut();
                let mut measure = |s: &str, pt: f64, weight: u16| {
                    font.measure_ui_text_at_size(
                        s, weight, marspot::font_shape::ShapeOptions::default(), pt,
                    )
                };
                marspot::ui::components::settings_modal::hit_test(
                    rect, &cur_settings, &mut measure, x_phys, y_phys,
                )
            };
            if let Some((row, seg)) = hit {
                let cur = marspot::settings::get();
                if let Some(next) = row.apply(&cur, seg) {
                    // Write, then adopt.  If the disk write fails the
                    // panel must keep showing what the file says, not
                    // what the click asked for — a control that lies
                    // about having saved is worse than one that does
                    // not move.
                    match marspot::settings::write(&next) {
                        Ok(()) => {
                            marspot::settings::reload_if_changed();
                            lx_event!(
                                "SETTINGS_CHANGED",
                                "settings panel wrote a new value",
                                row = format!("{:?}", row),
                                reclaim = next.reclaim_enabled as u32,
                                idle_min = next.reclaim_idle_minutes as u64,
                                prefetch = next.reclaim_prefetch as u32
                            );
                        }
                        Err(e) => lx_warn!(
                            "core.settings.write_failed",
                            &format!("{e}; the panel keeps showing the file's values")
                        ),
                    }
                }
            }
            win!(self, wi).needs_render = true;
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
                HEADER_PT * win!(self, wi).scale,
                win!(self, wi).pending_grid_cols, win!(self, wi).pending_grid_rows,
                win!(self, wi).layout_modal_label_w,
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
                // Resolve every pane on the open transition so the
                // modal preview shows values fetched now, not up to
                // CWD_SWEEP_INTERVAL old.
                let sids: Vec<u64> = win!(self, wi)
                    .panes
                    .iter()
                    .filter_map(|p| p.shelld_session_id())
                    .collect();
                for sid in sids {
                    self.resolve_pane_cwd(sid);
                }
            }
            win!(self, wi).layout_modal_open = !win!(self, wi).layout_modal_open;
            win!(self, wi).layout_drag = None;
            win!(self, wi).needs_render = true;
            return;
        }

        // Sidebar close-[×]: refuse to close the last session.
        if let Some(idx) = close_session_hit {
            // The last pane closes too — see `close_window_if_emptied`.
            if idx < win!(self, wi).panes.len() {
                self.close_session(wi, idx);
                if wi < self.windows.len() {
                    self.rebuild_layout(wi);
                }
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

        // Title-strip press: could be a click (enter title edit) or
        // the start of a pane drag (RFC-005 step 5).  Arm the drag and
        // Focus shifts immediately (clicking focuses); a drag from the
        // title strip moves the pane.
        if let Some(idx) = title_hit {
            // RFC-006 — a dormant placeholder has no live content to
            // move; its title press is inert (the CELL click below is
            // what revives it).
            if win!(self, wi).panes.get(idx).is_some_and(|p| p.is_dormant()) {
                return;
            }
            if idx < win!(self, wi).panes.len() {
                self.resolve_pending_on_defocus(wi, idx);
                win!(self, wi).focused_idx = idx;
                let _ = win!(self, wi).focused_pane_mut().snap_to_live();
                win!(self, wi).selection = None;
                win!(self, wi).selection_dragging = false;
                win!(self, wi).needs_render = true;
                self.pane_drag = Some(PaneDrag {
                    from_wi: wi,
                    idx,
                    start: (x_phys, y_phys),
                    active: false,
                });
                return;
            }
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

        // RFC-006 — clicking a dormant placeholder is ITS revive
        // gesture: spawn a fresh shell into that slot, explicitly and
        // only on this click.  Checked before selection so the click
        // doesn't also start selecting the hint text.
        if let Some((idx, _, _)) = cell_pos_hit {
            if win!(self, wi).panes.get(idx).is_some_and(|p| p.is_dormant()) {
                if let Ok(sid) = allocate_next_session_id() {
                    let (cols, rows) = win!(self, wi)
                        .layout
                        .cells
                        .get(idx)
                        .map(|c| (c.cols, c.rows))
                        .unwrap_or((INITIAL_COLS, INITIAL_ROWS));
                    win!(self, wi).panes[idx] =
                        spawn_l3_pane_async(cols, rows, sid, &self.event_tx);
                    win!(self, wi).focused_idx = idx;
                    win!(self, wi).needs_render = true;
                    self.save_session_state();
                    lx_event!(
                        "DORMANT_REVIVED",
                        "placeholder clicked; spawning a shell into the slot",
                        session_id = sid
                    );
                }
                return;
            }
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

    fn mouse_drag(
        &mut self,
        wi: usize,
        x_phys: f64,
        y_phys: f64,
        hover_window_id: u32,
        hover_x: f64,
        hover_y: f64,
    ) {
        // RFC-005 step 5 — an armed title press becomes a pane drag
        // once the pointer clears the slop radius.  The activation
        // repaints the title (a ⇢ marker) so the mode is visible.
        if let Some(d) = self.pane_drag.as_mut() {
            if !d.active {
                let (sx, sy) = d.start;
                if (x_phys - sx).hypot(y_phys - sy) > PANE_DRAG_SLOP_PHYS {
                    d.active = true;
                    let from = d.from_wi;
                    win!(self, from).needs_render = true;
                }
            }
            if self.pane_drag.map(|d| d.active).unwrap_or(false) {
                self.update_drop_target(hover_window_id, hover_x, hover_y);
                return;
            }
        }
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

    fn mouse_up(&mut self, wi: usize, drop_window_id: u32, drop_x: f64, drop_y: f64) {
        // RFC-005 step 5 / RFC-006 — resolve an armed title press.
        if let Some(d) = self.pane_drag.take() {
            let target = self.drop_target.take();
            self.clear_drop_preview();
            if d.active {
                let from = d.from_wi;
                win!(self, from).needs_render = true;
                // The SAME resolution the ghost was drawn from — the
                // release delivers exactly what was previewed.
                match self.resolve_drop_outcome(&d, target, drop_window_id) {
                    DropOutcome::Split { to_wi, at_idx, zone } => {
                        self.split_insert(d.from_wi, d.idx, to_wi, at_idx, zone);
                    }
                    DropOutcome::Swap { to_wi, idx } => {
                        self.swap_panes(d.from_wi, d.idx, to_wi, idx);
                    }
                    DropOutcome::Fill { to_wi, idx } => {
                        self.fill_placeholder(d.from_wi, d.idx, to_wi, idx);
                    }
                    DropOutcome::Append { to_wi } => {
                        self.move_pane_to_window(d.from_wi, d.idx, to_wi);
                    }
                    DropOutcome::NewWindow => {
                        self.move_pane_to_new_window_at(
                            d.from_wi,
                            d.idx,
                            (drop_x, drop_y),
                        );
                    }
                    DropOutcome::Nothing => {
                        lx_debug!(
                            "core.pane_drag.cancelled",
                            "release resolves to nothing — as previewed",
                            drop_window_id = drop_window_id
                        );
                    }
                }
                return;
            }
            // Never activated: this was a click, not a drag.  It has
            // already focused the pane on mouse-down, and there is
            // nothing else for a title click to do — a pane's name is
            // its directory, not something to type.
            return;
        }
        // F3+3.3 — finalize LayoutModal card drag: pick the
        // destination slot under the cursor, swap, redraw.  No
        // animation (V2.0); settle = single-frame jump.
        if let Some(d) = win!(self, wi).layout_drag.take() {
            use marspot::ui::components::LayoutModal;
            let modal = LayoutModal::layout(
                win!(self, wi).w_phys, win!(self, wi).h_phys, win!(self, wi).scale,
                HEADER_PT * win!(self, wi).scale,
                win!(self, wi).pending_grid_cols, win!(self, wi).pending_grid_rows,
                win!(self, wi).layout_modal_label_w,
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
        // A plugin-held PaneSession that took the keyboard owns the
        // wheel too.  Keys route to L1 in `key` above; the wheel used
        // to fall straight through to the PTY, and on a mouse-tracking
        // pane `apply_scroll_lines` encodes it as `CSI < 64;x;y M` —
        // so scrolling during a profile cycle typed mouse reports into
        // the shell prompt the cycle had just uncovered, and their echo
        // kept the PTY noisy enough that `await_quiet` could only ever
        // time out (2026-09-01, the `^[[<64;37;32M` screenful).
        //
        // Dropped rather than routed up: a held pane's picture is
        // frozen, so there is no scroll for the user to see either.
        if let Some(active_sid) = self.focused_pane_active_session(wi) {
            if self
                .pane_session_for(active_sid)
                .is_some_and(|s| s.has(marspot::shell_proto::PANE_SESSION_CAP_LOCK_KEYS))
            {
                return;
            }
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
        // RFC-008 — a plugin may own this pane's wheel.  Checked before
        // the pane's own routing: the program repaints in place, so
        // there is no scrollback for the terminal to move and no mouse
        // reporting to forward through; only these keys reach it.
        //
        // `enter` goes only when the program's own on-screen marker
        // says the view is NOT open.  Read, never remembered: the
        // program leaves that view on its own as well as by the user's
        // key, and `enter` is typically a toggle — a stale flag would
        // then close the transcript instead of opening it (which is
        // exactly what happened when this was a remembered bool).
        //
        // Nothing here sends a key to LEAVE the view: a stray tick at
        // the bottom would otherwise close what the user was reading.
        //
        // And only an UPWARD tick may OPEN it.  Reaching for history is
        // an upward gesture; a downward one at rest means "I am already
        // at the newest, show me what is below" — answering that by
        // opening a history view is a surprise.  A closed view plus a
        // downward tick is therefore not ours: it falls through to the
        // pane's own routing untouched.
        if let Some(sid) = win!(self, wi).panes[idx].session().shelld_session_id() {
            if self.pane_wheel_keys.contains_key(&sid) && lines != 0 {
                let mut buf: Vec<u8> = Vec::new();
                let ticks = lines.unsigned_abs().min(
                    win!(self, wi).panes[idx].session().grid().rows().max(1) as u32,
                );
                // Read the state, do not remember it — see `WheelKeys`.
                let open = {
                    let k = &self.pane_wheel_keys[&sid];
                    // The program's own answer, when it gave one.
                    let alt_scroll =
                        win!(self, wi).panes[idx].session().l3_alt_scroll_active();
                    let grid = win!(self, wi).panes[idx].session().grid();
                    marspot::wheel_marker::view_is_open(
                        alt_scroll,
                        grid.cols(),
                        grid.rows(),
                        |col, row| grid.cell(col, row).ch,
                        &k.marker,
                    )
                };
                let up = lines > 0;
                let since_enter = self
                    .pane_wheel_enter_at
                    .get(&sid)
                    .map(|t| t.elapsed().as_millis() as u64);
                let ask = marspot::wheel_marker::should_send_enter(open, since_enter);
                let mut suppressed = false;
                if marspot::wheel_marker::wheel_is_ours(open, up) {
                    if let Some(k) = self.pane_wheel_keys.get(&sid) {
                        let mut opening = false;
                        if ask && !k.enter.is_empty() {
                            buf.extend_from_slice(&k.enter);
                            opening = true;
                        } else if !open && !k.enter.is_empty() {
                            suppressed = true;
                        }
                        // The tick that OPENS the view does not also
                        // travel in it.  A flick is one wheel event
                        // carrying many lines, so sending the enter key
                        // and that whole distance together opened the
                        // history and immediately threw the user tens
                        // of lines into it — landing in the middle of
                        // whatever the program had printed there rather
                        // than at the edge they were reaching for.
                        // Reaching for history is one gesture; moving
                        // inside it is the next one.
                        if !opening {
                            let key = if up { &k.up } else { &k.down };
                            for _ in 0..ticks {
                                buf.extend_from_slice(key);
                            }
                        }
                    }
                }
                if ask && !buf.is_empty() {
                    self.pane_wheel_enter_at.insert(sid, std::time::Instant::now());
                }
                if suppressed {
                    // The thing the state log could not see.  A run of
                    // these is the flick that used to toggle the view
                    // open and shut several times over.
                    lx_debug_sampled!(
                        "core.pane_wheel.enter_held",
                        8,
                        "tick arrived while the last enter was still in flight",
                        session = sid,
                        since_ms = since_enter.unwrap_or(0)
                    );
                }
                // One line per change of state, not per event: a
                // momentum scroll is many events, and the thing worth
                // seeing is whether the view opened and stayed open.
                // Silence here is what made three wrong fixes all look
                // right (2026-09-06).
                if !buf.is_empty() {
                    if self.pane_wheel_open.get(&sid) != Some(&open) {
                        lx_info!(
                            "core.pane_wheel.state",
                            &format!("sid={sid} open={open} up={up} sent_enter={}", !open)
                        );
                        self.pane_wheel_open.insert(sid, open);
                    }
                    win!(self, wi).panes[idx].session_mut().forward_inject_input(&buf);
                    win!(self, wi).needs_render = true;
                    return;
                }
            }
        }
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
    /// candidate window, or `None` when the slot takes no input.
    /// Where the renderer's last IOSurface frame spent its time —
    /// forwarded so the loop's stall report can carry it.
    fn renderer_split(&self) -> marspot::render_metal::RenderSplit {
        self.renderer.last_render_split()
    }

    /// Commit a frame and return; the caller presents it once
    /// `WindowRender::settled()` says the GPU is done.
    fn render(
        &mut self,
        wi: usize,
        target_tex: &objc2::rc::Retained<ProtocolObject<dyn MTLTexture>>,
    ) -> Option<(f64, f64, f64, f64)> {
        self.render_inner(wi, target_tex, false)
    }

    /// Render and wait, for the one caller that announces the surface
    /// in the same breath.
    ///
    /// Attach installs a fresh pair and acks `SurfaceReady` right
    /// away, so the frame must genuinely be finished — an unfinished
    /// surface announced here is a black window until the next frame.
    /// It happens once per attach and nobody is typing into it, so the
    /// wait costs nothing worth reclaiming.
    fn render_blocking(
        &mut self,
        wi: usize,
        target_tex: &objc2::rc::Retained<ProtocolObject<dyn MTLTexture>>,
    ) -> Option<(f64, f64, f64, f64)> {
        self.render_inner(wi, target_tex, true)
    }

    fn render_inner(
        &mut self,
        wi: usize,
        target_tex: &objc2::rc::Retained<ProtocolObject<dyn MTLTexture>>,
        block: bool,
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

        // Title placeholder = basename of the cwd cached in
        // `pane_cwds`.  Pure read: `sweep_pane_cwds` owns population
        // (once per CWD_SWEEP_INTERVAL from the main loop's periodic
        // block, plus a one-shot resolve at pane spawn and on
        // LayoutModal open).  Nothing here may issue a syscall — this
        // runs per frame, per window.
        let cwd_basenames: Vec<Option<&str>> = (0..win!(self, wi).panes.len())
            .map(|i| {
                let sid = win!(self, wi).panes[i].shelld_session_id()?;
                let path = self.pane_cwds.get(&sid)?;
                std::path::Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
            })
            .collect();

        // Resolved label per cell: the pane's own name (its directory,
        // numbered when shared — `marspot::pane_name`) → plugin-set
        // title (MsgType::PaneTitle, cc/...) → ordinal fallback.
        //
        // There is no user-set title any more: a pane's name is derived,
        // never stored, so it cannot disagree with what `--send` will
        // accept.  A rename would have made the name a second source of
        // truth about which pane is which.
        // Every pane in every window, with where it sits: the number
        // in `spg#2` is the pane's position, so a name cannot be
        // computed from one window alone.
        let refs: Vec<marspot::pane_name::PaneRef> = self
            .windows
            .iter()
            .enumerate()
            .flat_map(|(w, win)| {
                let cols = win.grid_cols.max(1);
                win.panes.iter().enumerate().filter_map(move |(i, p)| {
                    let sid = p.shelld_session_id()?;
                    // Panes past the grid overflow into the sidebar and
                    // have no cell of their own.
                    let at = (i < cols * win.grid_rows.max(1))
                        .then(|| (w + 1, i / cols + 1, i % cols + 1));
                    Some((sid, at))
                })
            })
            .map(|(sid, at)| {
                marspot::pane_name::PaneRef::new(
                    sid,
                    self.pane_cwds.get(&sid).cloned().unwrap_or_default(),
                    at,
                )
            })
            .collect();
        let pane_names: std::collections::HashMap<u64, String> =
            marspot::pane_name::assign(&refs).into_iter().collect();
        let resolved_labels: Vec<String> = (0..win!(self, wi).panes.len())
            .map(|i| {
                if let Some(name) = win!(self, wi).panes[i]
                    .shelld_session_id()
                    .and_then(|sid| pane_names.get(&sid))
                    .filter(|n| !n.is_empty() && *n != "?")
                {
                    name.clone()
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
                // RFC-005 step 5 — a pane being dragged wears a ⇢ so
                // the mode is visible without a ghost overlay: drop it
                // on another window to move it there, release anywhere
                // else to cancel.
                if self
                    .pane_drag
                    .is_some_and(|d| d.active && d.from_wi == wi && d.idx == i)
                {
                    s.insert_str(0, "⇢ ");
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
        // The settings panel.  A snapshot per frame — one consistent
        // set of values, so a toggle and a segment can never be drawn
        // from either side of the same click.
        let settings_data = if win!(self, wi).settings_modal_open {
            Some(marspot::render_metal::SettingsRender {
                rect: self.settings_modal_rect(wi),
                settings: (*marspot::settings::get()).clone(),
                path: marspot::settings::path().display().to_string(),
            })
        } else {
            None
        };
        self.renderer.set_settings_panel(settings_data);
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
        // RFC-006 — this window's drop-preview ghost (usually None),
        // and the index of its pane mid-drag (dimmed by the renderer).
        self.renderer.set_drop_preview(win!(self, wi).drop_preview);
        self.renderer.set_drag_source(
            self.pane_drag
                .filter(|d| d.active && d.from_wi == wi)
                .map(|d| d.idx),
        );

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
            let (cell_w, _) = self.renderer.cell_dims();
            win!(self, wi).layout_modal_label_w = slot_titles
                .iter()
                .map(|t| t.chars().count())
                .max()
                .unwrap_or(0) as f64
                * cell_w;
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
        // Advance every pane's dim towards where its attention level
        // says it belongs, and keep the frames coming while any of them
        // is still moving.  Nothing here is a timer: when the last one
        // arrives this stops asking, and the window is idle again.
        let now = Instant::now();
        let mut fading = false;
        for (i, p) in win!(self, wi).panes.iter_mut().enumerate().take(cell_count) {
            fading |= p.aim_scrim(i == focused, now);
        }
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
                // A plugin only declares wheel keys about a program
                // it is actually driving, and the declaration survives
                // a core swap — so it is the durable answer to "does
                // an agent TUI paint this pane", where a badge that
                // can momentarily read empty is not.
                let agent_tui = p
                    .shelld_session_id()
                    .is_some_and(|sid| self.pane_wheel_keys.contains_key(&sid))
                    || !badge.is_empty();
                let mut v = p.view(
                    i == focused,
                    titles.get(i).map(|s| s.as_str()).unwrap_or(""),
                    badge,
                    agent_tui,
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
        if block {
            self.renderer.render_layout_to_texture(
                &mut wr,
                target_tex,
                &win!(self, wi).layout,
                &views,
                &entries,
                focused,
            );
        } else {
            self.renderer.render_layout_to_texture_async(
                &mut wr,
                target_tex,
                &win!(self, wi).layout,
                &views,
                &entries,
                focused,
            );
        }
        win!(self, wi).render = wr;
        // Cleared here, then set again if a dim is still on its way:
        // the order matters, because this reset runs after the frame
        // and would otherwise wipe the request for the next one.
        win!(self, wi).needs_render = fading;
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
            let (col, row) = pane.session().ime_caret_cell()?;
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
        /// RFC-006 — restore as a dormant placeholder: no reattach,
        /// no resurrect, no spawn.  A placeholder is layout.
        dormant: bool,
    }
    let slot_specs: Vec<SlotSpec> = match saved_state {
        Some(s) => s
            .panes
            .iter()
            .take(n_sessions)
            .map(|p| SlotSpec {
                sid: p.sid,
                cwd: p.last_cwd.clone(),
                dormant: p.flags & marspot::state::PANE_FLAG_DORMANT != 0,
            })
            .collect(),
        None => {
            let mut ids: Vec<u64> = dir_ids.iter().copied().collect();
            ids.sort();
            ids.truncate(n_sessions);
            let mut specs: Vec<SlotSpec> = ids
                .into_iter()
                .map(|sid| SlotSpec { sid, cwd: String::new(), dormant: false })
                .collect();
            while specs.len() < n_sessions {
                specs.push(SlotSpec { sid: 0, cwd: String::new(), dormant: false });
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
        // RFC-006 — a dormant slot restores AS a dormant slot: it is
        // layout, not a session, and none of the session machinery
        // below (reattach / resurrect / spawn) applies.
        if spec.dormant {
            panes.push(Pane::new_dormant(boot_cols, boot_rows));
            continue;
        }
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
        // 2026-07-28 — safe-mode boot (crash-loop brake, set by the
        // shell): reattach is fine, fresh processes are not.  The slot
        // stays vacant; the user revives it with a keystroke once the
        // loop is understood.  This is the line that stops a crash
        // loop from spawning a new generation every cycle.
        if std::env::var("MARSPOT_SAFE_MODE").is_ok() {
            lx_warn!(
                "core.boot.safe_mode_vacant",
                "safe mode: slot left vacant instead of spawning fresh",
                session = spawn_sid
            );
            claimed.insert(spawn_sid);
            panes.push(Pane::new_vacant(spawn_sid, boot_cols, boot_rows));
            continue;
        }
        // …but only once we know nobody is living there.  The two
        // unlinks below are what make a session reachable, and doing
        // them under a running L3 strands it for good: its listener fd
        // stays bound to a path that no longer exists, so no future
        // core can dial it and its own deadman (which only watched
        // entry.toml) saw nothing wrong.  Seven sessions were found in
        // exactly that state on 2026-07-29 — an L3 mid-execv had not
        // yet rewritten entry.toml when this boot scanned the
        // registry, so it never made `alive_ids`, and we cleaned the
        // slot out from under a live process.
        //
        // The A.3 dir lock is the right authority here precisely
        // because it does not depend on the registry being current: it
        // is held for the owner's whole life, survives execv, and
        // releases only on process death.  `alive_ids` answers "was
        // there a valid entry when we scanned"; this answers "is
        // anyone home right now".
        let dir_is_occupied = match marspot_term::session_registry::try_lock_session_dir(spawn_sid)
        {
            // Free — drop it straight away so the L3 we spawn takes
            // ownership itself.
            Ok(lock) => {
                std::mem::drop(lock);
                false
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
            // Couldn't even attempt the lock (IO error).  Don't let a
            // lock problem block session bootstrap — fall through to
            // the old behaviour.
            Err(_) => false,
        };
        if dir_is_occupied {
            lx_warn!(
                "core.boot.slot_occupied",
                "another process still owns this session dir — leaving the slot vacant \
                 instead of unlinking a live L3's socket",
                session = spawn_sid
            );
            claimed.insert(spawn_sid);
            panes.push(Pane::new_vacant(spawn_sid, boot_cols, boot_rows));
            continue;
        }
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
    // A pane's name is derived from its directory (`pane_name`), so
    // there is nothing here to restore: what used to be carried across
    // a swap was the user-set title, and there is no such thing now.
    (panes, reattached_ids)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // `--version` answers before anything else touches the machine.
    // The update path probes a freshly-staged binary with it for one
    // reason: to make the kernel exec this file while the outgoing
    // core is still on screen, so the Gatekeeper assessment is paid
    // then instead of during the swap's blackout.  A probe that
    // migrated state, opened the log, or read env would be a probe
    // with side effects — so this arm comes first and does neither.
    if args.first().map(String::as_str) == Some("--version") {
        println!("{}", env!("MARSPOT_VERSION_CORE"));
        return;
    }

    // RFC-004 D.1 — must precede logx / any path computation.
    marspot::paths::migrate_legacy_state_root();
    marspot::logx::init("core");
    // Start the async path oracle before the first frame.  Without it
    // `link_probe::oracle()` falls back to the blocking `FsOracle`,
    // which is correct for one-shot renderers and catastrophic for a
    // render loop — see the module docs.
    marspot::link_probe::install();

    // Must run before any env is read — see `parse_log_event`.
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

    // Adopt the display's scale **before** the renderer exists: the
    // font cache bakes it into the terminal cell at build time, and a
    // cell built at the wrong scale would have to reflow every grid to
    // correct.  See `marspot::ui::chrome_scale`.
    marspot::ui::set_chrome_scale(scale);
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
    // Each record keeps the slot it was read from — that index is what
    // pairs it with `window-state.bin`'s geometry list, and it has to
    // survive windows being closed and parked.
    let mut saved_windows: std::collections::VecDeque<(usize, marspot::state::SavedWindowLayout)> =
        saved_state
            .map(|s| s.windows.into_iter().enumerate().collect())
            .unwrap_or_default();
    let boot_window = saved_windows.pop_front().map(|(_, r)| r);
    // No saved layout means a first run, or the launch after the user
    // closed the last pane of the last window.  Both want one window
    // with one shell in it — a 3×3 wall of nine shells is a layout the
    // user asks for, not one to be handed on arrival.
    let (grid_cols, grid_rows): (usize, usize) = match boot_window.as_ref() {
        // Same shared bound as the picker and the restore path — see
        // `restore_saved_window`.  This is the copy that ate the
        // user's 7th column on every core swap.
        Some(s) if s.grid_cols > 0 && s.grid_rows > 0 => (
            (s.grid_cols as usize).clamp(GRID_MIN, GRID_MAX),
            (s.grid_rows as usize).clamp(GRID_MIN, GRID_MAX),
        ),
        _ => (1, 1),
    };
    let n_sessions = match boot_window.as_ref() {
        Some(s) => s.panes.len().clamp(1, SESSION_COUNT_HARD_CAP),
        None => 1,
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
            .flat_map(|(_, w)| w.panes.iter())
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
        pane_wheel_keys: std::collections::HashMap::new(),
        pane_wheel_enter_at: std::collections::HashMap::new(),
        pane_wheel_open: std::collections::HashMap::new(),
        pane_titles: std::collections::HashMap::new(),
        pane_cwds: std::collections::HashMap::new(),
        pending_to_shell: Vec::new(),
        last_focus_notified: std::collections::HashMap::new(),
        pane_sessions: std::collections::HashMap::new(),
        esc_history: std::collections::HashMap::new(),
        shell_child_pids: std::collections::HashMap::new(),
        // Both clocks start "already due" so the first main-loop
        // iteration resolves every pane's cwd (boot titles land on the
        // first paint) and the first change it finds is persisted.
        last_cwd_sweep: Instant::now() - CWD_SWEEP_INTERVAL,
        last_cwd_save: Instant::now() - CWD_SAVE_MIN_GAP,
        cwd_save_pending: false,
        reconnecting: std::collections::HashSet::new(),
        all_exited: false,
        saw_window_aware_attach: false,
        l3_mode,
        event_tx: event_tx.clone(),
        drag_window: None,
        pane_drag: None,
        drop_target: None,
        pending_move_sid: None,
        saved_windows,
        parked_windows: Vec::new(),
        dev_close_panes: std::env::var("MARSPOT_DEV_CLOSE_PANES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        dev_close_panes_at: Some(Instant::now() + Duration::from_secs(3)),
        dev_open_settings_at: std::env::var("MARSPOT_DEV_OPEN_SETTINGS")
            .is_ok()
            .then(|| Instant::now() + Duration::from_secs(2)),
        layout_discarded: false,
        windows: vec![{
            let mut w = WindowState::new(
                FIRST_WINDOW_ID,
                panes,
                initial_focused_idx,
                grid_cols,
                grid_rows,
                w_phys,
                h_phys,
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
    // Safe-mode boot: L1 would refuse these anyway; not asking keeps
    // the intent visible on both sides of the wire.
    if std::env::var("MARSPOT_SAFE_MODE").is_err() {
        let slots: Vec<u32> =
            app.saved_windows.iter().map(|(slot, _)| *slot as u32).collect();
        for slot in slots {
            app.pending_to_shell.push((
                MsgType::WindowOpenRequest,
                marspot::shell_proto::encode_window_open_request(slot),
            ));
        }
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

    // The thread that paints is the thread the user is waiting on.
    // Measured on an idle bench host with background load as the only
    // variable: at default QoS a frame's p99 went from 293 µs (idle)
    // to 6,082 µs (load 11), while the GPU's own account of the same
    // frame never moved (111 → 115 µs).  Nothing got heavier; this
    // thread simply stopped being scheduled.  Raising it to
    // USER_INTERACTIVE, same machine and load, seconds apart, both
    // orderings: p99 325 µs — the tail is gone.  See `marspot::qos`.
    let qos_ok = marspot::qos::raise_current_thread_to_user_interactive();
    lx_event!(
        "CORE_LOOP",
        "entering event loop (event-driven, no fixed cadence)",
        qos_user_interactive = qos_ok as u32
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
            // A frame on the GPU has to be reaped by a poll — nothing
            // sends us an event when it lands — so while one is in
            // flight the loop wakes often enough to present it
            // promptly.  This costs nothing at rest: with no frame
            // committed there is nothing to poll, and the idle timeout
            // stays a full second, which is what keeps CPU at rest
            // near zero (a hard project constraint).
            const SETTLE_POLL: Duration = Duration::from_millis(1);
            let recv_timeout = app
                .windows
                .iter()
                .filter(|w| w.needs_render || w.render.frame_in_flight())
                .map(|w| {
                    if w.render.frame_in_flight() {
                        SETTLE_POLL
                    } else {
                        Duration::MAX
                    }
                })
                .chain(
                    app.windows
                        .iter()
                        .filter(|w| w.needs_render)
                // One reading of the clock, not two.  The first cut
                // asked `t.elapsed()` in the guard and again in the
                // body, and time passes between them: an elapsed that
                // was a hair under the interval when tested could be a
                // hair over when subtracted, and `Duration - Duration`
                // panics on underflow.  It took twelve hours of logs
                // to hit once — `overflow when subtracting durations`,
                // straight through `main`, taking the window with it.
                        .map(|w| match w.last_render_at {
                            Some(t) => frame_min_interval.saturating_sub(t.elapsed()),
                            None => Duration::ZERO,
                        }),
                )
                .min()
                .unwrap_or(Duration::from_secs(1));
            let recv_timeout = if app.any_prediction_pending() {
                recv_timeout.min(Duration::from_millis(10))
            } else {
                recv_timeout
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
        // The process panel is per-window: two windows can each have
        // one open, and a kill pending in an unfocused window still has
        // to escalate on schedule.
        // Dev seam (`MARSPOT_DEV_CLOSE_PANES=n`): close the focused
        // pane of the key window n times, a second apart.  A script
        // cannot click the pane's [×], and "closing the last pane
        // closes the window / quits the app" is otherwise untestable.
        // Unset in the installed app.
        app.expire_stale_predictions();
        app.dev_drive_close_panes();
        // Dev seam (`MARSPOT_DEV_OPEN_SETTINGS=1`): open the settings
        // panel once, shortly after boot, so a sandbox run can be
        // screenshotted.  How a panel *looks* has no test — the only
        // check is to look at it, and driving the toolbar click from a
        // script needs accessibility permission the sandbox has not
        // got.  Unset in the installed app.
        app.dev_drive_open_settings();
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
        //   3) re-read every pane's shell cwd every
        //      CWD_SWEEP_INTERVAL so the title-strip placeholder
        //      follows `cd` on its own clock — the keyboard cannot
        //      tell us a directory changed (see CWD_SWEEP_INTERVAL).
        //      Interval-gated inside, and only repaints the windows
        //      whose cwds actually moved.
        app.sweep_pane_cwds();
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
                CoreEvent::MouseDrag(x, y, win, hw, hx, hy) => {
                    if let Some(wi) = app.drag_target(win) {
                        app.mouse_drag(wi, x, y, hw, hx, hy)
                    }
                }
                CoreEvent::MouseUp(win, drop, dx, dy) => {
                    if let Some(wi) = app.drag_target(win) {
                        app.mouse_up(wi, drop, dx, dy)
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
                CoreEvent::WindowChrome(win, lights) => {
                    app.apply_window_chrome(win, lights);
                }
                CoreEvent::WindowClosed(win) => app.close_window(win),
                CoreEvent::SurfaceAttachWindow(fr, bk, w, h, sc, win, slot, lights) => {
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
                        app.adopt_window(win, w, h, sc, slot.map(|s| s as usize));
                    }
                    // Where the OS's own window buttons end, as the
                    // shell measured them.  `None` = a shell that
                    // predates the field; keep whatever we had.
                    if let Some(px) = lights {
                        app.apply_window_chrome(win, px);
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
                CoreEvent::PaneWheelKeys(sid, enter, up, down, marker) => {
                    app.set_pane_wheel_keys(sid, enter, up, down, marker);
                }
                CoreEvent::PaneRecede(sid, level) => {
                    app.set_pane_recede(sid, level);
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
                CoreEvent::PaneHoldGrid(sid, on) => {
                    app.forward_pane_hold_grid(sid, on);
                }
                CoreEvent::PaneRenderMarkup(sid, on) => {
                    app.forward_pane_render_markup(sid, on);
                }
                CoreEvent::PaneInjectPaste(sid, text) => {
                    app.forward_pane_paste(sid, &text);
                }
                CoreEvent::PaneResetMouseReporting(sid) => {
                    app.forward_pane_reset_mouse_reporting(sid);
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
        // Tell L1 which pane the user is on, when it changes.  One
        // comparison per window per loop iteration, and a frame only
        // on an actual change — focus moves at human speed.
        //
        // L1 plugins need it because "the user is looking at this pane
        // again" is the earliest honest moment to start restoring
        // something reclaimed while idle.  Waiting for a keystroke
        // means the restore begins after they have already tried to
        // use the pane, which is indistinguishable from slowness.
        app.notify_pane_focus_changes();
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
            let _ = app.render_blocking(wi, &tex);
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
            // Its last frame is still on the GPU.  Starting another
            // would overwrite the instance buffers that frame is
            // reading, and the whole reason we no longer wait is to
            // spend this time on input instead.  `needs_render` stays
            // set, so the frame goes out as soon as the GPU is done.
            if win!(app, wi).render.frame_in_flight() {
                continue;
            }
            // A window with no paint target (stale ids at attach) is
            // skipped, not fatal: its peers keep painting and the next
            // attach gives it one.
            let Some(tex) = win!(app, wi).surfaces.as_ref().map(|s| s.writing_tex()) else {
                continue;
            };
            // Double-buffer: render into the back slot.  The frame is
            // committed and left running; the pass below flips only
            // once the GPU reports it complete, so `SurfaceReady`
            // still names a slot the shell can safely sample — that
            // property is what makes it the dual-buffer race fix, and
            // it survives the move off the blocking wait.
            let render_t0 = Instant::now();
            win!(app, wi).last_render_at = Some(render_t0);
            watch.phase("render");
            let caret = app.render(wi, &tex);
            watch.phase("post-render");
            // The frame is on the GPU, not finished.  Hold the caret
            // and let the poll below flip and announce it — the shell
            // must still only ever be pointed at a settled surface.
            win!(app, wi).pending_caret = caret;
            continue;
        }

        // Frames that finished since the last pass: flip to them and
        // tell the shell.  This is the half of the old blocking render
        // that had to stay synchronous — it just no longer costs the
        // main loop the GPU's queueing time.
        for wi in 0..app.windows.len() {
            if !win!(app, wi).render.settled() {
                continue;
            }
            let caret = win!(app, wi).pending_caret.take();
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
                dur_us = win!(app, wi)
                    .last_render_at
                    .map(|t| t.elapsed().as_micros() as u64)
                    .unwrap_or(0),
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
            // `render` being the slowest phase is where the trail used
            // to go cold — "render took 41 s" names no cost you can
            // attack.  The split rides along on every stall report so
            // the next one arrives already decomposed into CPU
            // instance-building (glyph rasterisation included, with a
            // count), command encoding, and the GPU wait.
            lx_warn!(
                "l2.loop.stall",
                &r.summary(),
                slowest = r.slowest,
                slowest_ms = r.slowest_took.as_millis(),
                breakdown = r.breakdown(),
                render_split = app.renderer_split().summary(),
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

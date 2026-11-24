// CARVE-OUT: pre-existing god file: the L2 renderer and input-dispatch process; to be split by concern; shrink on every touch
//! marspot-core — the renderer + input-dispatch half of the
//! silent-update split.
//!
//! Spawned by `marspot-shell` as a child; renders the full marspot
//! multi-pane UI into the shared IOSurface the shell created,
//! attaches to shelld sessions, and processes input forwarded over
//! the control socket.
//!
//! Features: 9-grid (all `LayoutMode`s via the [layout] picker), sidebar
//! with focus rows / close [×] / add [+], click-to-focus, per-pane
//! scrollback scrolling, drag selection (linewise + Option-blockwise)
//! with Cmd-C copy, cell-title editing, IME preedit rendering, and
//! Cmd-B sidebar toggle.  The shared state shapes and pure logic
//! live in `marspot::ui`; this file owns the socket-event plumbing.

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
use marspot::latency_trace::{self as trace, Stage};
use marspot::layout::Layout;
use marspot::pane::{L3Conn, L3Spawn, Pane};
use marspot::render::{SessionView, SidebarEntry};
use marspot::render_metal::MetalRenderer;
use marspot::ui::components::surface_stack::{
    self, Change, CloseBy, Key as SurfaceKey, KeyAction, Press, SurfaceKind, SurfaceStack,
};
use marspot::ui::components::panel_script::{Input as DevInput, Scene, Script as PanelScript};
use marspot::ui::components::{PanelChrome, GRID_MAX, GRID_MIN};
use marspot::ui::components::chrome_target::ChromeTarget;
use marspot::ui::strings::{self, t, Arg, Msg};
use marspot::session::SessionState;
use marspot::session_registry::{
    self, allocate_next_session_id, list_session_entries,
};
use marspot::shell_proto::{
    decode_selection_text, encode_hello_ack, encode_pong, Frame, MsgType, DEFAULT_CONTROL_FD, ENV_CONTROL_FD, ENV_SURFACE_HEIGHT,
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
    /// Pid of each row of the process table, in the order drawn.
    row_pids: Vec<i32>,
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
    /// Where everything in the frame on screen is; clicks, the
    /// pointer and the wheel are all answered from it.  `None` until
    /// the first frame is built.
    layout: Option<marspot::ui::components::process_panel::Layout>,
    surface: std::rc::Rc<std::cell::RefCell<marspot::ui::components::process_view::ProcessSurface>>,
    /// How far the process list is pushed up, in physical pixels.
    scroll_y: marspot::ui::Units,
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
/// verbatim, which is three past the point where the same pattern
/// gets extracted.  `which` discriminates them in the log — one event name to
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
    /// Show a file link's file selected in its folder.
    RevealLink,
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
            x if x == Self::RevealLink.tag() => Some(Self::RevealLink),
            _ => None,
        }
    }
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
    use marspot::iosurface::IOSurface;

    pub(super) fn win(id: u32, panes: Vec<Pane>) -> WindowState {
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

    use marspot_term::test_env::{sandbox_state_dir, StateSandbox};

    /// Panics unless the calling test holds `sandbox` and the state
    /// root is still the one it made: a test that forgot to take one
    /// would otherwise write into whatever root the process has.
    fn assert_sandboxed(sandbox: &StateSandbox) {
        let resolved = marspot::state::state_file_path();
        assert!(
            resolved.starts_with(sandbox.path()),
            "state path is not in this test's sandbox: {}",
            resolved.display()
        );
    }

    /// The guard that would have caught the 2026-07-27 incident: no
    /// test in this module may run against the real state dir.
    #[test]
    fn tests_never_touch_the_real_state_dir() {
        let sandbox = sandbox_state_dir();
        assert_sandboxed(&sandbox);
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
    /// **Refuses to build without the test's own state dir.**  A `CoreApp` reaches
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
    pub(super) fn app_with(sandbox: &StateSandbox, windows: Vec<WindowState>) -> CoreApp {
        assert_sandboxed(sandbox);
        app_in(sandbox.path(), windows)
    }

    /// `app_with` for a state root the test made some other way.
    pub(super) fn app_in(root: &std::path::Path, windows: Vec<WindowState>) -> CoreApp {
        assert!(marspot::state::state_file_path().starts_with(root), "state path is not in the test's root");
        let (event_tx, _rx) = mpsc::channel();
        CoreApp {
            renderer: MetalRenderer::new_headless().expect("headless renderer"),
            pane_badges: std::collections::HashMap::new(),
            pane_wheel_keys: std::collections::HashMap::new(),
            pane_agent_tui: std::collections::HashMap::new(),
            pane_wheel_enter_at: std::collections::HashMap::new(),
            pane_wheel_open: std::collections::HashMap::new(),
            wheel_sel_path: None,
            pane_titles: std::collections::HashMap::new(),
            pane_cwds: std::collections::HashMap::new(),
            pending_to_shell: Vec::new(),
            last_focus_notified: std::collections::HashMap::new(),
            terminal_focus: None,
            pane_sessions: std::collections::HashMap::new(),
            esc_history: std::collections::HashMap::new(),
            shell_child_pids: std::collections::HashMap::new(),
            last_cwd_sweep: Instant::now() - CWD_SWEEP_INTERVAL,
            last_cwd_save: Instant::now() - CWD_SAVE_MIN_GAP,
            cwd_save_pending: false,
            reconnecting: std::collections::HashSet::new(),
            all_exited: false,
            saw_window_aware_attach: false,
            event_tx,
            drag_window: None,
            pane_drag: None,
            scrollbar_drag: None,
            mouse_report_press: None,
            mouse_report_last_cell: None,
            drop_target: None,
            pending_move_sid: None,
            saved_windows: std::collections::VecDeque::new(),
            parked_windows: Vec::new(),
            dev_close_panes: 0,
            dev_close_panes_at: None,
            dev_open_settings_at: None,
            dev_drag: None,
            dev_panels: None,
            pane_control: Default::default(),
            strips: Default::default(),
            layout_discarded: false,
            windows,
            shadows: Vec::new(),
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
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![win(1, Vec::new()), win(2, Vec::new())]);
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
            let (f, b) = s.ids();
            (f, b, s.writing_idx)
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
        app.windows[1].surfaces.as_mut().unwrap().announce();
        assert_eq!(pair_of(&app, 0).2, 0, "peer's writing half flipped");
        assert_eq!(pair_of(&app, 1).2, 1);

        // An attach naming a window the core doesn't have is a normal
        // race (the window closed first), not a reason to touch anyone.
        assert_eq!(app.attach_window_surfaces(99, a_f, a_b, 10.0, 10.0, 1.0), None);
        assert_eq!(pair_of(&app, 0), (a_f, a_b, 0));
    }

    /// A shell that shows the surfaces themselves has the one before
    /// the frame just announced on the screen until it says otherwise,
    /// and that is the surface the next frame would be drawn into.
    #[test]
    fn a_paced_window_draws_nothing_until_its_last_frame_is_reported_shown() {
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![win(1, Vec::new()), win(2, Vec::new())]);
        let a = [IOSurface::create(64, 64).expect("front"), IOSurface::create(64, 64).expect("back")];
        let b = [IOSurface::create(64, 64).expect("front"), IOSurface::create(64, 64).expect("back")];
        app.attach_window_surfaces(1, a[0].id(), a[1].id(), 64.0, 64.0, 1.0).expect("attached");
        app.attach_window_surfaces(2, b[0].id(), b[1].id(), 64.0, 64.0, 1.0).expect("attached");

        // a shell that never said it paces is never waited for
        let first = app.windows[0].surfaces.as_mut().unwrap().announce();
        assert_eq!(first, a[0].id());
        assert!(!frames::waits(&app.windows[0]), "waited for a shell that reports nothing");

        frames::pace();
        let second = app.windows[0].surfaces.as_mut().unwrap().announce();
        assert_eq!(second, a[1].id());
        assert!(frames::waits(&app.windows[0]));
        assert!(!frames::waits(&app.windows[1]), "one window's frame held another's");
        // a report about a frame since replaced, or about another window
        frames::shown(&mut app, 1, first);
        frames::shown(&mut app, 2, second);
        assert!(frames::waits(&app.windows[0]), "let go by a report that was not about its last frame");
        // the pair it already has, attached again, is no new paint target
        app.attach_window_surfaces(1, a[0].id(), a[1].id(), 64.0, 64.0, 1.0).expect("attached");
        assert!(frames::waits(&app.windows[0]), "the same pair attached again cleared the wait");
        frames::shown(&mut app, 1, second);
        assert!(!frames::waits(&app.windows[0]));
        // a new pair has never been on the screen
        app.windows[0].surfaces.as_mut().unwrap().announce();
        app.attach_window_surfaces(1, b[0].id(), b[1].id(), 64.0, 64.0, 1.0).expect("attached");
        assert!(!frames::waits(&app.windows[0]));
    }

    /// The core exits for a restart only when every pane's session is
    /// lost, across all windows.  A live pane in any window keeps it
    /// up, however long its window has been out of focus.
    #[test]
    fn the_app_exits_only_when_every_window_is_dead() {
        let sandbox = sandbox_state_dir();
        // Vacant = born exited; pending = a spawn in flight, i.e.
        // alive — but only for `PENDING_MAX` (30 s), so the pane is
        // built *after* the renderer (whose construction can take
        // minutes on a loaded machine) and pumped immediately.
        let dead = || Pane::new_vacant(1, 80, 24);
        let alive = || Pane::new_pending(2, 80, 24);

        let mut app = app_with(&sandbox, vec![win(1, Vec::new()), win(2, Vec::new())]);
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
        let mut sandbox = sandbox_state_dir();
        use marspot::ui::components::{GRID_MAX, GRID_MIN};
        sandbox.env.set("MARSPOT_SESSION_BIN", "/nonexistent/marspot-session");
        for cols in GRID_MIN..=GRID_MAX {
            let record = marspot::state::SavedWindowLayout {
                grid_cols: cols as u16,
                grid_rows: 2,
                focused_idx: 0,
                panes: vec![marspot::state::SavedPane { sid: 11, ..Default::default() }],
            };
            let mut app = app_with(&sandbox, vec![win(1, vec![Pane::new_vacant(1, 80, 24)])]);
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
        let mut sandbox = sandbox_state_dir();
        // Point the assembly worker at a binary that cannot start, so
        // it fails fast instead of spawning real L3s behind the test.
        sandbox.env.set("MARSPOT_SESSION_BIN", "/nonexistent/marspot-session");
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
        let mut app = app_with(&sandbox, vec![win(1, vec![Pane::new_vacant(1, 80, 24)])]);
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
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![
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
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![
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
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![win(1, Vec::new()), win(2, Vec::new())]);
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
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![win(1, Vec::new()), win(2, Vec::new())]);
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
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![win(1, Vec::new()), win(2, Vec::new())]);

        app.toggle_panel(1, SurfaceKind::Accounts);
        assert!(app.windows[1].cc_usage_modal.is_some());
        assert!(app.windows[0].cc_usage_modal.is_none(), "peer must stay closed");
        assert_eq!(app.windows[0].panels.open(), None);

        app.toggle_panel(1, SurfaceKind::Accounts);
        assert!(app.windows[1].cc_usage_modal.is_none(), "toggles off again");
    }

    /// The four panels open and close by one set of rules: a toggle
    /// replaces whatever was open, Esc and Cmd-W close any of them, and
    /// a panel's own data lives exactly as long as it is open.
    #[test]
    fn every_panel_opens_alone_and_closes_on_esc_and_cmd_w() {
        use marspot::input::{KeyState, LogicalKey, NamedKey};
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![win(1, Vec::new())]);
        let press = |logical, super_| {
            let event = MarspotKeyEvent { logical, state: KeyState::Pressed, ..Default::default() };
            (event, Modifiers { super_, ..Default::default() })
        };
        let data = |app: &CoreApp| {
            let w = &app.windows[0];
            (w.process_panel.is_some(), w.cc_usage_modal.is_some())
        };
        for kind in SurfaceKind::PANELS {
            for (logical, super_) in
                [(LogicalKey::Named(NamedKey::Escape), false), (LogicalKey::Char('w'), true)]
            {
                // Over another panel, so the open is also a replace.
                let other = SurfaceKind::PANELS.into_iter().find(|o| *o != kind).unwrap();
                app.toggle_panel(0, other);
                app.toggle_panel(0, kind);
                app.toggle_panel(0, kind);
                app.toggle_panel(0, kind);
                assert_eq!(app.windows[0].panels.open(), Some(kind));
                assert_eq!(
                    data(&app),
                    (kind == SurfaceKind::Processes, kind == SurfaceKind::Accounts),
                    "{kind:?}: only the open panel holds data"
                );
                let (event, mods) = press(logical.clone(), super_);
                app.key(0, event, mods);
                assert_eq!(app.windows[0].panels.open(), None, "{kind:?} {logical:?}");
                assert_eq!(data(&app), (false, false), "{kind:?} {logical:?}");
            }
        }
    }

    /// The Esc-three-times escape hatch is about one pane refusing to
    /// give the keyboard back, so its history is per session.  Global,
    /// presses aimed at one pane could force-end another — reachable
    /// as soon as two windows each hold a locked pane.
    #[test]
    fn the_escape_hatch_counts_presses_per_session() {
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![win(1, Vec::new())]);
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
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![
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
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![
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
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![
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
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![
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
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![
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
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![win(1, vec![Pane::new_vacant(10, 80, 24)])]);
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
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![win(1, vec![
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
        let sandbox = sandbox_state_dir();
        let mk = || {
            let mut app = app_with(&sandbox, vec![
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
        app.mouse_drag(0, 100.0 + PANE_DRAG_SLOP + 1.0, 10.0, 0, 0.0, 0.0);
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
        let r = CellRect { x: 0.0, y_top: 0.0, w: 400.0, h: 400.0, cols: 10, rows: 10, strip_rows: 0, term_bottom: 400.0 };
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
        let slim = CellRect { x: 0.0, y_top: 0.0, w: 90.0, h: 400.0, cols: 3, rows: 10, strip_rows: 0, term_bottom: 400.0 };
        // floor lifts 22.5px→48, cap trims to w/3 = 30.
        assert_eq!(drop_zone_at(&slim, 29.0, 200.0), DropZone::Left, "inside the 30px band");
        assert_eq!(drop_zone_at(&slim, 31.0, 200.0), DropZone::Center, "cap = w/3 keeps a center");
    }

    /// RFC-006 §2 flagship — a 1×1 window splits into 1×2 / 2×1 when
    /// a pane is dropped on its only pane's edge, and the dragged
    /// pane lands exactly where the preview said.
    #[test]
    fn dropping_on_a_1x1_window_splits_it() {
        let sandbox = sandbox_state_dir();
        let mk = || app_with(&sandbox, vec![
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

    /// The last live pane of a window can only be dragged out when it
    /// has a title strip to grab, and a lone pane has none.  The case
    /// that exists is a live pane beside a placeholder: it keeps its
    /// strip, and splitting it into another window empties the source,
    /// which then asks L1 to close it.
    #[test]
    fn splitting_out_the_last_live_pane_closes_its_window() {
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![
            win(1, vec![Pane::new_vacant(10, 80, 24), Pane::new_dormant(80, 24)]),
            win(2, vec![Pane::new_vacant(20, 80, 24)]),
        ]);
        app.rebuild_layout(0);
        app.rebuild_layout(1);
        assert_eq!(app.windows[1].layout.cell_title_h, 0.0, "a lone pane has no strip");
        assert!(
            app.windows[0].layout.cell_title_h > 0.0,
            "a live pane beside a placeholder keeps its strip"
        );

        app.split_insert(0, 0, 1, 0, DropZone::Right);

        assert!(app.windows[0].panes.iter().all(|p| p.is_dormant()));
        assert_eq!((app.windows[1].grid_cols, app.windows[1].grid_rows), (2, 1));
        assert_eq!(app.windows[1].panes[1].shelld_session_id(), Some(10));
        assert!(
            app.pending_to_shell
                .iter()
                .any(|(ty, _)| *ty == MsgType::WindowCloseRequest),
            "the emptied source window asks L1 to close it"
        );
    }

    /// Same-window edge drop is a REARRANGE: no placeholder, the pane
    /// count is unchanged, only the order moves.
    #[test]
    fn splitting_within_a_window_rearranges_without_placeholders() {
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![win(1, vec![
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
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![
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
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![win(1, vec![
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
        let sandbox = sandbox_state_dir();
        let mut panes: Vec<Pane> = (0..36).map(|i| Pane::new_vacant(100 + i, 80, 24)).collect();
        let extra = Pane::new_vacant(10, 80, 24);
        let mut app = app_with(&sandbox, vec![
            win(1, vec![extra]),
            win(2, std::mem::take(&mut panes)),
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
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![
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
    fn write_entry_with_child_pid(sandbox: &StateSandbox, sid: u64, pid: i32) {
        assert_sandboxed(sandbox);
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
        let sandbox = sandbox_state_dir();
        let sid = 7701;
        write_entry_with_child_pid(&sandbox, sid, std::process::id() as i32);
        let mut app = app_with(&sandbox, vec![win(1, vec![Pane::new_vacant(sid, 80, 24)])]);

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
        let sandbox = sandbox_state_dir();
        let sid = 7702;
        write_entry_with_child_pid(&sandbox, sid, std::process::id() as i32);
        let mut app = app_with(&sandbox, vec![win(1, vec![Pane::new_vacant(sid, 80, 24)])]);
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
        let sandbox = sandbox_state_dir();
        let sid = 7703;
        write_entry_with_child_pid(&sandbox, sid, std::process::id() as i32);
        let mut app = app_with(&sandbox, vec![win(1, vec![Pane::new_vacant(sid, 80, 24)])]);

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
        let sandbox = sandbox_state_dir();
        let sid = 7704;
        write_entry_with_child_pid(&sandbox, sid, std::process::id() as i32);
        let mut app = app_with(&sandbox, vec![win(1, vec![Pane::new_vacant(sid, 80, 24)])]);
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
#[path = "marspot-core/boot_assembly_tests.rs"]
mod boot_assembly_tests;
#[cfg(test)]
#[path = "marspot-core/real_pty_tests.rs"]
mod real_pty_tests;
#[cfg(test)]
#[path = "marspot-core/shell_end_tests.rs"]
mod shell_end_tests;

/// cc — state behind the toolbar `Cc` (Claude usage) modal.
struct CcUsageModalState {
    /// Parsed feed; `None` = file missing/unreadable (modal shows a
    /// placeholder instead of closing).
    data: Option<marspot::cc_usage::CcUsage>,
    codex: Option<marspot::cc_usage::CodexUsage>,
    loaded_at: Instant,
    surface: std::rc::Rc<std::cell::RefCell<marspot::ui::components::cc_usage_view::CcUsageSurface>>,
}

struct ContextMenuState {
    items: std::rc::Rc<[marspot::ui::components::MenuItem]>,
    anchor_x: marspot::ui::Units,
    anchor_y: marspot::ui::Units,
    region: ContextRegion,
    surface: std::rc::Rc<std::cell::RefCell<marspot::ui::components::context_menu_view::MenuSurface>>,
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
    /// The shell shows our surfaces directly and reports each shown.
    FramePace,
    /// (window, surface): that surface is on the window and the one
    /// before it has left the screen.
    FrameShown(u32, u32),
    /// Shell sent a liveness probe.  We echo the nonce back via PONG.
    Ping(u32),
    /// The shell drew a toolbar icon this core asked for:
    /// `(symbol, point_px, side, mask)`.
    IconMask(u16, u16, u16, Vec<u8>),
    /// Shell closed the control socket — supervisor will tear us down.
    Closed,
    /// An L3 session process published a new grid into
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
    /// This session's pane rang the bell; the flag is the reader's
    /// "one already queued", cleared when this event is handled.
    PaneBell(u64, std::sync::Arc<std::sync::atomic::AtomicBool>),
    /// A session said which grid region it publishes into, which frames it reads, or that its pane woke.
    L3Says(u64, Frame),
    /// L1 asked for something of a pane; session `.0` reported on such a request.
    ShellPaneControl(Frame),
    PaneReport(u64, Frame),
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
    PaneAgentTui(u64, bool),
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

/// The drag endings the `MARSPOT_DEV_DRAG` seam can play (RFC-006 §8
/// step 6).  Which *result* each one produces also depends on the
/// layout the script seeds — `SplitRight` from a 2×1 window leaves a
/// dormant placeholder behind, from a 1×1 window closes it.
#[derive(Clone, Copy, Debug)]
enum DevDrag {
    /// Window 0's first pane onto window 1's first pane, right band.
    SplitRight,
    /// Window 0's first pane released outside every marspot window.
    NewWindow,
}

impl DevDrag {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "split-right" => Some(Self::SplitRight),
            "new-window" => Some(Self::NewWindow),
            _ => None,
        }
    }
}

/// How far (window units) the pointer must travel from the press
/// before a title-bar hold becomes a pane drag.  Generous enough
/// that an ordinary click never trips it on a shaky hand.
const PANE_DRAG_SLOP: f64 = 10.0;

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
    let band_w = (rect.w * 0.25).max(48.0 * marspot::ui::chrome_scale()).min(rect.w / 3.0);
    let band_h = (rect.h * 0.25).max(48.0 * marspot::ui::chrome_scale()).min(rect.h / 3.0);
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
    /// When this window last beeped (`bell::ring_window`).
    last_bell_at: Option<Instant>,
    /// The caret of a frame that is committed but not yet settled.
    ///
    /// It travels with the frame: announcing it before the surface is
    /// shown would point the IME at a position the user cannot see yet.
    pending_caret: Option<(f64, f64, f64, f64)>,
    /// Its first frame, and its first frame showing every pane's own
    /// output: one INFO line each (`frames::note_drawn`).
    first_frames: frames::FirstFrames,
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
    /// Set while a selection stands on a pane whose picture the
    /// *program* owns (a full-screen program on the alt screen).  Such
    /// a pane files nothing into marspot's scrollback and scrolls by
    /// repainting, so the tape reads the shift off each frame and
    /// keeps the selection on its text.  Lifetime is tied to
    /// `selection` in `sync_selection_tape`, not to the fourteen
    /// places that clear a selection.
    selection_tape: Option<marspot::selection_tape::SelectionTape>,
    /// While a tape is following a pane, the wheel is forwarded one
    /// step at a time: the next step waits until the tape has read the
    /// last one off a frame, or until this instant passes.
    ///
    /// Open-loop forwarding was the defect behind "稍微快一丁点就整个
    /// 选择区都没有了": a momentum scroll delivers events far faster
    /// than frames are folded, so several steps landed between two
    /// folds and their combined distance left no overlap to align on.
    /// Capping one event's distance could not fix that — the cap is
    /// per event and the problem is per frame.
    ///
    /// The deadline is the backstop for a step that moves nothing (the
    /// program is already at the end of its own history, so no frame
    /// arrives to clear the gate) — without it the pane would stop
    /// scrolling for good.
    tape_scroll_gate: Option<std::time::Instant>,
    selection_dragging: bool,
    /// F3+3.0 — grid shape (cols × rows) is now an arbitrary
    /// pair rather than a 7-variant enum.  Cell total = cols×rows;
    /// when N panes > cells the overflow stays in the sidebar.
    /// User changes via the `LayoutModal`.
    grid_cols: usize,
    grid_rows: usize,
    /// Which panel is open over this window's panes, and the rules it
    /// opens, closes and moves by.  Per window: opening a panel in one
    /// must not put it in another.
    panels: SurfaceStack,
    /// Right edge of the OS's traffic-light cluster, physical px from
    /// the window's left edge; `Some(0.0)` when it is not on screen
    /// (full screen takes it away), **`None` until the shell has
    /// measured**.  The toolbar starts after it.
    ///
    /// `None` is not `Some(0.0)`: a window that has not been measured
    /// yet must keep clear of where the buttons normally are, or its
    /// first frames draw the toolbar on top of them.
    lights_right_phys: Option<f64>,
    /// Width of the widest layout-panel card label, physical px, as of
    /// the last published frame.  The panel picks its width from it, so
    /// the hit-test uses the number that produced what is on screen.
    layout_modal_label_w: f64,
    /// What the pointer is on in the open layout panel.
    layout_surface: std::rc::Rc<std::cell::RefCell<marspot::ui::components::layout_view::LayoutSurface>>,
    /// What each slot of the layout panel holds, as last drawn.
    layout_titles: Vec<String>,
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
    /// The process monitor's data while it is open; libproc is only
    /// walked while this is `Some`.
    process_panel: Option<ProcessPanelState>,
    /// The accounts panel's data while it is open; the feed file is
    /// only read while this is `Some`.
    cc_usage_modal: Option<CcUsageModalState>,
    /// How far the settings panel's content is pushed up inside its
    /// rect.  Only ever nonzero on a window too short to show it whole.
    settings_modal_scroll: marspot::ui::Units,
    /// The settings panel's surface: its layout, what the pointer and
    /// the keyboard have done to it, and what it last painted.
    settings_surface: std::rc::Rc<std::cell::RefCell<marspot::ui::components::settings_view::SettingsSurface>>,
    /// Where the settings file is, as the panel's footer shows it.
    settings_path: std::sync::Arc<str>,
    /// The shortcuts panel's surface.
    shortcuts_surface: std::rc::Rc<std::cell::RefCell<marspot::ui::components::shortcuts_view::ShortcutsSurface>>,
    /// The helper panel's data while it is open.
    helpers_panel: Option<helper_panel::HelperPanelState>,
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
            last_bell_at: None,
            pending_caret: None,
            first_frames: frames::FirstFrames::default(),
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
            selection_tape: None,
            tape_scroll_gate: None,
            grid_cols,
            grid_rows,
            panels: SurfaceStack::default(),
            lights_right_phys: None,
            layout_modal_label_w: 0.0,
            layout_surface: Default::default(),
            layout_titles: Vec::new(),
            context_menu: None,
            pending_grid_cols: grid_cols,
            pending_grid_rows: grid_rows,
            card_slots: (0..(grid_cols * grid_rows)).collect(),
            layout_drag: None,
            sidebar_collapsed: true,
            process_panel: None,
            cc_usage_modal: None,
            settings_modal_scroll: Default::default(),
            settings_surface: Default::default(),
            settings_path: std::sync::Arc::from(marspot::settings::path().display().to_string()),
            shortcuts_surface: Default::default(),
            helpers_panel: None,
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

#[path = "marspot-core/frames.rs"]
mod frames;
use frames::WindowSurfaces;
#[path = "marspot-core/hover.rs"]
mod hover;
#[path = "marspot-core/link_menu.rs"]
mod link_menu;
use link_menu::{link_menu_items_for, LinkContext};
#[path = "marspot-core/chrome_input.rs"]
mod chrome_input;
#[path = "marspot-core/idle.rs"]
mod idle;
#[path = "marspot-core/shell_end.rs"]
mod shell_end;
#[path = "marspot-core/keys.rs"]
mod keys;
#[path = "marspot-core/probe_args.rs"]
mod probe_args;
#[path = "marspot-core/reattach.rs"]
mod reattach;
use reattach::reattach_l3_pane;
#[path = "marspot-core/tree_panels.rs"]
mod tree_panels;
use marspot::ui::components::layout_modal::Hover as LayoutAsk;
use marspot::ui::components::process_panel::Hover as ProcessAsk;
use marspot::ui::components::{layout_view, process_view};
#[path = "marspot-core/theme_follow.rs"]
mod theme_follow;
#[path = "marspot-core/bell.rs"]
mod bell;
#[path = "marspot-core/l3_reader.rs"]
mod l3_reader;
#[path = "marspot-core/shell_reader.rs"]
mod shell_reader;
#[path = "marspot-core/pane_control.rs"]
mod pane_control;
#[path = "marspot-core/pane_reports.rs"]
mod pane_reports;
#[path = "marspot-core/shadows.rs"]
mod shadows;
#[path = "marspot-core/spawn_said.rs"]
mod spawn_said;
#[path = "marspot-core/shadow_place.rs"]
mod shadow_place;
#[path = "marspot-core/shadow_boot.rs"]
mod shadow_boot;
#[path = "marspot-core/unwatched.rs"]
mod unwatched;
#[path = "marspot-core/pump.rs"]
mod pump;
use l3_reader::l3_reader_loop;
#[path = "marspot-core/scrollbar_input.rs"]
mod scrollbar_input;
#[path = "marspot-core/boot_assembly.rs"]
mod boot_assembly;
use boot_assembly::assemble_panes_at_boot;
#[path = "marspot-core/spawn.rs"]
mod spawn;
use spawn::{
    assemble_restore_window_async, live_cwd_of, spawn_l3_pane_async, spawn_l3_pane_with_cwd,
};
#[path = "marspot-core/sessions.rs"]
mod sessions;
#[path = "marspot-core/windows.rs"]
mod windows;
#[path = "marspot-core/panes.rs"]
mod panes;
#[path = "marspot-core/pane_move.rs"]
mod pane_move;
#[path = "marspot-core/context_menu.rs"]
mod context_menu;
#[path = "marspot-core/card_slots.rs"]
mod card_slots;
#[path = "marspot-core/dev_drive.rs"]
mod dev_drive;
#[path = "marspot-core/layout_state.rs"]
mod layout_state;
#[path = "marspot-core/pane_forward.rs"]
mod pane_forward;
#[path = "marspot-core/panels.rs"]
mod panels;
#[path = "marspot-core/helper_strip/mod.rs"]
mod helper_strip;
#[path = "marspot-core/helper_panel.rs"]
mod helper_panel;
#[path = "marspot-core/process_panel.rs"]
mod process_panel;
#[path = "marspot-core/links.rs"]
mod links;
#[path = "marspot-core/search.rs"]
mod search;
#[path = "marspot-core/pointer.rs"]
mod pointer;
#[path = "marspot-core/pointer_press.rs"]
mod pointer_press;
#[path = "marspot-core/pointer_drag.rs"]
mod pointer_drag;
#[path = "marspot-core/wheel.rs"]
mod wheel;
#[path = "marspot-core/tape.rs"]
mod tape;
#[path = "marspot-core/draw.rs"]
mod draw;

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
    /// A plugin's explicit "an agent TUI paints this pane" declaration
    /// (`MsgType::PaneAgentTui`), re-issued every tick.  Replaces
    /// inferring it from a wheel-key declaration or a non-empty badge:
    /// both are sent once or only on change, so a core swap left the
    /// new L2 with neither and the pane lost its TUI-shaped link
    /// scanning until something happened to change.  Absent means an
    /// L1 that does not send it yet — the old inference is the
    /// fallback, so a mixed-version pair still behaves.
    pane_agent_tui: std::collections::HashMap<u64, bool>,
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
    /// Dev-cycle only — which wheel path a pane with a live selection
    /// took, logged once per change rather than once per tick (a
    /// momentum scroll is hundreds of ticks).  Here because the report
    /// "选择框不会变动，内容滚动就选了不同的文字" matches none of the
    /// three paths as written: the drag path moves nothing on an
    /// alt-screen pane and the release path drops the selection.
    wheel_sel_path: Option<String>,
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
    /// The session whose L3 was last told it has the keyboard.
    terminal_focus: Option<u64>,
    /// RFC-003 pane sessions currently held by L1 plugins, keyed by
    /// shelld_session_id.  Membership routes L2 behaviour:
    ///   * LOCK_KEYS cap → key events forwarded as PaneSessionKey,
    ///     not the PTY
    ///   * FREEZE_GRID cap → render keeps the last-painted instance
    ///     buffer for that pane (C6)
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
    /// Every pane's session is lost (see `every_session_lost`): the
    /// loop exits and L1 starts a core that resurrects them.
    all_exited: bool,
    /// True once a `SurfaceAttachWindow` has arrived.  From then on the
    /// legacy `SurfaceAttach` is ignored as its duplicate — the shell
    /// sends both so that a core predating RFC-005 still gets its
    /// surface.
    saw_window_aware_attach: bool,
    /// Clone of the event channel so a new L3's poke reader (spawned by
    /// [+]) can wake the loop, exactly like the boot spawns.
    event_tx: Sender<CoreEvent>,
    /// Every open window, in creation order.  Never empty: the last
    /// window closing exits the app.
    windows: Vec<WindowState>,
    /// Sessions that run without a window: never laid out or drawn.
    shadows: Vec<Pane>,
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
    /// A press holding a pane's scrollbar, until the button comes up.
    scrollbar_drag: Option<scrollbar_input::ScrollbarDrag>,
    /// The pane a forwarded mouse press went to, and the cell it
    /// landed in.  A program that was told a button went down has to
    /// be told it came up, even if the pointer left the pane first —
    /// otherwise it sits believing the button is still held.
    mouse_report_press: Option<(usize, u32, u32)>,
    /// The last cell a motion report named.  Motion is reported when
    /// the pointer changes cell, not when the mouse moves: a program
    /// wants to know it is over a different character, and the pixels
    /// in between are the terminal's business.
    mouse_report_last_cell: Option<(usize, u32, u32)>,
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
    /// Dev seam only (`MARSPOT_DEV_DRAG`): one scripted pane drag, and
    /// the earliest moment to perform it.
    dev_drag: Option<(DevDrag, Instant)>,
    /// The rows panes give up at their bottoms, by session.
    strips: helper_strip::Strips,
    /// Dev seam only (`MARSPOT_DEV_PANELS`): a script of panel input
    /// still being played.  See `dev_drive_panels`.
    dev_panels: Option<PanelScript>,
    /// Pane-control frames held for L1, and whether it was told the caps.
    pane_control: pane_control::PaneControl,
    /// Set once the user closed the last pane of the last window.  The
    /// saved layout has been deleted at that point and the app is on
    /// its way out; any later save would resurrect what they just
    /// dismantled.
    layout_discarded: bool,
    /// Index into `windows` of the window with keyboard focus.
    key_window: usize,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if probe_args::answer(&args) {
        return;
    }
    // The shell's icon drawer: draw `symbol:point_px,...` to stdout and
    // exit, touching nothing else.  Drawing a system symbol registers
    // this process with LaunchServices, which a core that runs windows
    // must not be; this one lives for the drawing only.
    if args.first().map(String::as_str) == Some("--draw-icons") {
        probe_args::draw_icons(args.get(1).map(String::as_str).unwrap_or(""));
        return;
    }

    // RFC-004 D.1 — must precede logx / any path computation.
    marspot::paths::migrate_legacy_state_root();
    marspot::logx::init("core");
    marspot_term::signals::log_fatal_senders("L2_SIGNAL");
    // Start the async path oracle before the first frame.  Without it
    // `link_probe::oracle()` falls back to the blocking `FsOracle`: right
    // for one-shot renderers, catastrophic for a render loop.
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
    // One L3 process per cell, each owning its own session process + shm
    // grid in its own address space.  L2 owns session *assignment* so the
    // N children never race for one session: reuse the live sessions
    // first (bytelog replay on attach), then `create_session` for the
    // rest, handing each L3 its exact id.
    //
    // A core update lands the new session engine in pending/; promote
    // it into current/ before spawning so each L3 (resolved as core's
    // sibling = current/marspot-session in an installed app) boots the
    // new binary in lockstep with this core.
    match marspot::updater::promote_pending_session() {
        Ok(true) => lx_event!(
            "SESSION_PROMOTE",
            "promoted staged marspot-session → current/ at boot"
        ),
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
    let mut panes = assembled;
    let taken: std::collections::HashSet<u64> =
        panes.iter().filter_map(Pane::shelld_session_id).chain(reserved_sids.iter().copied()).collect();
    let mut boot_shadows = shadow_boot::adopt_shadows_at_boot(&taken, &event_tx);
    // Every reattached L3 not on current/'s build is asked to move, not
    // only after a promote: one whose probe failed last time moves now.
    // Asking one already on it does nothing.  A window restored later
    // asks for its own panes when they land.
    let asked = marspot::session_signal::ask_all_to_update(
        panes
            .iter_mut()
            .filter(|p| p.session().l3_session_id().is_some_and(|sid| reattached_ids.contains(&sid)))
            .chain(boot_shadows.iter_mut()),
    );
    lx_event!(
        "L3_BOOT_FANOUT",
        "asked reattached L3s not on current/marspot-session to move to it",
        n_asked = asked,
        n_reattached = reattached_ids.len()
    );
    // Nothing could be spawned or reattached: there is no other backend.
    if panes.is_empty() {
        lx_error!("core.boot.no_sessions", "no sessions could be created — exiting");
        return;
    }

    let initial_focused_idx = boot_window.as_ref()
        .map(|s| (s.focused_idx as usize).min(panes.len().saturating_sub(1)))
        .unwrap_or(0);
    let mut app = CoreApp {
        renderer,
        mouse_report_press: None,
        mouse_report_last_cell: None,
        pane_badges: std::collections::HashMap::new(),
        pane_wheel_keys: std::collections::HashMap::new(),
        pane_agent_tui: std::collections::HashMap::new(),
        pane_wheel_enter_at: std::collections::HashMap::new(),
        pane_wheel_open: std::collections::HashMap::new(),
        wheel_sel_path: None,
        pane_titles: std::collections::HashMap::new(),
        pane_cwds: std::collections::HashMap::new(),
        pending_to_shell: Vec::new(),
        last_focus_notified: std::collections::HashMap::new(),
        terminal_focus: None,
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
        event_tx: event_tx.clone(),
        drag_window: None,
        pane_drag: None,
        scrollbar_drag: None,
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
        dev_drag: std::env::var("MARSPOT_DEV_DRAG")
            .ok()
            .and_then(|v| DevDrag::parse(&v))
            .map(|d| (d, Instant::now() + Duration::from_secs(3))),
        dev_panels: PanelScript::from_env(),
        pane_control: pane_control::PaneControl::from_env(),
        strips: helper_strip::Strips::from_env(),
        layout_discarded: false,
        shadows: boot_shadows,
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
    std::thread::spawn(move || shell_reader::reader_loop(reader_stream, reader_tx));

    // SIGUSR2 → per-session silent-update trigger (swaps idle L3 panes).
    install_swap_trigger(event_tx.clone());

    // The thread that paints is the thread the user is waiting on.
    // Measured on an idle bench host with background load as the only
    // variable: at default QoS a frame's p99 went from 293 µs (idle)
    // to 6,082 µs (load 11), while the GPU's own account of the same
    // frame never moved (111 → 115 µs).  Nothing got heavier; this
    // thread simply stopped being scheduled.  Raising it to
    // USER_INTERACTIVE, same machine and load, seconds apart, both
    // orderings: p99 325 µs — the tail is gone.  See `marspot::qos`.
    let role_ok = marspot::qos::take_application_role();
    let qos_ok = marspot::qos::raise_current_thread_to_user_interactive();
    lx_event!(
        "CORE_LOOP",
        "entering event loop (event-driven, no fixed cadence)",
        app_role = role_ok as u32,
        qos_user_interactive = qos_ok as u32
    );
    // Self-reporting stalls.  A blocked iteration here freezes *every*
    // pane, so the threshold is tighter than L3's: this loop is
    // supposed to turn a frame around, and anything past ~80 ms has
    // already cost the user a visible hitch.  Silent below it.
    let mut watch = marspot_term::loop_watch::LoopWatch::new(
        L2_LOOP_STALL_THRESHOLD,
    );
    // A live report: `end()` cannot speak until the iteration completes.
    watch.spawn_watchdog(|ev| match ev {
        marspot_term::loop_watch::Watch::Stalling { elapsed, phase } => lx_warn!(
            "l2.loop.stalling",
            "main loop iteration STILL running — every pane is frozen right now",
            phase = phase,
            elapsed_ms = elapsed.as_millis()
        ),
        marspot_term::loop_watch::Watch::Over { phase, trace } => lx_warn!(
            "l2.loop.stall_trace",
            &trace.summary(),
            phase = phase
        ),
    });

    let wake = event_tx.clone();
    theme_follow::watch(move || drop(wake.send(CoreEvent::L3Ready)));
    let wake = event_tx.clone();
    app.renderer.set_frame_done(move || drop(wake.send(CoreEvent::L3Ready)));
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
            // deferred frame.  The deadline is the soonest across the
            // dirty windows — one window's cap must not delay another's.
            // A window with a frame on the GPU sets no deadline: the
            // frame's completion is an event (`set_frame_done`), and
            // until it arrives the window cannot draw anyway.  With no
            // frame owed the wait is `idle_wait`: no deadline at all
            // unless some work is pending on a clock.
            let recv_timeout = app
                .windows
                .iter()
                .filter(|w| w.needs_render && !w.render.frame_in_flight() && !frames::waits(w))
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
                })
                .min()
                .or_else(|| app.idle_wait());
            let recv_timeout = if app.any_prediction_pending() || app.dev_panels.is_some() {
                Some(recv_timeout.map_or(Duration::from_millis(10), |t| t.min(Duration::from_millis(10))))
            } else {
                recv_timeout
            };
            if recv_timeout.is_none() {
                trace::flush();
            }
            match recv_timeout.map_or_else(|| event_rx.recv().map_err(|_| RecvTimeoutError::Disconnected), |t| event_rx.recv_timeout(t)) {
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
        // screenshotted: a script has no accessibility permission to
        // click the toolbar with.  Unset in the installed app.
        app.dev_drive_open_settings();
        // Dev seam (`MARSPOT_DEV_DRAG=split-right|new-window`): one pane
        // drag through the same handlers the mouse drives.  A script
        // cannot move the pointer in the sandbox app, and RFC-006's
        // endings were otherwise pinned only by unit tests that build
        // their own windows.  Unset in the installed app.
        app.dev_drive_drag();
        app.dev_drive_panels();
        app.pane_control_tick();
        app.strips_tick();
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
        //   3) re-read every pane's shell cwd so the title-strip
        //      placeholder follows `cd` (see `idle`).
        app.sweep_pane_cwds_when_due();
        app.follow_theme();
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
                    trace::mark(Stage::CoreKeyHandled, 0);
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
                    for wi in 0..app.windows.len() {
                        if !focused {
                            app.clear_hover(wi);
                        }
                        app.windows[wi].needs_render = true;
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
                // the shell's, under the id of the connection it came on
                CoreEvent::L3Says(sid, frame) if frame.msg_type == MsgType::PaneAwake => {
                    if let Ok(said) = marspot::shell_proto::decode_pane_awake(&frame.payload) {
                        let said = marspot::shell_proto::PaneAwake { session_id: sid, ..said };
                        app.pending_to_shell.push((MsgType::PaneAwake, marspot::shell_proto::encode_pane_awake(&said)));
                    }
                }
                CoreEvent::L3Says(sid, frame) => app.session_says(sid, &frame),
                CoreEvent::ShellPaneControl(f) => drop(app.route_pane_control(f)),
                CoreEvent::PaneReport(sid, f) => app.report_to_shell(sid, f),
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
                    let tx = app.event_tx.clone();
                    let Some(pane) = app.pane_by_sid_mut(sid) else { return };
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
                            pane.session_mut().swap_l3_control(new_control, selection_rx);
                            std::thread::spawn(move || {
                                l3_reader_loop(rh, sid, tx, selection_tx);
                            });
                            lx_event!(
                                "L3_CONTROL_RECONNECTED",
                                "L2 reader reconnected after EOF",
                                session = sid
                            );
                        }
                        Err(e) => lx_warn!(
                            "core.l3.control_clone_failed",
                            &format!("{e}"),
                            session = sid
                        ),
                    }
                }
                CoreEvent::L3SpawnFinished(sid, outcome) => app.adopt_spawn_outcome(sid, outcome),
                CoreEvent::PaneBell(sid, queued) => app.pane_bell(sid, &queued, Instant::now()),
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
                    app.pane_control.caps_after_hello();
                }
                CoreEvent::Ping(nonce) => {
                    to_ack.push((MsgType::Pong, encode_pong(nonce)));
                }
                CoreEvent::FramePace => {
                    frames::pace();
                    to_ack.push((MsgType::FramePaceAck, Vec::new()));
                }
                CoreEvent::FrameShown(window, surface) => frames::shown(app, window, surface),
                CoreEvent::IconMask(symbol, point_px, side, mask) => {
                    if marspot::ui::system::macos::icons::deliver(symbol, point_px, side, mask) {
                        for w in app.windows.iter_mut() {
                            w.needs_render = true;
                        }
                    } else {
                        lx_warn!(
                            "core.icon.refused",
                            "an icon from the shell that was not asked for",
                            symbol = symbol,
                            point_px = point_px,
                            side = side
                        );
                    }
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
                    if let Some((wi, _)) = app.find_window_pane_by_sid(sid) {
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
                CoreEvent::PaneAgentTui(sid, on) => {
                    // Re-issued every tick; only a change is worth a
                    // repaint.
                    if app.pane_agent_tui.get(&sid) != Some(&on) {
                        app.pane_agent_tui.insert(sid, on);
                        for w in app.windows.iter_mut() {
                            w.needs_render = true;
                        }
                    }
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
        app.pane_control.flush(&control_writer);
        for (symbol, point_px) in marspot::ui::system::macos::icons::take_wanted() {
            let payload = marspot::shell_proto::encode_icon_mask_wanted(symbol, point_px);
            send_to_shell(&control_writer, Frame::new(MsgType::IconMaskWanted, payload), "icon_mask_wanted");
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
            // its last frame is not yet off the screen: this one waits
            if frames::waits(&win!(app, wi)) {
                win!(app, wi).needs_render = true;
                continue;
            }
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
            frames::announce_attached(&mut app, wi, &control_writer);
        }
        watch.phase("pump");
        let pump_t0 = if trace::enabled() { trace::now_ns() } else { 0 };
        let pumped = app.pump_all();
        if pumped > 0 {
            trace::mark_at(Stage::PumpBegin, 0, pump_t0);
            trace::mark(Stage::PumpEnd, 0);
            app.note_output();
            bytes_pumped_total = bytes_pumped_total.saturating_add(pumped as u64);
            last_progress = Instant::now();
        }
        if app.all_exited {
            lx_event!(
                "CORE_EXIT",
                "every pane's session is gone and none ended; exiting for a new core to resurrect them",
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
            if win!(app, wi).render.frame_in_flight() || frames::waits(&win!(app, wi)) {
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
            trace::mark(Stage::RenderBegin, wi as u32);
            let caret = app.render(wi, &tex);
            frames::trace_committed(wi, app.renderer_split());
            watch.phase("post-render");
            // The frame is on the GPU, not finished.  Hold the caret
            // and let the poll below flip and announce it — the shell
            // must still only ever be pointed at a settled surface.
            win!(app, wi).pending_caret = caret;
            continue;
        }

        // icons the frames just built had no mask for: asked for now,
        // not at the next wake, which may be a while off
        for (symbol, point_px) in marspot::ui::system::macos::icons::take_wanted() {
            let payload = marspot::shell_proto::encode_icon_mask_wanted(symbol, point_px);
            send_to_shell(&control_writer, Frame::new(MsgType::IconMaskWanted, payload), "icon_mask_wanted");
        }

        frames::announce_settled(&mut app, &control_writer, frame);

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

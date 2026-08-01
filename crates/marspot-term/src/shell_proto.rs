//! Shell ↔ core wire protocol.
//!
//! Two channels carry traffic between the marspot-shell outer process
//! and the marspot-core renderer:
//!
//! 1. **IOSurface** (one-way, GPU memory) — the core writes pixels,
//!    the shell reads pixels.  No protocol; just a shared kernel
//!    object reached by ID (see `iosurface.rs`).
//! 2. **Control socket** (bidirectional, `socketpair(AF_UNIX, SOCK_STREAM)`)
//!    — keyboard / mouse / focus / resize events flow shell → core,
//!    status / health flows core → shell.  This module defines that
//!    wire format.
//!
//! Frame layout mirrors `shelld_proto`:
//!
//! ```text
//! [magic   : u32 LE  = b"MSPC"     ]  // marspot shell↔core
//! [msg_type: u32 LE                 ]
//! [len     : u32 LE  (= payload len)]
//! [payload : len bytes              ]
//! ```
//!
//! Little-endian throughout (Apple Silicon native, no byte-swap).
//! Magic ≠ `MSPS` so a misrouted shelld frame trips magic check
//! cleanly instead of being misinterpreted.
//!
//! Versioning is a `Hello`/`HelloAck` handshake carrying a u32; the
//! peers refuse to talk if they disagree.  Adding a new `MsgType`
//! with a fresh code is backward compatible; changing an existing
//! payload layout is a major bump.

use std::io::{self, Read, Write};

// ───────────────────────────────────────────────────────────────────
// Environment variables — used during the brief window between
// fork+exec and the control socket coming online.  The shell sets
// these before `Command::spawn`; the core reads them from `std::env`
// in its `main`.
// ───────────────────────────────────────────────────────────────────

/// Globally-unique IOSurface ID the core should look up.  `u32`
/// decimal.  PROTO_VERSION=2: this is the FRONT surface (the one
/// shell starts out sampling).  Core renders to the BACK surface
/// first, then acks `SurfaceReady(back_id)` so shell flips its
/// front to that — and so on, ping-ponging each frame.
pub const ENV_SURFACE_ID: &str = "MARSPOT_SHELL_SURFACE_ID";

/// PROTO_VERSION=2: second of the dual-buffer IOSurface pair.
/// `u32` decimal.  Core attaches both at boot, alternates writes
/// between them, and acks SurfaceReady(id) per frame so shell
/// knows which is currently safe to sample.  Closes the cross-
/// process IOSurface read/write race that single-buffer hit at
/// every Clear→Draw boundary on the writer side.
pub const ENV_SURFACE_ID_BACK: &str = "MARSPOT_SHELL_SURFACE_ID_BACK";

/// Surface width in **physical pixels** at attach time.  Decimal
/// `usize`.
pub const ENV_SURFACE_WIDTH: &str = "MARSPOT_SHELL_SURFACE_W";

/// Surface height in **physical pixels** at attach time.  Decimal
/// `usize`.
pub const ENV_SURFACE_HEIGHT: &str = "MARSPOT_SHELL_SURFACE_H";

/// Backing scale factor of the shell window's screen (1.0, 2.0, …).
pub const ENV_SURFACE_SCALE: &str = "MARSPOT_SHELL_SURFACE_SCALE";

/// File-descriptor number where the core inherits the control-socket
/// peer end.  Defaults to `3` (the first non-stdio fd) so we don't
/// stomp on stdin/stdout/stderr.  Decimal `i32`.
pub const ENV_CONTROL_FD: &str = "MARSPOT_SHELL_CONTROL_FD";
pub const DEFAULT_CONTROL_FD: i32 = 3;

// ───────────────────────────────────────────────────────────────────
// Wire constants
// ───────────────────────────────────────────────────────────────────

pub const MAGIC: u32 = u32::from_le_bytes(*b"MSPC");
/// Wire protocol version.  Both sides Hello-handshake on this value;
/// mismatch causes the shell to kill its core and respawn.  Bumped to
/// 2 on 2026-06-15 with the double-IOSurface (front+back) handshake
/// that closes the cross-process IOSurface read/write race
/// (`SurfaceAttach` frame, plus `SurfaceReady` per frame).  v=1 used
/// a single IOSurface and the `Resize` frame to attach it.
pub const PROTO_VERSION: u32 = 2;

/// RFC-005 — the window a frame is about.
///
/// Deliberately NOT a `PROTO_VERSION` bump.  The shell kills a core
/// whose `HelloAck` version differs from its own, so bumping the
/// version is itself the hazardous act: a core-only update would put
/// a v3 shell against a v2 core and respawn-loop it.  Instead the
/// window id rides along in ways that are invisible to a peer that
/// does not know about it:
///
/// * input frames **append** the id.  Every one of those decoders
///   already reads `payload.len() < N` and ignores a longer tail, so
///   an old reader keeps working and a new reader falls back to
///   `FIRST_WINDOW_ID` when the tail is absent.
/// * `SurfaceAttach` could not append — its decoder demands exactly
///   32 bytes — so the window-aware form is a **new msg type**, which
///   old readers silently skip (`Frame::read_from`'s forward-compat
///   rule).  The legacy frame keeps being sent for the first window.
///
/// Both directions therefore stay lossless across any skew.
pub const FIRST_WINDOW_ID: u32 = 1;

/// Read a trailing window id, or `FIRST_WINDOW_ID` when the writer
/// predates RFC-005.  `at` is the offset the id would start at.
fn trailing_window_id(payload: &[u8], at: usize) -> u32 {
    match payload.get(at..at + 4) {
        Some(b) => u32::from_le_bytes(b.try_into().unwrap()),
        None => FIRST_WINDOW_ID,
    }
}
pub const HEADER_LEN: usize = 12;
/// Sanity ceiling.  Input frames are tiny (≤256 B); a generous cap
/// rules out runaway allocations from corrupted lengths.
pub const MAX_PAYLOAD_LEN: usize = 64 * 1024;

#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MsgType {
    // ── lifecycle (1..=9) ──
    Hello = 1,
    HelloAck = 2,
    /// shell → core: liveness probe.  Payload = u32 nonce; core
    /// echoes it back in `Pong` so the shell can match request/reply
    /// and ignore stale Pongs after a respawn.
    Ping = 3,
    /// core → shell: response to `Ping`.  Payload = u32 nonce.
    Pong = 4,
    // ── input (10..=29) ──
    KeyEvent = 10,
    MouseDown = 11,
    MouseDrag = 12,
    MouseUp = 13,
    Scroll = 14,
    Preedit = 15,
    /// L2 → L3: clipboard text the user pasted (Cmd-V).  The GUI-free L3
    /// can't read the macOS pasteboard, so L2 resolves it and forwards the
    /// already-decoded text; L3 wraps it in bracketed-paste markers (if its
    /// terminal enabled the mode) and writes it to the PTY.
    Paste = 16,
    /// shell → core: bare mouse-move (no button) at physical-pixel
    /// `(x f64, y f64)` + zero modifier byte.  High-frequency but
    /// cheap — L2 hit-tests against chrome rects to track hover
    /// affordances (icon button BG darkens under cursor) and
    /// requests a redraw only when the hover region changes.
    MouseMove = 17,
    /// shell → core: right-mouse-down at physical-pixel `(x f64, y f64)`
    /// + modifier byte.  Same encoding as `MouseDown`.  Drives the
    /// F3+9 right-click context menu in L2; missing on pre-F3+9
    /// images, which is wire-safe per `feedback_frame_forward_compat`
    /// (older receivers silently skip the unknown msg_type, older
    /// senders simply never produce it).
    MouseRightDown = 18,
    // ── window state (30..=49) ──
    Focus = 30,
    Resize = 31,
    /// core → shell: "I have rendered the first frame to the new
    /// IOSurface; you may swap the presenter now."  Payload is the
    /// surface ID the core just confirmed (u32 LE).  The shell
    /// ignores frames whose ID doesn't match its currently-pending
    /// surface — see Step 4's resize state machine.
    SurfaceReady = 32,
    /// core → shell: focused-pane caret rect in view-local physical
    /// pixels (top-left origin), or "no caret".  The shell feeds it
    /// to `MarspotAppCtx::set_caret_rect_phys` so the IME candidate
    /// window anchors under the caret even though the composition
    /// state lives in the core process.  Payload: u8 present flag +
    /// 4 × f64 LE (x, y, w, h) when present.
    CaretRect = 33,
    /// L3 (`marspot-session`) → L2 (`marspot-core`): "I published a new
    /// grid snapshot into shared memory; come read it."  Empty payload —
    /// a pure wake so L2 stays event-driven (idle CPU ~0) instead of
    /// polling the shm seq every frame.  L2 coalesces a burst of these
    /// into a single re-read + render.  See `docs/per-session-l3.md`.
    GridReady = 34,
    /// L2 (`marspot-core`) → L3 (`marspot-session`): "resize your session
    /// to these cell dims."  Payload: `cols: u16 LE, rows: u16 LE`.  L3
    /// resizes its Terminal + ioctl's the PTY (via shelld) + reflows, then
    /// republishes at the new dims.  Distinct from the shell↔core
    /// `Resize` (31), which carries an IOSurface id + pixel dims — this
    /// one is the pure cell-grid resize the L3 framebuffer needs.
    GridResize = 35,
    /// L2 (`marspot-core`) → L3 (`marspot-session`): "publish the window at
    /// this scrollback view offset (rows up from the live tail)."  Payload:
    /// `view_offset: u16 LE`.  L3 owns the scrollback, so L2 can't scroll
    /// its own mirror (which holds only the visible window) — it asks L3
    /// which window to publish.  See `docs/per-session-l3.md`.
    GridScroll = 36,
    /// L2 → L3: "give me the clipboard text under this selection."  L3
    /// owns the grid + scrollback (L2's mirror is window-only), so Cmd-C
    /// on an L3 pane round-trips through here.  Payload: `anchor(col u16,
    /// abs u32), focus(col u16, abs u32), blockwise u8` — the abs coords
    /// match `Grid::cell_at_view`.  L3 replies with `SelectionText`.
    GetSelectionText = 37,
    /// L3 → L2: reply to `GetSelectionText`.  Payload: `len u32 LE` + UTF-8
    /// bytes (empty len = empty selection).
    SelectionText = 38,
    /// core → shell: "I just rendered a complete frame into the IOSurface;
    /// present it now."  Empty payload — a pure wake so the shell presents
    /// on real frame events instead of a blind ~60 fps timer (which burned
    /// idle CPU and occasionally sampled the surface mid-render → a flicker).
    /// Deprecated at PROTO_VERSION=2: `SurfaceReady` carries both the
    /// "go present" wake AND the "this id is now safe to read" id, so
    /// `FrameRendered` is redundant on the dual-IOSurface path.
    FrameRendered = 39,
    /// shell → core: "I have allocated two IOSurfaces.  Render into
    /// them alternately; after each `commit + waitUntilCompleted`,
    /// ack with `SurfaceReady(id)` so I know that id is safe to
    /// sample."  Payload (32 bytes LE):
    ///   front_id u32
    ///   back_id  u32
    ///   w_phys   f64
    ///   h_phys   f64
    ///   scale    f64
    /// `front_id` is the surface shell wants core to write FIRST
    /// (becomes "front" after the first SurfaceReady ack); `back_id`
    /// is the other one.  Sent at boot (replacing the v=1 single-
    /// surface env-var-only handshake) and on every resize.
    /// Introduced at PROTO_VERSION=2 — supersedes the single-surface
    /// `Resize` frame on the v=2 path.
    SurfaceAttach = 40,
    /// shell → core: "decorate the pane backing this shelld session
    /// with this badge text on the right side of its title strip."
    /// Payload: `session_id u64 LE, badge_len u16 LE, badge_utf8`.
    /// Empty badge = "clear it".  L2 stores per-shelld-session and
    /// composes the title strip; gone when the pane is destroyed.
    /// Used by the L1 claudecode plugin to surface the bound
    /// claudecode sessionId next to the pane label.
    PaneBadge = 41,
    /// core → shell: user clicked the active part of a pane badge
    /// (e.g. the `P<n>` profile tag).  Payload: `session_id u64 LE`.
    /// The shell hands this to the L1 plugin owning the badge so the
    /// plugin can react (cycle profile, open menu, etc.) — L2 has no
    /// idea what the badge means.
    PaneBadgeClicked = 42,
    /// shell → core: "an L1 plugin is taking over this pane for a
    /// while; honour these capability bits — freeze the grid, swallow
    /// keystrokes (forward them back as PaneSessionKey instead),
    /// allow overlays."  Payload: `session_id u64 LE, caps u32 LE`.
    /// Subsequent PaneBadge / PaneSessionOverlay frames address the
    /// session by sid until the matching PaneSessionEnd lands.
    PaneSessionBegin = 43,
    /// shell → core: pane session over — restore live grid + key
    /// forwarding for `session_id u64 LE`.
    PaneSessionEnd = 44,
    /// core → shell: a key event arrived while the pane was in a
    /// LOCK_KEYS session.  Payload: `session_id u64 LE, wire_key_event…`.
    /// Plugin handler decides: swallow / forward / end-session.
    PaneSessionKey = 45,
    /// core → shell: user pressed Esc three times in five seconds
    /// while a pane session held the keyboard.  Hard exit hatch:
    /// shell unconditionally ends the session (plugin gets on_end).
    /// Payload: `session_id u64 LE`.
    PaneSessionUserEscape = 46,
    /// shell → core: paint a plugin-controlled region over part of
    /// the pane.  Payload: `session_id u64 LE, region(4×f32 LE),
    /// body_len u32 LE, body_utf8`.  Body shape is plugin-defined
    /// (renderer just blits the text for now); future iterations
    /// will carry attributed runs.  Stub frame in C1 — full overlay
    /// rendering lands when a plugin actually uses it.
    PaneSessionOverlay = 47,
    /// L1 → L2: cc plugin asks L2 to push these raw bytes into the
    /// PTY backing this session.  Payload: u64 LE session_id +
    /// u32 LE byte_len + raw bytes.  No bracketed-paste wrap; the
    /// caller's bytes hit the PTY verbatim.  L2 forwards to L3 via
    /// its existing control socket using a sibling InjectInput
    /// frame; L3's main loop writes the bytes straight to the PTY.
    InjectInput = 48,
    // ── search (50..=53) — pane upgrade B2; see
    //    docs/scrollback-search.md §5
    /// L2 → L3: "start (or restart) a substring search on the
    /// focused pane's scrollback + live grid".  Payload:
    ///   query_id        u32 LE   — monotonic on L2; L3 stamps it
    ///                              back on every SearchResults so
    ///                              stale results get dropped
    ///   case_sensitive  u8       — 0 / 1
    ///   max_total       u32 LE   — first-batch cap; SearchMore for
    ///                              older
    ///   query_byte_len  u32 LE   — utf-8 length follows
    ///   query           utf-8 bytes
    /// Receiving a new SearchScrollback with a different query_id
    /// last-write-wins: cancel the in-flight worker and spawn fresh.
    SearchScrollback = 50,
    /// L3 → L2: search result batch.  Streams in chunks of up to
    /// `max_total`; final batch carries `has_more = 0`.  Payload:
    ///   query_id        u32 LE
    ///   has_more        u8       — 0/1
    ///   total_seen      u32 LE   — running count of hits seen by
    ///                              this worker
    ///   hit_count       u32 LE
    ///   for each hit:
    ///     logical_line_idx        u64 LE
    ///     char_offset             u32 LE
    ///     char_len                u32 LE
    ///     snippet_match_start     u16 LE
    ///     snippet_match_end       u16 LE
    ///     snippet_byte_len        u32 LE
    ///     snippet                 utf-8 bytes
    ///     phys_span_count         u16 LE
    ///     for each span:
    ///       phys_row_idx     u64 LE
    ///       col_start        u16 LE
    ///       col_end_inclusive u16 LE
    SearchResults = 51,
    /// L2 → L3: request more results past the last yielded batch.
    /// Payload:
    ///   query_id  u32 LE
    ///   count     u32 LE
    ///   direction u8     — 0=older(more history),
    ///                      1=newer(live grid + post-init scrollback);
    ///                      v1 only direction=0 is used
    SearchMore = 52,
    /// L2 → L3: cancel an in-flight search.  Payload: query_id u32 LE.
    SearchCancel = 53,
    // 54 was `PaneCwd` (L3→L2 OSC 7 push) — retired in F3+3.6 after
    // the modal switched to a pull-based `proc_pidinfo` lookup.  The
    // discriminant is left as a hole so a future L3 (briefly on the
    // old binary across an install) sending a PaneCwd frame just
    // falls into the unknown-msg-type silent-skip path.
    /// L2 → L1: user clicked the toolbar's dev-panel toggle icon.
    /// Empty payload.  L1 owns the dev panel's NSWindow (built in
    /// `run_app`), L2 just routes the click — L2 doesn't keep its
    /// own visibility state, L1 is the single source of truth.
    DevPanelToggle = 55,
    /// L1 plugin → L2: set a pane's title text(insert into the title
    /// resolution chain ABOVE cwd basename, BELOW user-set custom title
    /// + edit buffer).Payload mirrors `PaneBadge`:
    ///   `session_id u64 LE, title_len u16 LE, title_utf8`.
    /// Empty `title_len` clears the plugin-set title for that pane.
    /// Used by the claudecode plugin to project-name the pane on bind.
    PaneTitle = 56,
    /// L1 → L2: user dropped files from Finder onto the terminal
    /// view.  L1 owns the NSView (only it sees NSDraggingDestination
    /// callbacks); L2 hit-tests the drop point against the pane
    /// layout, shell-escapes each path, and forwards the joined text
    /// into that pane's PTY via the existing `Paste` path (so
    /// bracketed-paste apps like claudecode see it as a paste).
    /// Payload: `x f64 LE, y f64 LE` (drop point, physical px,
    /// top-left origin — same convention as `MouseDown`), `count u16
    /// LE`, then per path `len u16 LE + UTF-8 bytes`.
    FileDrop = 57,
    /// core → shell: user right-clicked the active prefix of a pane
    /// badge.  L2 owns the hit-test (it knows badge geometry); the
    /// menu CONTENT is L1 plugin state, so this asks "what should the
    /// menu say?".  Request/response pair with `PaneBadgeMenu`, same
    /// shape as `GetSelectionText`/`SelectionText`.  Payload:
    /// `session_id u64 LE, anchor_x f64 LE, anchor_y f64 LE`
    /// (physical px, top-left origin — echoed back so the reply is
    /// stateless).
    PaneBadgeMenuRequest = 58,
    /// shell → core: the badge context-menu items for a prior
    /// `PaneBadgeMenuRequest`.  L2 opens its ContextMenu at the echoed
    /// anchor.  An empty item list means "no menu" — L2 shows nothing.
    /// Payload: `session_id u64 LE, anchor_x f64 LE, anchor_y f64 LE,
    /// count u8`, then per item `tag u32 LE, label_len u16 LE,
    /// label_utf8`.
    PaneBadgeMenu = 59,
    /// core → shell: user picked an item from a `PaneBadgeMenu`.  The
    /// tag is the plugin-assigned opaque id from the menu frame.
    /// Payload: `session_id u64 LE, tag u32 LE`.
    PaneBadgeMenuAction = 60,
    // ── windows (61..=63) — RFC-005 ──
    /// Window-aware `SurfaceAttach`; carries a trailing `window_id`.
    /// A frame naming an unseen window IS that window's birth event.
    SurfaceAttachWindow = 61,
    /// A window went away; the core drops its `WindowState`.
    WindowClosed = 62,
    /// Which window is key.  Keyboard and IME follow it.
    WindowFocus = 63,
    /// core → shell: reopen a window the last session had.  Sent once
    /// per saved window past the boot one, at boot.  Payload:
    /// `frame_index u32 LE` — which entry of `window-state.bin` the
    /// shell should restore the geometry from.
    ///
    /// The core asks rather than announces because L1 owns windows
    /// and allocates their ids; the resulting `SurfaceAttachWindow`
    /// is still the window's birth event, exactly as for Cmd-N.
    WindowOpenRequest = 64,
    /// core → shell: close this window (RFC-005 step 5 — the last
    /// pane was moved out and an empty window closes itself).  L1
    /// owns windows, so the core asks.  Payload: `window_id u32 LE`.
    WindowCloseRequest = 65,
    /// core → shell: the user's focus moved to this pane.  Payload:
    /// `session_id u64 LE`.
    ///
    /// L1 plugins get it as an event.  It exists because "the user is
    /// looking at this pane again" is the earliest honest moment to
    /// start restoring something that was reclaimed while idle —
    /// waiting for a keystroke means the restore starts after they
    /// have already tried to use it, which reads as "it takes
    /// forever".
    PaneFocused = 66,
    /// shell → core: how far this pane has receded from active use.
    /// Payload: `session_id u64 LE, level u32 LE`.
    ///
    /// The levels track a session's own lifecycle, not a stopwatch:
    ///   0 — live.  Includes a shell sitting at its prompt: a terminal
    ///       waiting for you is not idle, and an earlier version that
    ///       dimmed anything quiet for five minutes marked 15 of 18
    ///       panes on this desktop, which is the same as marking none.
    ///   1 — resting: a bound program finished its turn and has been
    ///       waiting long enough to be a reclamation candidate.  The
    ///       dim is the warning that it is drifting out.
    ///   2 — parked: the program was reclaimed; the picture is frozen
    ///       and a keystroke (or focus) brings it back.
    ///
    /// How dark each level looks is the renderer's ladder — it also
    /// depends on whether this is the pane the user is in, which L1
    /// does not know.
    PaneRecede = 67,
    /// L1 → L2 → L3: hold this pane's grid where it is.
    ///
    /// Unlike `PANE_SESSION_CAP_FREEZE_GRID`, which only stops L2 from
    /// pumping its own copy, this stops **L3** from feeding the PTY
    /// into its terminal at all — so the held picture survives an L2
    /// restart, which every silent update performs.
    PaneHoldGrid = 68,
    /// L1 → L2: deliver text to a pane the way a paste would.
    ///
    /// Distinct from `InjectInput`, which writes bytes verbatim: only
    /// L3 knows whether the program in the pane has bracketed paste
    /// on, and multi-line text delivered without it runs line by line
    /// as commands.  Anything that hands a *message* to whatever is
    /// running in a pane wants this one.
    PaneInjectPaste = 69,
    /// CLI → L1: deliver text to a pane named by the caller.
    ///
    /// The first thing that types into a pane on behalf of something
    /// that is not the user sitting in front of it.
    CliSendText = 70,
    /// L1 → CLI: `ok u8, message`.
    CliResult = 71,
    /// CLI → L1: what panes are there?  Addressing a pane by name is
    /// only usable if the names can be looked up.
    CliListPanes = 72,
    /// L1 → CLI: the panes, as L1 sees them.
    CliPaneList = 73,
    // ── error (200..=255) ──
    Error = 200,
}

impl MsgType {
    pub fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            1 => MsgType::Hello,
            2 => MsgType::HelloAck,
            3 => MsgType::Ping,
            4 => MsgType::Pong,
            10 => MsgType::KeyEvent,
            11 => MsgType::MouseDown,
            12 => MsgType::MouseDrag,
            13 => MsgType::MouseUp,
            14 => MsgType::Scroll,
            15 => MsgType::Preedit,
            16 => MsgType::Paste,
            17 => MsgType::MouseMove,
            18 => MsgType::MouseRightDown,
            30 => MsgType::Focus,
            31 => MsgType::Resize,
            32 => MsgType::SurfaceReady,
            33 => MsgType::CaretRect,
            34 => MsgType::GridReady,
            35 => MsgType::GridResize,
            36 => MsgType::GridScroll,
            37 => MsgType::GetSelectionText,
            38 => MsgType::SelectionText,
            39 => MsgType::FrameRendered,
            40 => MsgType::SurfaceAttach,
            41 => MsgType::PaneBadge,
            42 => MsgType::PaneBadgeClicked,
            43 => MsgType::PaneSessionBegin,
            44 => MsgType::PaneSessionEnd,
            45 => MsgType::PaneSessionKey,
            46 => MsgType::PaneSessionUserEscape,
            47 => MsgType::PaneSessionOverlay,
            48 => MsgType::InjectInput,
            50 => MsgType::SearchScrollback,
            51 => MsgType::SearchResults,
            52 => MsgType::SearchMore,
            53 => MsgType::SearchCancel,
            55 => MsgType::DevPanelToggle,
            56 => MsgType::PaneTitle,
            57 => MsgType::FileDrop,
            58 => MsgType::PaneBadgeMenuRequest,
            59 => MsgType::PaneBadgeMenu,
            60 => MsgType::PaneBadgeMenuAction,
            61 => MsgType::SurfaceAttachWindow,
            62 => MsgType::WindowClosed,
            63 => MsgType::WindowFocus,
            64 => MsgType::WindowOpenRequest,
            65 => MsgType::WindowCloseRequest,
            66 => MsgType::PaneFocused,
            67 => MsgType::PaneRecede,
            68 => MsgType::PaneHoldGrid,
            69 => MsgType::PaneInjectPaste,
            70 => MsgType::CliSendText,
            71 => MsgType::CliResult,
            72 => MsgType::CliListPanes,
            73 => MsgType::CliPaneList,
            200 => MsgType::Error,
            _ => return None,
        })
    }
}

/// Encode an `InjectInput` payload: u64 LE session_id + u32 LE len +
/// raw bytes.  No bracketed-paste markup — the caller controls every
/// byte that ends up on the PTY.
pub fn encode_inject_input(session_id: u64, bytes: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(12 + bytes.len());
    v.extend_from_slice(&session_id.to_le_bytes());
    v.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    v.extend_from_slice(bytes);
    v
}

pub fn decode_inject_input(payload: &[u8]) -> io::Result<(u64, Vec<u8>)> {
    if payload.len() < 12 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "InjectInput too short"));
    }
    let sid = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let len = u32::from_le_bytes(payload[8..12].try_into().unwrap()) as usize;
    if payload.len() != 12 + len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("InjectInput len {len} but payload total {}", payload.len()),
        ));
    }
    Ok((sid, payload[12..].to_vec()))
}

// ─── B2: search wire encoders / decoders ───────────────────────────
// Hand-rolled per D13 — no new bincode dep.  Lengths are bounded by
// the (existing) Frame layer's 32-bit payload size, well above any
// realistic search payload.

pub fn encode_search_scrollback(
    query_id: u32,
    case_sensitive: bool,
    max_total: u32,
    query: &str,
) -> Vec<u8> {
    let q = query.as_bytes();
    let mut v = Vec::with_capacity(4 + 1 + 4 + 4 + q.len());
    v.extend_from_slice(&query_id.to_le_bytes());
    v.push(case_sensitive as u8);
    v.extend_from_slice(&max_total.to_le_bytes());
    v.extend_from_slice(&(q.len() as u32).to_le_bytes());
    v.extend_from_slice(q);
    v
}

pub fn decode_search_scrollback(payload: &[u8]) -> io::Result<(u32, bool, u32, String)> {
    if payload.len() < 4 + 1 + 4 + 4 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "SearchScrollback header truncated"));
    }
    let qid = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    let case = payload[4] != 0;
    let max_total = u32::from_le_bytes(payload[5..9].try_into().unwrap());
    let q_len = u32::from_le_bytes(payload[9..13].try_into().unwrap()) as usize;
    if payload.len() != 13 + q_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("SearchScrollback q_len {q_len} but payload total {}", payload.len()),
        ));
    }
    let query = std::str::from_utf8(&payload[13..])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "SearchScrollback query not utf-8"))?
        .to_string();
    Ok((qid, case, max_total, query))
}

/// Wire shape mirrors `scrollback_search::SearchHit` field-for-field
/// in the order documented in §5.3.  Kept as a flat struct on the
/// wire side so encode/decode are trivial loops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireSearchHit {
    pub logical_line_idx: u64,
    pub char_offset: u32,
    pub char_len: u32,
    pub snippet_match_start: u16,
    pub snippet_match_end: u16,
    pub snippet: String,
    pub spans: Vec<WirePhysicalSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WirePhysicalSpan {
    pub phys_row_idx: u64,
    pub col_start: u16,
    pub col_end_inclusive: u16,
}

pub fn encode_search_results(
    query_id: u32,
    has_more: bool,
    total_seen: u32,
    hits: &[WireSearchHit],
) -> Vec<u8> {
    // Generous reservation; final size grows with snippets/spans.
    let mut v = Vec::with_capacity(4 + 1 + 4 + 4 + hits.len() * 64);
    v.extend_from_slice(&query_id.to_le_bytes());
    v.push(has_more as u8);
    v.extend_from_slice(&total_seen.to_le_bytes());
    v.extend_from_slice(&(hits.len() as u32).to_le_bytes());
    for h in hits {
        v.extend_from_slice(&h.logical_line_idx.to_le_bytes());
        v.extend_from_slice(&h.char_offset.to_le_bytes());
        v.extend_from_slice(&h.char_len.to_le_bytes());
        v.extend_from_slice(&h.snippet_match_start.to_le_bytes());
        v.extend_from_slice(&h.snippet_match_end.to_le_bytes());
        let snip = h.snippet.as_bytes();
        v.extend_from_slice(&(snip.len() as u32).to_le_bytes());
        v.extend_from_slice(snip);
        v.extend_from_slice(&(h.spans.len() as u16).to_le_bytes());
        for s in &h.spans {
            v.extend_from_slice(&s.phys_row_idx.to_le_bytes());
            v.extend_from_slice(&s.col_start.to_le_bytes());
            v.extend_from_slice(&s.col_end_inclusive.to_le_bytes());
        }
    }
    v
}

pub fn decode_search_results(
    payload: &[u8],
) -> io::Result<(u32, bool, u32, Vec<WireSearchHit>)> {
    let mut p = 0;
    fn need<'a>(payload: &'a [u8], p: usize, n: usize, what: &str) -> io::Result<&'a [u8]> {
        if p + n > payload.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("SearchResults truncated at {what}"),
            ));
        }
        Ok(&payload[p..p + n])
    }
    let qid = u32::from_le_bytes(need(payload, p, 4, "qid")?.try_into().unwrap());
    p += 4;
    let has_more = need(payload, p, 1, "has_more")?[0] != 0;
    p += 1;
    let total_seen = u32::from_le_bytes(need(payload, p, 4, "total_seen")?.try_into().unwrap());
    p += 4;
    let hit_count = u32::from_le_bytes(need(payload, p, 4, "hit_count")?.try_into().unwrap()) as usize;
    p += 4;
    let mut hits = Vec::with_capacity(hit_count);
    for _ in 0..hit_count {
        let logical_line_idx = u64::from_le_bytes(need(payload, p, 8, "lli")?.try_into().unwrap());
        p += 8;
        let char_offset = u32::from_le_bytes(need(payload, p, 4, "char_offset")?.try_into().unwrap());
        p += 4;
        let char_len = u32::from_le_bytes(need(payload, p, 4, "char_len")?.try_into().unwrap());
        p += 4;
        let snippet_match_start = u16::from_le_bytes(need(payload, p, 2, "sm_start")?.try_into().unwrap());
        p += 2;
        let snippet_match_end = u16::from_le_bytes(need(payload, p, 2, "sm_end")?.try_into().unwrap());
        p += 2;
        let snippet_byte_len = u32::from_le_bytes(need(payload, p, 4, "snip_len")?.try_into().unwrap()) as usize;
        p += 4;
        let snippet = std::str::from_utf8(need(payload, p, snippet_byte_len, "snippet")?)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "SearchResults snippet not utf-8"))?
            .to_string();
        p += snippet_byte_len;
        let span_count = u16::from_le_bytes(need(payload, p, 2, "span_count")?.try_into().unwrap()) as usize;
        p += 2;
        let mut spans = Vec::with_capacity(span_count);
        for _ in 0..span_count {
            let phys_row_idx = u64::from_le_bytes(need(payload, p, 8, "phys")?.try_into().unwrap());
            p += 8;
            let col_start = u16::from_le_bytes(need(payload, p, 2, "col_start")?.try_into().unwrap());
            p += 2;
            let col_end_inclusive = u16::from_le_bytes(need(payload, p, 2, "col_end")?.try_into().unwrap());
            p += 2;
            spans.push(WirePhysicalSpan { phys_row_idx, col_start, col_end_inclusive });
        }
        hits.push(WireSearchHit {
            logical_line_idx,
            char_offset,
            char_len,
            snippet,
            snippet_match_start,
            snippet_match_end,
            spans,
        });
    }
    if p != payload.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("SearchResults trailing bytes: parsed {p} of {}", payload.len()),
        ));
    }
    Ok((qid, has_more, total_seen, hits))
}

pub fn encode_search_more(query_id: u32, count: u32, direction: u8) -> Vec<u8> {
    let mut v = Vec::with_capacity(9);
    v.extend_from_slice(&query_id.to_le_bytes());
    v.extend_from_slice(&count.to_le_bytes());
    v.push(direction);
    v
}

pub fn decode_search_more(payload: &[u8]) -> io::Result<(u32, u32, u8)> {
    if payload.len() != 9 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("SearchMore wrong length: {}", payload.len()),
        ));
    }
    let qid = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    let count = u32::from_le_bytes(payload[4..8].try_into().unwrap());
    let direction = payload[8];
    Ok((qid, count, direction))
}

pub fn encode_search_cancel(query_id: u32) -> Vec<u8> {
    query_id.to_le_bytes().to_vec()
}

pub fn decode_search_cancel(payload: &[u8]) -> io::Result<u32> {
    if payload.len() != 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("SearchCancel wrong length: {}", payload.len()),
        ));
    }
    Ok(u32::from_le_bytes(payload[0..4].try_into().unwrap()))
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub msg_type: MsgType,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(msg_type: MsgType, payload: Vec<u8>) -> Self {
        Self { msg_type, payload }
    }

    /// Bytes this frame occupies on the wire, header included.  Used
    /// to size queues that hold frames before they reach a socket.
    pub fn wire_len(&self) -> usize {
        HEADER_LEN + self.payload.len()
    }

    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<usize> {
        let mut header = [0u8; HEADER_LEN];
        header[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        header[4..8].copy_from_slice(&(self.msg_type as u32).to_le_bytes());
        header[8..12].copy_from_slice(&(self.payload.len() as u32).to_le_bytes());
        w.write_all(&header)?;
        if !self.payload.is_empty() {
            w.write_all(&self.payload)?;
        }
        Ok(HEADER_LEN + self.payload.len())
    }

    /// Read one frame off `r`.  Forward-compatible: a frame whose
    /// `msg_type` this build doesn't recognise (e.g. a new variant
    /// added in a future release) is **silently skipped** — its
    /// payload is consumed and the loop reads the next frame.  Magic /
    /// length-cap / IO errors are still hard errors.
    ///
    /// This makes peer additions of new message types safe across
    /// version skew: the old reader stays alive instead of erroring
    /// and tearing down the control channel.  See RFC-003 §6
    /// Amendment 14: that property is the foundation of the L3
    /// silent self-update — a future L2 sending `RequestSelfUpdate`
    /// at an L3 that lacks the variant must not be lethal.
    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Option<Self>> {
        loop {
            let mut header = [0u8; HEADER_LEN];
            match read_exact_or_eof(r, &mut header)? {
                ReadEnd::Eof => return Ok(None),
                ReadEnd::Full => {}
            }
            let magic = u32::from_le_bytes(header[0..4].try_into().unwrap());
            if magic != MAGIC {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bad magic 0x{:08x}, expected 0x{:08x}", magic, MAGIC),
                ));
            }
            let type_raw = u32::from_le_bytes(header[4..8].try_into().unwrap());
            let len = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
            if len > MAX_PAYLOAD_LEN {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("payload len {} exceeds cap {}", len, MAX_PAYLOAD_LEN),
                ));
            }
            let mut payload = vec![0u8; len];
            if len > 0 {
                r.read_exact(&mut payload)?;
            }
            match MsgType::from_u32(type_raw) {
                Some(msg_type) => return Ok(Some(Frame { msg_type, payload })),
                None => {
                    // Forward-compat: drop unknown frames on the floor
                    // and keep reading.  The control channel survives a
                    // future peer that speaks a wider protocol.
                    continue;
                }
            }
        }
    }
}

enum ReadEnd {
    Full,
    Eof,
}

fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<ReadEnd> {
    let mut off = 0;
    while off < buf.len() {
        match r.read(&mut buf[off..]) {
            Ok(0) => {
                if off == 0 {
                    return Ok(ReadEnd::Eof);
                }
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF mid-frame"));
            }
            Ok(n) => off += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(ReadEnd::Full)
}

// ───────────────────────────────────────────────────────────────────
// Payload codecs — one encode/decode pair per MsgType.
// All multibyte integers little-endian.
// ───────────────────────────────────────────────────────────────────

/// HELLO payload: protocol version the sender speaks (u32).
pub fn encode_hello(version: u32) -> Vec<u8> {
    version.to_le_bytes().to_vec()
}
pub fn decode_hello(payload: &[u8]) -> io::Result<u32> {
    if payload.len() != 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "HELLO payload != 4 bytes",
        ));
    }
    Ok(u32::from_le_bytes(payload[0..4].try_into().unwrap()))
}

/// HELLO_ACK: same shape as HELLO — version the receiver agrees on
/// (echoed back, so the sender knows both sides are aligned).
pub fn encode_hello_ack(version: u32) -> Vec<u8> {
    version.to_le_bytes().to_vec()
}
pub fn decode_hello_ack(payload: &[u8]) -> io::Result<u32> {
    decode_hello(payload)
}

/// PING / PONG payload: u32 LE nonce.  Shell rotates the nonce so it
/// can ignore Pongs from a previous Ping that arrived after a timeout.
pub fn encode_ping(nonce: u32) -> Vec<u8> {
    nonce.to_le_bytes().to_vec()
}
pub fn decode_ping(payload: &[u8]) -> io::Result<u32> {
    if payload.len() != 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PING payload != 4 bytes",
        ));
    }
    Ok(u32::from_le_bytes(payload[0..4].try_into().unwrap()))
}
pub fn encode_pong(nonce: u32) -> Vec<u8> {
    nonce.to_le_bytes().to_vec()
}
pub fn decode_pong(payload: &[u8]) -> io::Result<u32> {
    decode_ping(payload)
}

// ── KeyEvent ──
//
// Layout:
//   state    : u8   (0 = Pressed, 1 = Released)
//   mods     : u8   (bit 0 shift, 1 ctrl, 2 alt, 3 super)
//   kind     : u8   (0 = Char, 1 = Named, 2 = Other)
//   pad      : u8   (reserved, 0)
//   key_data : u32  Char → codepoint;  Named → discriminant in low byte
//   text_len : u16
//   text     : text_len bytes UTF-8

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WireKeyState {
    Pressed = 0,
    Released = 1,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WireLogicalKind {
    Char = 0,
    Named = 1,
    Other = 2,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum WireNamedKey {
    Enter = 0,
    Backspace = 1,
    Tab = 2,
    Escape = 3,
    ArrowUp = 4,
    ArrowDown = 5,
    ArrowLeft = 6,
    ArrowRight = 7,
    PageUp = 8,
    PageDown = 9,
    Home = 10,
    End = 11,
    Insert = 12,
    Delete = 13,
    F1 = 14,
    F2 = 15,
    F3 = 16,
    F4 = 17,
    F5 = 18,
    F6 = 19,
    F7 = 20,
    F8 = 21,
    F9 = 22,
    F10 = 23,
    F11 = 24,
    F12 = 25,
}

impl WireNamedKey {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Enter,
            1 => Self::Backspace,
            2 => Self::Tab,
            3 => Self::Escape,
            4 => Self::ArrowUp,
            5 => Self::ArrowDown,
            6 => Self::ArrowLeft,
            7 => Self::ArrowRight,
            8 => Self::PageUp,
            9 => Self::PageDown,
            10 => Self::Home,
            11 => Self::End,
            12 => Self::Insert,
            13 => Self::Delete,
            14 => Self::F1,
            15 => Self::F2,
            16 => Self::F3,
            17 => Self::F4,
            18 => Self::F5,
            19 => Self::F6,
            20 => Self::F7,
            21 => Self::F8,
            22 => Self::F9,
            23 => Self::F10,
            24 => Self::F11,
            25 => Self::F12,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct WireKeyEvent {
    pub state: WireKeyState,
    pub mods: u8,
    pub kind: WireLogicalKind,
    pub key_data: u32,
    pub text: String,
}

pub fn mods_to_byte(shift: bool, ctrl: bool, alt: bool, super_: bool) -> u8 {
    (shift as u8) | ((ctrl as u8) << 1) | ((alt as u8) << 2) | ((super_ as u8) << 3)
}

pub fn mods_from_byte(b: u8) -> (bool, bool, bool, bool) {
    (
        b & 0b0001 != 0,
        b & 0b0010 != 0,
        b & 0b0100 != 0,
        b & 0b1000 != 0,
    )
}

pub fn encode_key_event(ev: &WireKeyEvent, window_id: u32) -> Vec<u8> {
    let text_bytes = ev.text.as_bytes();
    let mut out = Vec::with_capacity(10 + text_bytes.len() + 4);
    out.push(ev.state as u8);
    out.push(ev.mods);
    out.push(ev.kind as u8);
    out.push(0); // pad
    out.extend_from_slice(&ev.key_data.to_le_bytes());
    out.extend_from_slice(&(text_bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(text_bytes);
    out.extend_from_slice(&window_id.to_le_bytes());
    out
}

pub fn decode_key_event(payload: &[u8]) -> io::Result<(WireKeyEvent, u32)> {
    if payload.len() < 10 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("KeyEvent payload < 10 bytes (got {})", payload.len()),
        ));
    }
    let state = match payload[0] {
        0 => WireKeyState::Pressed,
        1 => WireKeyState::Released,
        v => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("KeyEvent bad state {v}"),
            ))
        }
    };
    let mods = payload[1];
    let kind = match payload[2] {
        0 => WireLogicalKind::Char,
        1 => WireLogicalKind::Named,
        2 => WireLogicalKind::Other,
        v => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("KeyEvent bad kind {v}"),
            ))
        }
    };
    let key_data = u32::from_le_bytes(payload[4..8].try_into().unwrap());
    let text_len = u16::from_le_bytes(payload[8..10].try_into().unwrap()) as usize;
    if payload.len() < 10 + text_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "KeyEvent payload truncated",
        ));
    }
    let text = std::str::from_utf8(&payload[10..10 + text_len])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
        .to_string();
    Ok((
        WireKeyEvent {
            state,
            mods,
            kind,
            key_data,
            text,
        },
        trailing_window_id(payload, 10 + text_len),
    ))
}

// ── Mouse / Scroll ──

pub fn encode_mouse(x: f64, y: f64, mods: u8, window_id: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(21);
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    out.push(mods);
    out.extend_from_slice(&window_id.to_le_bytes());
    out
}

pub fn decode_mouse(payload: &[u8]) -> io::Result<(f64, f64, u8, u32)> {
    if payload.len() < 17 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "mouse payload < 17 bytes",
        ));
    }
    let x = f64::from_le_bytes(payload[0..8].try_into().unwrap());
    let y = f64::from_le_bytes(payload[8..16].try_into().unwrap());
    let mods = payload[16];
    Ok((x, y, mods, trailing_window_id(payload, 17)))
}

/// `MouseDrag` grows a hover tail: which marspot window the pointer
/// is over RIGHT NOW plus the pointer in THAT window's physical
/// coordinates.  RFC-006's live drop preview is drawn from it.  Old
/// readers stop at their own tail; missing tail = hover 0 = no
/// preview (the drag still resolves on release).
pub fn encode_mouse_drag(
    x: f64,
    y: f64,
    mods: u8,
    window_id: u32,
    hover_window_id: u32,
    hover_x: f64,
    hover_y: f64,
) -> Vec<u8> {
    let mut out = encode_mouse(x, y, mods, window_id);
    out.extend_from_slice(&hover_window_id.to_le_bytes());
    out.extend_from_slice(&hover_x.to_le_bytes());
    out.extend_from_slice(&hover_y.to_le_bytes());
    out
}

/// `(x, y, mods, window_id, hover_window_id, hover_x, hover_y)`.
pub fn decode_mouse_drag(payload: &[u8]) -> io::Result<(f64, f64, u8, u32, u32, f64, f64)> {
    let (x, y, mods, win) = decode_mouse(payload)?;
    if payload.len() >= 41 {
        let hw = u32::from_le_bytes(payload[21..25].try_into().unwrap());
        let hx = f64::from_le_bytes(payload[25..33].try_into().unwrap());
        let hy = f64::from_le_bytes(payload[33..41].try_into().unwrap());
        Ok((x, y, mods, win, hw, hx, hy))
    } else {
        Ok((x, y, mods, win, 0, 0.0, 0.0))
    }
}

/// `MouseUp` grows a second trailing id: the marspot window under the
/// pointer at release time (0 = none).  RFC-005 step 5's drag-a-pane-
/// between-windows needs it — AppKit keeps delivering the drag to the
/// window that took the press, so only L1 (which owns the NSWindows)
/// can say where the pointer actually ended up.  An old reader stops
/// at its own tail and never sees it; a new reader missing the tail
/// (old shell) reads 0 = "no drop target", which cancels the drag —
/// the safe end of the deal.
/// …and, since RFC-006, the release point: physical coords in the
/// drop window when `drop_window_id ≠ 0`, SCREEN POINTS when it is 0
/// (the only case that needs screen space — placing the new window a
/// drag-to-desktop births).
pub fn encode_mouse_up(
    x: f64,
    y: f64,
    mods: u8,
    window_id: u32,
    drop_window_id: u32,
    drop_x: f64,
    drop_y: f64,
) -> Vec<u8> {
    let mut out = encode_mouse(x, y, mods, window_id);
    out.extend_from_slice(&drop_window_id.to_le_bytes());
    out.extend_from_slice(&drop_x.to_le_bytes());
    out.extend_from_slice(&drop_y.to_le_bytes());
    out
}

/// `(x, y, mods, window_id, drop_window_id, drop_x, drop_y)`.
pub fn decode_mouse_up(payload: &[u8]) -> io::Result<(f64, f64, u8, u32, u32, f64, f64)> {
    let (x, y, mods, win) = decode_mouse(payload)?;
    let drop = if payload.len() >= 25 {
        u32::from_le_bytes(payload[21..25].try_into().unwrap())
    } else {
        0
    };
    let (dx, dy) = if payload.len() >= 41 {
        (
            f64::from_le_bytes(payload[25..33].try_into().unwrap()),
            f64::from_le_bytes(payload[33..41].try_into().unwrap()),
        )
    } else {
        (0.0, 0.0)
    };
    Ok((x, y, mods, win, drop, dx, dy))
}

pub fn encode_scroll(dx: f64, dy: f64, precise: bool, window_id: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(21);
    out.extend_from_slice(&dx.to_le_bytes());
    out.extend_from_slice(&dy.to_le_bytes());
    out.push(if precise { 1 } else { 0 });
    out.extend_from_slice(&window_id.to_le_bytes());
    out
}

pub fn decode_scroll(payload: &[u8]) -> io::Result<(f64, f64, bool, u32)> {
    if payload.len() < 17 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "scroll payload < 17 bytes",
        ));
    }
    let dx = f64::from_le_bytes(payload[0..8].try_into().unwrap());
    let dy = f64::from_le_bytes(payload[8..16].try_into().unwrap());
    let precise = payload[16] != 0;
    Ok((dx, dy, precise, trailing_window_id(payload, 17)))
}

// ── Focus / Resize / Preedit ──

pub fn encode_focus(focused: bool) -> Vec<u8> {
    vec![if focused { 1 } else { 0 }]
}
pub fn decode_focus(payload: &[u8]) -> io::Result<bool> {
    if payload.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "focus payload empty",
        ));
    }
    Ok(payload[0] != 0)
}

/// Resize payload layout:
///
/// ```text
/// [new_surface_id : u32 LE]
/// [w_phys         : f64 LE]
/// [h_phys         : f64 LE]
/// [scale          : f64 LE]
/// ```
///
/// The shell creates a fresh IOSurface at the new dimensions, then
/// sends this frame.  The core looks the surface up, rebuilds its
/// render target and layout, and acks with `SurfaceReady(id)`.
/// SURFACE_ATTACH payload (32 bytes LE):
///   front_id u32, back_id u32, w_phys f64, h_phys f64, scale f64.
/// Introduced at PROTO_VERSION=2 for double-buffer IOSurface.
pub fn encode_surface_attach(
    front_id: u32,
    back_id: u32,
    w_phys: f64,
    h_phys: f64,
    scale: f64,
) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + 4 + 8 + 8 + 8);
    v.extend_from_slice(&front_id.to_le_bytes());
    v.extend_from_slice(&back_id.to_le_bytes());
    v.extend_from_slice(&w_phys.to_le_bytes());
    v.extend_from_slice(&h_phys.to_le_bytes());
    v.extend_from_slice(&scale.to_le_bytes());
    v
}

pub fn decode_surface_attach(
    payload: &[u8],
) -> io::Result<(u32, u32, f64, f64, f64)> {
    if payload.len() != 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SURFACE_ATTACH payload != 32 bytes",
        ));
    }
    Ok((
        u32::from_le_bytes(payload[0..4].try_into().unwrap()),
        u32::from_le_bytes(payload[4..8].try_into().unwrap()),
        f64::from_le_bytes(payload[8..16].try_into().unwrap()),
        f64::from_le_bytes(payload[16..24].try_into().unwrap()),
        f64::from_le_bytes(payload[24..32].try_into().unwrap()),
    ))
}

/// Window-aware `SurfaceAttach` (RFC-005): the legacy 32-byte body
/// plus a trailing `window_id`.
///
/// A separate msg type rather than a longer `SurfaceAttach` because
/// that decoder demands *exactly* 32 bytes — appending would hard-fail
/// on every core already installed.  An old core skips this type
/// silently and keeps driving off the legacy frame, so the shell sends
/// both for the first window.
///
/// A frame naming a `window_id` the core has not seen is that
/// window's birth event; there is no separate "create window" message.
pub fn encode_surface_attach_window(
    front_id: u32,
    back_id: u32,
    w_phys: f64,
    h_phys: f64,
    scale: f64,
    window_id: u32,
) -> Vec<u8> {
    let mut v = encode_surface_attach(front_id, back_id, w_phys, h_phys, scale);
    v.extend_from_slice(&window_id.to_le_bytes());
    v
}

pub fn decode_surface_attach_window(
    payload: &[u8],
) -> io::Result<(u32, u32, f64, f64, f64, u32)> {
    if payload.len() < 36 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SURFACE_ATTACH_WINDOW payload < 36 bytes",
        ));
    }
    let (front, back, w, h, scale) = decode_surface_attach(&payload[..32])?;
    Ok((front, back, w, h, scale, trailing_window_id(payload, 32)))
}

/// A window closed.  The core drops that `WindowState` — and with it
/// the panes it held, which are closed the same way the sidebar's
/// `[x]` closes one.
pub fn encode_window_closed(window_id: u32) -> Vec<u8> {
    window_id.to_le_bytes().to_vec()
}

pub fn decode_window_closed(payload: &[u8]) -> io::Result<u32> {
    if payload.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "WINDOW_CLOSED payload < 4 bytes",
        ));
    }
    Ok(u32::from_le_bytes(payload[0..4].try_into().unwrap()))
}

/// Which window became key.  Keyboard and IME follow this rather than
/// tagging every keystroke: they are by definition delivered to the
/// key window, and the frames share one ordered socket.
pub fn encode_window_focus(window_id: u32) -> Vec<u8> {
    window_id.to_le_bytes().to_vec()
}

pub fn decode_window_focus(payload: &[u8]) -> io::Result<u32> {
    if payload.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "WINDOW_FOCUS payload < 4 bytes",
        ));
    }
    Ok(u32::from_le_bytes(payload[0..4].try_into().unwrap()))
}

/// Ask L1 to reopen a saved window, restoring its geometry from entry
/// `frame_index` of `window-state.bin`.  An index past the end of that
/// file is not an error — the window opens at the default rect.
/// `WindowOpenRequest` with this frame index is a USER action (move a
/// pane to a brand-new window), not a boot restore: L1 opens it at
/// the default rect and does NOT apply the crash-loop / generation
/// gates that guard automatic restores.
pub const WINDOW_OPEN_USER: u32 = u32::MAX;

pub fn encode_window_close_request(window_id: u32) -> Vec<u8> {
    window_id.to_le_bytes().to_vec()
}

pub fn decode_window_close_request(payload: &[u8]) -> io::Result<u32> {
    if payload.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "WINDOW_CLOSE_REQUEST payload < 4 bytes",
        ));
    }
    Ok(u32::from_le_bytes(payload[0..4].try_into().unwrap()))
}

pub fn encode_window_open_request(frame_index: u32) -> Vec<u8> {
    frame_index.to_le_bytes().to_vec()
}

/// `WINDOW_OPEN_USER` request carrying a placement hint: the window
/// opens centred on `(x, y)` screen points (drag-to-desktop births a
/// window where the pane was dropped).  Old readers see only the
/// leading frame_index.
pub fn encode_window_open_request_at(frame_index: u32, x: f64, y: f64) -> Vec<u8> {
    let mut out = frame_index.to_le_bytes().to_vec();
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    out
}

/// `(frame_index, hint)` — hint is `None` when the tail is absent or
/// zero (no meaningful screen point is ever exactly (0, 0) for a
/// centred window; the boot restore path never sends one).
pub fn decode_window_open_request_at(payload: &[u8]) -> io::Result<(u32, Option<(f64, f64)>)> {
    let idx = decode_window_open_request(payload)?;
    if payload.len() >= 20 {
        let x = f64::from_le_bytes(payload[4..12].try_into().unwrap());
        let y = f64::from_le_bytes(payload[12..20].try_into().unwrap());
        if x != 0.0 || y != 0.0 {
            return Ok((idx, Some((x, y))));
        }
    }
    Ok((idx, None))
}

pub fn decode_window_open_request(payload: &[u8]) -> io::Result<u32> {
    if payload.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "WINDOW_OPEN_REQUEST payload < 4 bytes",
        ));
    }
    Ok(u32::from_le_bytes(payload[0..4].try_into().unwrap()))
}

pub fn encode_resize(new_surface_id: u32, w_phys: f64, h_phys: f64, scale: f64) -> Vec<u8> {
    let mut out = Vec::with_capacity(28);
    out.extend_from_slice(&new_surface_id.to_le_bytes());
    out.extend_from_slice(&w_phys.to_le_bytes());
    out.extend_from_slice(&h_phys.to_le_bytes());
    out.extend_from_slice(&scale.to_le_bytes());
    out
}

pub fn decode_resize(payload: &[u8]) -> io::Result<(u32, f64, f64, f64)> {
    if payload.len() < 28 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "resize payload < 28 bytes",
        ));
    }
    let id = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    let w = f64::from_le_bytes(payload[4..12].try_into().unwrap());
    let h = f64::from_le_bytes(payload[12..20].try_into().unwrap());
    let s = f64::from_le_bytes(payload[20..28].try_into().unwrap());
    Ok((id, w, h, s))
}

/// PaneBadge payload: `session_id u64 LE, badge_len u16 LE, badge_utf8`.
/// Empty `badge_len` means "clear any badge currently set for this
/// session".  Caps `badge_len` at 64 bytes — a sane upper bound for
/// the right-edge decoration the title strip can fit; longer payloads
/// are rejected as malformed rather than truncated.
pub const PANE_BADGE_MAX_LEN: u16 = 64;

pub fn encode_pane_badge(session_id: u64, badge: &str) -> Vec<u8> {
    let bytes = badge.as_bytes();
    let n = bytes.len().min(PANE_BADGE_MAX_LEN as usize);
    let mut out = Vec::with_capacity(8 + 2 + n);
    out.extend_from_slice(&session_id.to_le_bytes());
    out.extend_from_slice(&(n as u16).to_le_bytes());
    out.extend_from_slice(&bytes[..n]);
    out
}

pub fn decode_pane_badge(payload: &[u8]) -> io::Result<(u64, String)> {
    if payload.len() < 10 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pane_badge payload < 10 bytes",
        ));
    }
    let session_id = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let n = u16::from_le_bytes(payload[8..10].try_into().unwrap()) as usize;
    if n > PANE_BADGE_MAX_LEN as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("pane_badge len {} > cap {}", n, PANE_BADGE_MAX_LEN),
        ));
    }
    if payload.len() < 10 + n {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pane_badge payload truncated before body",
        ));
    }
    let badge = String::from_utf8_lossy(&payload[10..10 + n]).into_owned();
    Ok((session_id, badge))
}

/// PaneTitle payload mirrors PaneBadge — `session_id u64 LE,
/// title_len u16 LE, title_utf8`.  Empty `title_len` clears the
/// plugin-set title for that session.
pub const PANE_TITLE_MAX_LEN: u16 = 128;

pub fn encode_pane_title(session_id: u64, title: &str) -> Vec<u8> {
    let bytes = title.as_bytes();
    let n = bytes.len().min(PANE_TITLE_MAX_LEN as usize);
    let mut out = Vec::with_capacity(8 + 2 + n);
    out.extend_from_slice(&session_id.to_le_bytes());
    out.extend_from_slice(&(n as u16).to_le_bytes());
    out.extend_from_slice(&bytes[..n]);
    out
}

pub fn decode_pane_title(payload: &[u8]) -> io::Result<(u64, String)> {
    if payload.len() < 10 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pane_title payload < 10 bytes",
        ));
    }
    let session_id = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let n = u16::from_le_bytes(payload[8..10].try_into().unwrap()) as usize;
    if n > PANE_TITLE_MAX_LEN as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("pane_title len {} > cap {}", n, PANE_TITLE_MAX_LEN),
        ));
    }
    if payload.len() < 10 + n {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pane_title payload truncated before body",
        ));
    }
    let title = String::from_utf8_lossy(&payload[10..10 + n]).into_owned();
    Ok((session_id, title))
}

/// PaneRecede payload: `session_id u64 LE, level u32 LE`.
pub fn encode_pane_recede(session_id: u64, level: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(12);
    v.extend_from_slice(&session_id.to_le_bytes());
    v.extend_from_slice(&level.to_le_bytes());
    v
}

pub fn decode_pane_recede(payload: &[u8]) -> io::Result<(u64, u32)> {
    if payload.len() < 12 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PaneRecede payload too short",
        ));
    }
    let sid = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let level = u32::from_le_bytes(payload[8..12].try_into().unwrap());
    Ok((sid, level))
}

/// PaneHoldGrid payload: `session_id u64 LE, on u8` (1 = hold).
pub fn encode_pane_hold_grid(session_id: u64, on: bool) -> Vec<u8> {
    let mut v = Vec::with_capacity(9);
    v.extend_from_slice(&session_id.to_le_bytes());
    v.push(on as u8);
    v
}

pub fn decode_pane_hold_grid(payload: &[u8]) -> io::Result<(u64, bool)> {
    if payload.len() < 9 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PaneHoldGrid payload too short",
        ));
    }
    let sid = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    Ok((sid, payload[8] != 0))
}

/// PaneInjectPaste payload: `session_id u64 LE, len u32 LE, utf8`.
pub fn encode_pane_inject_paste(session_id: u64, text: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(12 + text.len());
    v.extend_from_slice(&session_id.to_le_bytes());
    v.extend_from_slice(&(text.len() as u32).to_le_bytes());
    v.extend_from_slice(text.as_bytes());
    v
}

pub fn decode_pane_inject_paste(payload: &[u8]) -> io::Result<(u64, String)> {
    if payload.len() < 12 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PaneInjectPaste payload too short",
        ));
    }
    let sid = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let n = u32::from_le_bytes(payload[8..12].try_into().unwrap()) as usize;
    if payload.len() < 12 + n {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PaneInjectPaste payload truncated before body",
        ));
    }
    Ok((sid, String::from_utf8_lossy(&payload[12..12 + n]).into_owned()))
}

/// CliSendText payload: `target len u32 + utf8, text len u32 + utf8`.
///
/// The target is a name, not an id: whoever runs the CLI knows the
/// project it means ("spg"), not the session number L1 gave it.
pub fn encode_cli_send_text(target: &str, text: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + target.len() + text.len());
    v.extend_from_slice(&(target.len() as u32).to_le_bytes());
    v.extend_from_slice(target.as_bytes());
    v.extend_from_slice(&(text.len() as u32).to_le_bytes());
    v.extend_from_slice(text.as_bytes());
    v
}

pub fn decode_cli_send_text(payload: &[u8]) -> io::Result<(String, String)> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "CliSendText payload truncated");
    if payload.len() < 4 {
        return Err(bad());
    }
    let t = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
    if payload.len() < 8 + t {
        return Err(bad());
    }
    let target = String::from_utf8_lossy(&payload[4..4 + t]).into_owned();
    let n = u32::from_le_bytes(payload[4 + t..8 + t].try_into().unwrap()) as usize;
    if payload.len() < 8 + t + n {
        return Err(bad());
    }
    let text = String::from_utf8_lossy(&payload[8 + t..8 + t + n]).into_owned();
    Ok((target, text))
}

/// CliResult payload: `ok u8, message len u32 + utf8`.
pub fn encode_cli_result(ok: bool, message: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + message.len());
    v.push(ok as u8);
    v.extend_from_slice(&(message.len() as u32).to_le_bytes());
    v.extend_from_slice(message.as_bytes());
    v
}

pub fn decode_cli_result(payload: &[u8]) -> io::Result<(bool, String)> {
    if payload.len() < 5 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "CliResult payload too short",
        ));
    }
    let n = u32::from_le_bytes(payload[1..5].try_into().unwrap()) as usize;
    if payload.len() < 5 + n {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "CliResult payload truncated before body",
        ));
    }
    Ok((payload[0] != 0, String::from_utf8_lossy(&payload[5..5 + n]).into_owned()))
}

/// CliPaneList payload: `count u32`, then per pane
/// `sid u64, cwd len u32 + utf8, title len u32 + utf8`.
///
/// The title field carries the pane's *address* rather than its window
/// title: what a listing is for is telling a caller how to name this
/// pane again, and the window title is already the directory.
pub fn encode_cli_pane_list(panes: &[(u64, String, String)]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + panes.len() * 64);
    v.extend_from_slice(&(panes.len() as u32).to_le_bytes());
    for (sid, cwd, title) in panes {
        v.extend_from_slice(&sid.to_le_bytes());
        v.extend_from_slice(&(cwd.len() as u32).to_le_bytes());
        v.extend_from_slice(cwd.as_bytes());
        v.extend_from_slice(&(title.len() as u32).to_le_bytes());
        v.extend_from_slice(title.as_bytes());
    }
    v
}

pub fn decode_cli_pane_list(payload: &[u8]) -> io::Result<Vec<(u64, String, String)>> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "CliPaneList payload truncated");
    if payload.len() < 4 {
        return Err(bad());
    }
    let n = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
    let mut at = 4;
    let mut out = Vec::with_capacity(n);
    let mut take_str = |at: &mut usize| -> io::Result<String> {
        if payload.len() < *at + 4 {
            return Err(bad());
        }
        let len = u32::from_le_bytes(payload[*at..*at + 4].try_into().unwrap()) as usize;
        *at += 4;
        if payload.len() < *at + len {
            return Err(bad());
        }
        let s = String::from_utf8_lossy(&payload[*at..*at + len]).into_owned();
        *at += len;
        Ok(s)
    };
    for _ in 0..n {
        if payload.len() < at + 8 {
            return Err(bad());
        }
        let sid = u64::from_le_bytes(payload[at..at + 8].try_into().unwrap());
        at += 8;
        let cwd = take_str(&mut at)?;
        let title = take_str(&mut at)?;
        out.push((sid, cwd, title));
    }
    Ok(out)
}

/// PaneFocused payload: `session_id u64 LE`.
pub fn encode_pane_focused(session_id: u64) -> Vec<u8> {
    session_id.to_le_bytes().to_vec()
}

pub fn decode_pane_focused(payload: &[u8]) -> io::Result<u64> {
    decode_pane_badge_clicked(payload)
}

/// PaneBadgeClicked payload: `session_id u64 LE`.  Carries which pane
/// (identified by its shelld session_id) the user clicked the badge
/// on; the badge text itself is L1 plugin state, so L2 only needs to
/// say "here, your turn".
pub fn encode_pane_badge_clicked(session_id: u64) -> Vec<u8> {
    session_id.to_le_bytes().to_vec()
}

pub fn decode_pane_badge_clicked(payload: &[u8]) -> io::Result<u64> {
    if payload.len() != 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pane_badge_clicked payload != 8 bytes",
        ));
    }
    Ok(u64::from_le_bytes(payload.try_into().unwrap()))
}

/// One row of a pane-badge context menu on the wire.  `tag` is the
/// plugin-assigned opaque id echoed back via `PaneBadgeMenuAction`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneBadgeMenuItem {
    pub tag: u32,
    pub label: String,
}

/// Caps for the PaneBadgeMenu frame.  Labels reuse the badge cap —
/// a menu row is the same order of magnitude as a badge; item count
/// is bounded well under the u8 the wire carries.
pub const PANE_BADGE_MENU_MAX_ITEMS: u8 = 16;
pub const PANE_BADGE_MENU_LABEL_MAX_LEN: u16 = 64;

/// PaneBadgeMenuRequest payload: `session_id u64 LE, anchor_x f64 LE,
/// anchor_y f64 LE`.
pub fn encode_pane_badge_menu_request(session_id: u64, x: f64, y: f64) -> Vec<u8> {
    let mut out = Vec::with_capacity(24);
    out.extend_from_slice(&session_id.to_le_bytes());
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    out
}

pub fn decode_pane_badge_menu_request(payload: &[u8]) -> io::Result<(u64, f64, f64)> {
    if payload.len() != 24 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pane_badge_menu_request payload != 24 bytes",
        ));
    }
    let session_id = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let x = f64::from_le_bytes(payload[8..16].try_into().unwrap());
    let y = f64::from_le_bytes(payload[16..24].try_into().unwrap());
    Ok((session_id, x, y))
}

/// PaneBadgeMenu payload: `session_id u64 LE, anchor_x f64 LE,
/// anchor_y f64 LE, count u8`, then per item `tag u32 LE,
/// label_len u16 LE, label_utf8`.
pub fn encode_pane_badge_menu(
    session_id: u64,
    x: f64,
    y: f64,
    items: &[PaneBadgeMenuItem],
) -> Vec<u8> {
    let n = items.len().min(PANE_BADGE_MENU_MAX_ITEMS as usize);
    let mut out = Vec::with_capacity(25 + n * 12);
    out.extend_from_slice(&session_id.to_le_bytes());
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    out.push(n as u8);
    for item in &items[..n] {
        let bytes = item.label.as_bytes();
        let len = bytes.len().min(PANE_BADGE_MENU_LABEL_MAX_LEN as usize);
        out.extend_from_slice(&item.tag.to_le_bytes());
        out.extend_from_slice(&(len as u16).to_le_bytes());
        out.extend_from_slice(&bytes[..len]);
    }
    out
}

pub fn decode_pane_badge_menu(
    payload: &[u8],
) -> io::Result<(u64, f64, f64, Vec<PaneBadgeMenuItem>)> {
    if payload.len() < 25 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pane_badge_menu payload < 25 bytes",
        ));
    }
    let session_id = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let x = f64::from_le_bytes(payload[8..16].try_into().unwrap());
    let y = f64::from_le_bytes(payload[16..24].try_into().unwrap());
    let count = payload[24];
    if count > PANE_BADGE_MENU_MAX_ITEMS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "pane_badge_menu count {} > cap {}",
                count, PANE_BADGE_MENU_MAX_ITEMS
            ),
        ));
    }
    let mut items = Vec::with_capacity(count as usize);
    let mut off = 25usize;
    for _ in 0..count {
        if payload.len() < off + 6 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "pane_badge_menu truncated before item header",
            ));
        }
        let tag = u32::from_le_bytes(payload[off..off + 4].try_into().unwrap());
        let len =
            u16::from_le_bytes(payload[off + 4..off + 6].try_into().unwrap()) as usize;
        if len > PANE_BADGE_MENU_LABEL_MAX_LEN as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "pane_badge_menu label len {} > cap {}",
                    len, PANE_BADGE_MENU_LABEL_MAX_LEN
                ),
            ));
        }
        off += 6;
        if payload.len() < off + len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "pane_badge_menu truncated before label body",
            ));
        }
        let label = String::from_utf8_lossy(&payload[off..off + len]).into_owned();
        off += len;
        items.push(PaneBadgeMenuItem { tag, label });
    }
    Ok((session_id, x, y, items))
}

/// PaneBadgeMenuAction payload: `session_id u64 LE, tag u32 LE`.
pub fn encode_pane_badge_menu_action(session_id: u64, tag: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    out.extend_from_slice(&session_id.to_le_bytes());
    out.extend_from_slice(&tag.to_le_bytes());
    out
}

pub fn decode_pane_badge_menu_action(payload: &[u8]) -> io::Result<(u64, u32)> {
    if payload.len() != 12 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pane_badge_menu_action payload != 12 bytes",
        ));
    }
    let session_id = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let tag = u32::from_le_bytes(payload[8..12].try_into().unwrap());
    Ok((session_id, tag))
}

/// PaneSession capability bits — packed into the u32 carried by
/// `PaneSessionBegin`.  Each capability gates one host-mediated
/// behaviour the plugin can ask for.
pub const PANE_SESSION_CAP_INPUT: u32 = 1 << 0;
pub const PANE_SESSION_CAP_LOCK_KEYS: u32 = 1 << 1;
pub const PANE_SESSION_CAP_FREEZE_GRID: u32 = 1 << 2;
pub const PANE_SESSION_CAP_OBSERVE_PTY: u32 = 1 << 3;
pub const PANE_SESSION_CAP_OVERLAY: u32 = 1 << 4;

pub fn encode_pane_session_begin(session_id: u64, caps: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(12);
    v.extend_from_slice(&session_id.to_le_bytes());
    v.extend_from_slice(&caps.to_le_bytes());
    v
}

pub fn decode_pane_session_begin(payload: &[u8]) -> io::Result<(u64, u32)> {
    if payload.len() != 12 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("pane_session_begin payload != 12 bytes (got {})", payload.len()),
        ));
    }
    let sid = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let caps = u32::from_le_bytes(payload[8..12].try_into().unwrap());
    Ok((sid, caps))
}

pub fn encode_pane_session_end(session_id: u64) -> Vec<u8> {
    session_id.to_le_bytes().to_vec()
}

pub fn decode_pane_session_end(payload: &[u8]) -> io::Result<u64> {
    if payload.len() != 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pane_session_end payload != 8 bytes",
        ));
    }
    Ok(u64::from_le_bytes(payload.try_into().unwrap()))
}

/// PaneSessionKey payload: `session_id u64 LE` + the existing
/// WireKeyEvent encoding (variable-length).  Plugin handler decodes
/// the WireKeyEvent the same way the regular KeyEvent path does.
pub fn encode_pane_session_key(session_id: u64, ev: &WireKeyEvent) -> Vec<u8> {
    // Pane sessions are addressed by session id, which is window-blind
    // — the plugin holding the pane does not care which window it is
    // drawn in — so the embedded key event carries a placeholder id
    // that the decoder drops.
    let key_payload = encode_key_event(ev, FIRST_WINDOW_ID);
    let mut v = Vec::with_capacity(8 + key_payload.len());
    v.extend_from_slice(&session_id.to_le_bytes());
    v.extend_from_slice(&key_payload);
    v
}

pub fn decode_pane_session_key(payload: &[u8]) -> io::Result<(u64, WireKeyEvent)> {
    if payload.len() < 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pane_session_key payload < 8 bytes",
        ));
    }
    let sid = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let (ev, _window_id) = decode_key_event(&payload[8..])?;
    Ok((sid, ev))
}

pub fn encode_pane_session_user_escape(session_id: u64) -> Vec<u8> {
    session_id.to_le_bytes().to_vec()
}

pub fn decode_pane_session_user_escape(payload: &[u8]) -> io::Result<u64> {
    if payload.len() != 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pane_session_user_escape payload != 8 bytes",
        ));
    }
    Ok(u64::from_le_bytes(payload.try_into().unwrap()))
}

/// PaneSessionOverlay payload (stub format — refined when the first
/// overlay renderer lands):
/// ```text
/// [session_id u64 LE]
/// [region: x f32, y_top f32, w f32, h f32  (16 bytes, phys px)]
/// [body_len u32 LE]
/// [body bytes]            — plugin-defined; renderer treats as UTF-8 for v1
/// ```
pub fn encode_pane_session_overlay(
    session_id: u64,
    region: (f32, f32, f32, f32),
    body: &[u8],
) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + 16 + 4 + body.len());
    v.extend_from_slice(&session_id.to_le_bytes());
    v.extend_from_slice(&region.0.to_le_bytes());
    v.extend_from_slice(&region.1.to_le_bytes());
    v.extend_from_slice(&region.2.to_le_bytes());
    v.extend_from_slice(&region.3.to_le_bytes());
    v.extend_from_slice(&(body.len() as u32).to_le_bytes());
    v.extend_from_slice(body);
    v
}

pub fn decode_pane_session_overlay(
    payload: &[u8],
) -> io::Result<(u64, (f32, f32, f32, f32), Vec<u8>)> {
    if payload.len() < 8 + 16 + 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pane_session_overlay header < 28 bytes",
        ));
    }
    let sid = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let rx = f32::from_le_bytes(payload[8..12].try_into().unwrap());
    let ry = f32::from_le_bytes(payload[12..16].try_into().unwrap());
    let rw = f32::from_le_bytes(payload[16..20].try_into().unwrap());
    let rh = f32::from_le_bytes(payload[20..24].try_into().unwrap());
    let body_len = u32::from_le_bytes(payload[24..28].try_into().unwrap()) as usize;
    if payload.len() < 28 + body_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pane_session_overlay body truncated",
        ));
    }
    Ok((sid, (rx, ry, rw, rh), payload[28..28 + body_len].to_vec()))
}

/// GridResize payload: `cols: u16 LE, rows: u16 LE` — the cell-grid
/// dims L2 wants this L3 session resized to (the in-view window L3
/// publishes into the shm framebuffer follows these dims).
pub fn encode_grid_resize(cols: u16, rows: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(4);
    out.extend_from_slice(&cols.to_le_bytes());
    out.extend_from_slice(&rows.to_le_bytes());
    out
}

pub fn decode_grid_resize(payload: &[u8]) -> io::Result<(u16, u16)> {
    if payload.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "grid_resize payload < 4 bytes",
        ));
    }
    let cols = u16::from_le_bytes(payload[0..2].try_into().unwrap());
    let rows = u16::from_le_bytes(payload[2..4].try_into().unwrap());
    Ok((cols, rows))
}

/// GridScroll payload: `view_offset: u16 LE` — rows up from the live
/// tail that L3 should publish into the framebuffer (0 = live).
pub fn encode_grid_scroll(view_offset: u16) -> Vec<u8> {
    view_offset.to_le_bytes().to_vec()
}

pub fn decode_grid_scroll(payload: &[u8]) -> io::Result<u16> {
    if payload.len() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "grid_scroll payload < 2 bytes",
        ));
    }
    Ok(u16::from_le_bytes(payload[0..2].try_into().unwrap()))
}

/// GetSelectionText payload: `anchor(col u16, abs u32), focus(col u16,
/// abs u32), blockwise u8`.
/// GetSelectionText payload: `seq u32 LE` (request id, echoed back in the
/// reply so a late reply from a timed-out request can't alias the next
/// copy) + anchor/focus coords + blockwise flag.
pub fn encode_get_selection_text(
    seq: u32,
    anchor: (u16, u32),
    focus: (u16, u32),
    blockwise: bool,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(17);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&anchor.0.to_le_bytes());
    out.extend_from_slice(&anchor.1.to_le_bytes());
    out.extend_from_slice(&focus.0.to_le_bytes());
    out.extend_from_slice(&focus.1.to_le_bytes());
    out.push(blockwise as u8);
    out
}

pub fn decode_get_selection_text(
    payload: &[u8],
) -> io::Result<(u32, (u16, u32), (u16, u32), bool)> {
    if payload.len() < 17 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "get_selection_text payload < 17 bytes",
        ));
    }
    let seq = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    let a_col = u16::from_le_bytes(payload[4..6].try_into().unwrap());
    let a_abs = u32::from_le_bytes(payload[6..10].try_into().unwrap());
    let f_col = u16::from_le_bytes(payload[10..12].try_into().unwrap());
    let f_abs = u32::from_le_bytes(payload[12..16].try_into().unwrap());
    let blockwise = payload[16] != 0;
    Ok((seq, (a_col, a_abs), (f_col, f_abs), blockwise))
}

/// SelectionText payload: `seq u32 LE` (echo of the request) + `len u32 LE`
/// + UTF-8 bytes.
pub fn encode_selection_text(seq: u32, text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(8 + bytes.len());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
    out
}

pub fn decode_selection_text(payload: &[u8]) -> io::Result<(u32, String)> {
    if payload.len() < 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "selection_text payload < 8 bytes",
        ));
    }
    let seq = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    let len = u32::from_le_bytes(payload[4..8].try_into().unwrap()) as usize;
    if payload.len() < 8 + len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "selection_text payload truncated",
        ));
    }
    std::str::from_utf8(&payload[8..8 + len])
        .map(|s| (seq, s.to_string()))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// SurfaceReady payload: the IOSurface ID the core has just rendered
/// to (u32 LE).  The shell uses this to switch its presenter from the
/// previous (stretched) surface to the new (crisp) one.
pub fn encode_surface_ready(surface_id: u32) -> Vec<u8> {
    surface_id.to_le_bytes().to_vec()
}

pub fn decode_surface_ready(payload: &[u8]) -> io::Result<u32> {
    if payload.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "surface_ready payload < 4 bytes",
        ));
    }
    Ok(u32::from_le_bytes(payload[0..4].try_into().unwrap()))
}

/// CaretRect payload: `[present: u8]` then, when present == 1,
/// `4 × f64 LE` (x, y, w, h) in view-local physical pixels with a
/// top-left origin — the exact tuple `set_caret_rect_phys` takes.
/// The trailing window id sits after whatever the variant carries —
/// offset 1 for `None`, offset 33 for `Some` — because it has to go
/// *after* the existing bytes to stay invisible to an old reader.
pub fn encode_caret_rect(rect: Option<(f64, f64, f64, f64)>, window_id: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(37);
    match rect {
        None => out.push(0),
        Some((x, y, w, h)) => {
            out.push(1);
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
            out.extend_from_slice(&w.to_le_bytes());
            out.extend_from_slice(&h.to_le_bytes());
        }
    }
    out.extend_from_slice(&window_id.to_le_bytes());
    out
}

pub fn decode_caret_rect(
    payload: &[u8],
) -> io::Result<(Option<(f64, f64, f64, f64)>, u32)> {
    if payload.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "caret_rect payload empty",
        ));
    }
    if payload[0] == 0 {
        return Ok((None, trailing_window_id(payload, 1)));
    }
    if payload.len() < 33 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "caret_rect payload < 33 bytes",
        ));
    }
    let f = |i: usize| f64::from_le_bytes(payload[i..i + 8].try_into().unwrap());
    Ok((
        Some((f(1), f(9), f(17), f(25))),
        trailing_window_id(payload, 33),
    ))
}

pub fn encode_preedit(text: &str, window_id: u32) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(2 + bytes.len() + 4);
    out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(bytes);
    out.extend_from_slice(&window_id.to_le_bytes());
    out
}

pub fn decode_preedit(payload: &[u8]) -> io::Result<(String, u32)> {
    if payload.len() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "preedit payload < 2 bytes",
        ));
    }
    let len = u16::from_le_bytes(payload[0..2].try_into().unwrap()) as usize;
    if payload.len() < 2 + len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "preedit payload truncated",
        ));
    }
    let text = std::str::from_utf8(&payload[2..2 + len])
        .map(|s| s.to_string())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    Ok((text, trailing_window_id(payload, 2 + len)))
}

/// `Paste` payload: `[len u16 LE][utf8 text]` — same wire shape as Preedit.
pub fn encode_paste(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(2 + bytes.len());
    out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(bytes);
    out
}

pub fn decode_paste(payload: &[u8]) -> io::Result<String> {
    if payload.len() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "paste payload < 2 bytes",
        ));
    }
    let len = u16::from_le_bytes(payload[0..2].try_into().unwrap()) as usize;
    if payload.len() < 2 + len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "paste payload truncated",
        ));
    }
    std::str::from_utf8(&payload[2..2 + len])
        .map(|s| s.to_string())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// Encode a `FileDrop` payload: drop point (physical px, top-left
/// origin) + the dropped paths.  Layout: `x f64 LE, y f64 LE,
/// count u16 LE`, then per path `len u16 LE + UTF-8 bytes`.
pub fn encode_file_drop(x: f64, y: f64, paths: &[String], window_id: u32) -> Vec<u8> {
    let body: usize = paths.iter().map(|p| 2 + p.len()).sum();
    let mut out = Vec::with_capacity(18 + body + 4);
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    out.extend_from_slice(&(paths.len() as u16).to_le_bytes());
    for p in paths {
        out.extend_from_slice(&(p.len() as u16).to_le_bytes());
        out.extend_from_slice(p.as_bytes());
    }
    out.extend_from_slice(&window_id.to_le_bytes());
    out
}

pub fn decode_file_drop(payload: &[u8]) -> io::Result<(f64, f64, Vec<String>, u32)> {
    if payload.len() < 18 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file-drop payload < 18 bytes",
        ));
    }
    let x = f64::from_le_bytes(payload[0..8].try_into().unwrap());
    let y = f64::from_le_bytes(payload[8..16].try_into().unwrap());
    let count = u16::from_le_bytes(payload[16..18].try_into().unwrap()) as usize;
    let mut paths = Vec::with_capacity(count);
    let mut off = 18usize;
    for _ in 0..count {
        if payload.len() < off + 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file-drop path header truncated",
            ));
        }
        let len = u16::from_le_bytes(payload[off..off + 2].try_into().unwrap()) as usize;
        off += 2;
        if payload.len() < off + len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file-drop path truncated",
            ));
        }
        let s = std::str::from_utf8(&payload[off..off + len])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        paths.push(s.to_string());
        off += len;
    }
    Ok((x, y, paths, trailing_window_id(payload, off)))
}

// ───────────────────────────────────────────────────────────────────
// Adapters between the wire types and the high-level `input` types.
// Kept in this module so both bins (shell encodes, core decodes)
// share the same translation, and a future bin can pick up either
// half without redefining it.
// ───────────────────────────────────────────────────────────────────

use crate::input_core::{KeyState, LogicalKey, MarspotKeyEvent, Modifiers, NamedKey};

pub fn mods_to_struct(b: u8) -> Modifiers {
    let (shift, ctrl, alt, super_) = mods_from_byte(b);
    Modifiers {
        shift,
        control: ctrl,
        alt,
        super_,
    }
}

pub fn struct_to_mods_byte(m: Modifiers) -> u8 {
    mods_to_byte(m.shift, m.control, m.alt, m.super_)
}

pub fn event_to_wire(ev: &MarspotKeyEvent, mods: Modifiers) -> WireKeyEvent {
    let state = match ev.state {
        KeyState::Pressed => WireKeyState::Pressed,
        KeyState::Released => WireKeyState::Released,
    };
    let (kind, key_data) = match ev.logical {
        LogicalKey::Char(c) => (WireLogicalKind::Char, c as u32),
        LogicalKey::Named(n) => (WireLogicalKind::Named, named_to_u8(n) as u32),
        LogicalKey::Other => (WireLogicalKind::Other, 0),
    };
    WireKeyEvent {
        state,
        mods: struct_to_mods_byte(mods),
        kind,
        key_data,
        text: ev.text.clone().unwrap_or_default(),
    }
}

pub fn wire_to_event(w: WireKeyEvent) -> (MarspotKeyEvent, Modifiers) {
    let state = match w.state {
        WireKeyState::Pressed => KeyState::Pressed,
        WireKeyState::Released => KeyState::Released,
    };
    let logical = match w.kind {
        WireLogicalKind::Char => char::from_u32(w.key_data)
            .map(LogicalKey::Char)
            .unwrap_or(LogicalKey::Other),
        WireLogicalKind::Named => WireNamedKey::from_u8(w.key_data as u8)
            .map(|n| LogicalKey::Named(u8_to_named(n)))
            .unwrap_or(LogicalKey::Other),
        WireLogicalKind::Other => LogicalKey::Other,
    };
    let text = if w.text.is_empty() {
        None
    } else {
        Some(w.text)
    };
    let ev = MarspotKeyEvent {
        state,
        logical,
        text,
    };
    (ev, mods_to_struct(w.mods))
}

fn named_to_u8(n: NamedKey) -> u8 {
    match n {
        NamedKey::Enter => 0,
        NamedKey::Backspace => 1,
        NamedKey::Tab => 2,
        NamedKey::Escape => 3,
        NamedKey::ArrowUp => 4,
        NamedKey::ArrowDown => 5,
        NamedKey::ArrowLeft => 6,
        NamedKey::ArrowRight => 7,
        NamedKey::PageUp => 8,
        NamedKey::PageDown => 9,
        NamedKey::Home => 10,
        NamedKey::End => 11,
        NamedKey::Insert => 12,
        NamedKey::Delete => 13,
        NamedKey::F1 => 14,
        NamedKey::F2 => 15,
        NamedKey::F3 => 16,
        NamedKey::F4 => 17,
        NamedKey::F5 => 18,
        NamedKey::F6 => 19,
        NamedKey::F7 => 20,
        NamedKey::F8 => 21,
        NamedKey::F9 => 22,
        NamedKey::F10 => 23,
        NamedKey::F11 => 24,
        NamedKey::F12 => 25,
    }
}

fn u8_to_named(w: WireNamedKey) -> NamedKey {
    match w {
        WireNamedKey::Enter => NamedKey::Enter,
        WireNamedKey::Backspace => NamedKey::Backspace,
        WireNamedKey::Tab => NamedKey::Tab,
        WireNamedKey::Escape => NamedKey::Escape,
        WireNamedKey::ArrowUp => NamedKey::ArrowUp,
        WireNamedKey::ArrowDown => NamedKey::ArrowDown,
        WireNamedKey::ArrowLeft => NamedKey::ArrowLeft,
        WireNamedKey::ArrowRight => NamedKey::ArrowRight,
        WireNamedKey::PageUp => NamedKey::PageUp,
        WireNamedKey::PageDown => NamedKey::PageDown,
        WireNamedKey::Home => NamedKey::Home,
        WireNamedKey::End => NamedKey::End,
        WireNamedKey::Insert => NamedKey::Insert,
        WireNamedKey::Delete => NamedKey::Delete,
        WireNamedKey::F1 => NamedKey::F1,
        WireNamedKey::F2 => NamedKey::F2,
        WireNamedKey::F3 => NamedKey::F3,
        WireNamedKey::F4 => NamedKey::F4,
        WireNamedKey::F5 => NamedKey::F5,
        WireNamedKey::F6 => NamedKey::F6,
        WireNamedKey::F7 => NamedKey::F7,
        WireNamedKey::F8 => NamedKey::F8,
        WireNamedKey::F9 => NamedKey::F9,
        WireNamedKey::F10 => NamedKey::F10,
        WireNamedKey::F11 => NamedKey::F11,
        WireNamedKey::F12 => NamedKey::F12,
    }
}

// ───────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {

    #[test]
    fn cli_pane_list_round_trips() {
        let panes = vec![
            (390u64, "/w/goliajp/spg".to_string(), "spg".to_string()),
            (412, "/w/stables/spg".to_string(), String::new()),
        ];
        assert_eq!(decode_cli_pane_list(&encode_cli_pane_list(&panes)).unwrap(), panes);
        let mut short = encode_cli_pane_list(&panes);
        short.truncate(20);
        assert!(decode_cli_pane_list(&short).is_err());
    }

    #[test]
    fn cli_frames_round_trip() {
        let (t, x) = decode_cli_send_text(&encode_cli_send_text("spg", "继续 autorun")).unwrap();
        assert_eq!((t.as_str(), x.as_str()), ("spg", "继续 autorun"));
        let (ok, msg) = decode_cli_result(&encode_cli_result(true, "queued on pane 390")).unwrap();
        assert!(ok);
        assert_eq!(msg, "queued on pane 390");
        // A truncated body is an error, not a short string.
        let mut short = encode_cli_send_text("spg", "hello");
        short.truncate(9);
        assert!(decode_cli_send_text(&short).is_err());
    }

    #[test]
    fn pane_inject_paste_round_trips() {
        let (sid, text) =
            decode_pane_inject_paste(&encode_pane_inject_paste(7, "line one\nline two")).unwrap();
        assert_eq!((sid, text.as_str()), (7, "line one\nline two"));
        // Truncated bodies are an error, not a silently short string.
        let mut short = encode_pane_inject_paste(7, "hello");
        short.truncate(14);
        assert!(decode_pane_inject_paste(&short).is_err());
    }

    #[test]
    fn pane_hold_grid_round_trips() {
        for on in [true, false] {
            let (sid, got) = decode_pane_hold_grid(&encode_pane_hold_grid(9_001, on)).unwrap();
            assert_eq!((sid, got), (9_001, on));
        }
        assert!(decode_pane_hold_grid(&[0u8; 8]).is_err(), "truncated payload is an error");
    }

    use super::*;
    use std::io::Cursor;

    #[test]
    fn frame_roundtrip() {
        let f = Frame::new(MsgType::KeyEvent, vec![1, 2, 3, 4, 5]);
        let mut buf = Vec::new();
        f.write_to(&mut buf).unwrap();
        let mut cur = Cursor::new(buf);
        let read = Frame::read_from(&mut cur).unwrap().unwrap();
        assert_eq!(read.msg_type, MsgType::KeyEvent);
        assert_eq!(read.payload, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn pane_badge_menu_request_roundtrip() {
        let payload = encode_pane_badge_menu_request(42, 123.5, 987.25);
        let (sid, x, y) = decode_pane_badge_menu_request(&payload).unwrap();
        assert_eq!(sid, 42);
        assert_eq!(x, 123.5);
        assert_eq!(y, 987.25);
    }

    #[test]
    fn pane_badge_menu_roundtrip() {
        let items = vec![
            PaneBadgeMenuItem { tag: 1, label: "switch to P1".into() },
            PaneBadgeMenuItem { tag: 3, label: "switch to P3".into() },
        ];
        let payload = encode_pane_badge_menu(7, 10.0, 20.0, &items);
        let (sid, x, y, decoded) = decode_pane_badge_menu(&payload).unwrap();
        assert_eq!(sid, 7);
        assert_eq!(x, 10.0);
        assert_eq!(y, 20.0);
        assert_eq!(decoded, items);
    }

    #[test]
    fn pane_badge_menu_empty_items_roundtrip() {
        let payload = encode_pane_badge_menu(7, 0.0, 0.0, &[]);
        let (_, _, _, decoded) = decode_pane_badge_menu(&payload).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn pane_badge_menu_truncated_errors() {
        let items = vec![PaneBadgeMenuItem { tag: 1, label: "switch to P1".into() }];
        let payload = encode_pane_badge_menu(7, 0.0, 0.0, &items);
        let err = decode_pane_badge_menu(&payload[..payload.len() - 1]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn pane_badge_menu_action_roundtrip() {
        let payload = encode_pane_badge_menu_action(9, 4);
        let (sid, tag) = decode_pane_badge_menu_action(&payload).unwrap();
        assert_eq!(sid, 9);
        assert_eq!(tag, 4);
    }

    #[test]
    fn frame_eof_clean_between_frames() {
        let mut cur: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        assert!(Frame::read_from(&mut cur).unwrap().is_none());
    }

    #[test]
    fn frame_bad_magic_errors() {
        let mut buf = vec![0u8; 12];
        buf[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        let mut cur = Cursor::new(buf);
        let err = Frame::read_from(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn frame_unknown_type_is_skipped_then_next_returned() {
        // Forward-compat: an unknown msg_type frame must NOT error
        // — it gets dropped, the loop reads the next frame.  This
        // property gates the Amendment 14 L3 silent self-update:
        // older L3 builds receive `RequestSelfUpdate` (id 50) and
        // must stay alive.
        let mut buf = Vec::new();
        // unknown frame
        let mut header = [0u8; HEADER_LEN];
        header[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        header[4..8].copy_from_slice(&999u32.to_le_bytes()); // unassigned
        header[8..12].copy_from_slice(&3u32.to_le_bytes());
        buf.extend_from_slice(&header);
        buf.extend_from_slice(&[0xAAu8, 0xBB, 0xCC]); // junk payload
        // followed by a known frame
        Frame::new(MsgType::Ping, encode_ping(7)).write_to(&mut buf).unwrap();
        let mut cur = Cursor::new(buf);
        let read = Frame::read_from(&mut cur).unwrap().unwrap();
        assert_eq!(read.msg_type, MsgType::Ping);
        assert_eq!(decode_ping(&read.payload).unwrap(), 7);
    }


    #[test]
    fn hello_roundtrip() {
        let p = encode_hello(7);
        assert_eq!(decode_hello(&p).unwrap(), 7);
    }

    #[test]
    fn ping_pong_roundtrip() {
        assert_eq!(decode_ping(&encode_ping(0xCAFE_BABE)).unwrap(), 0xCAFE_BABE);
        assert_eq!(decode_pong(&encode_pong(42)).unwrap(), 42);
    }

    #[test]
    fn mods_roundtrip() {
        for b in 0u8..16 {
            let (s, c, a, u) = mods_from_byte(b);
            assert_eq!(mods_to_byte(s, c, a, u), b);
        }
    }

    #[test]
    fn key_event_char_roundtrip() {
        let ev = WireKeyEvent {
            state: WireKeyState::Pressed,
            mods: mods_to_byte(false, true, false, false),
            kind: WireLogicalKind::Char,
            key_data: 'a' as u32,
            text: "a".to_string(),
        };
        let p = encode_key_event(&ev, 7);
        let (back, win) = decode_key_event(&p).unwrap();
        assert_eq!(win, 7);
        assert_eq!(back.state, ev.state);
        assert_eq!(back.mods, ev.mods);
        assert_eq!(back.kind, ev.kind);
        assert_eq!(back.key_data, ev.key_data);
        assert_eq!(back.text, ev.text);
    }

    #[test]
    fn key_event_named_arrow_roundtrip() {
        let ev = WireKeyEvent {
            state: WireKeyState::Pressed,
            mods: 0,
            kind: WireLogicalKind::Named,
            key_data: WireNamedKey::ArrowUp as u32,
            text: String::new(),
        };
        let p = encode_key_event(&ev, FIRST_WINDOW_ID);
        let (back, _win) = decode_key_event(&p).unwrap();
        assert_eq!(back.kind, WireLogicalKind::Named);
        assert_eq!(
            WireNamedKey::from_u8(back.key_data as u8),
            Some(WireNamedKey::ArrowUp)
        );
        assert_eq!(back.text, "");
    }

    #[test]
    fn mouse_roundtrip() {
        let p = encode_mouse(123.5, -45.25, 0b0010, 3);
        let (x, y, m, win) = decode_mouse(&p).unwrap();
        assert_eq!(x, 123.5);
        assert_eq!(y, -45.25);
        assert_eq!(m, 0b0010);
        assert_eq!(win, 3);
    }

    #[test]
    fn scroll_roundtrip() {
        let p = encode_scroll(0.0, 12.5, true, 9);
        let (dx, dy, precise, win) = decode_scroll(&p).unwrap();
        assert_eq!(dx, 0.0);
        assert_eq!(dy, 12.5);
        assert!(precise);
        assert_eq!(win, 9);
    }

    #[test]
    fn focus_roundtrip() {
        assert!(decode_focus(&encode_focus(true)).unwrap());
        assert!(!decode_focus(&encode_focus(false)).unwrap());
    }

    #[test]
    fn resize_roundtrip() {
        let p = encode_resize(0xDEAD_BEEF, 1200.0, 800.0, 2.0);
        let (id, w, h, s) = decode_resize(&p).unwrap();
        assert_eq!(id, 0xDEAD_BEEF);
        assert_eq!(w, 1200.0);
        assert_eq!(h, 800.0);
        assert_eq!(s, 2.0);
    }

    #[test]
    fn grid_resize_roundtrip() {
        let (c, r) = decode_grid_resize(&encode_grid_resize(203, 61)).unwrap();
        assert_eq!((c, r), (203, 61));
        assert_eq!(MsgType::from_u32(35), Some(MsgType::GridResize));
    }

    #[test]
    fn grid_scroll_roundtrip() {
        assert_eq!(decode_grid_scroll(&encode_grid_scroll(4096)).unwrap(), 4096);
        assert_eq!(MsgType::from_u32(36), Some(MsgType::GridScroll));
    }

    #[test]
    fn pane_badge_roundtrip_carries_text() {
        let p = encode_pane_badge(7, "cc:3ad170c8");
        let (sid, badge) = decode_pane_badge(&p).unwrap();
        assert_eq!(sid, 7);
        assert_eq!(badge, "cc:3ad170c8");
        assert_eq!(MsgType::from_u32(41), Some(MsgType::PaneBadge));
    }

    #[test]
    fn pane_badge_empty_means_clear() {
        let p = encode_pane_badge(9, "");
        let (sid, badge) = decode_pane_badge(&p).unwrap();
        assert_eq!(sid, 9);
        assert!(badge.is_empty());
    }

    #[test]
    fn pane_badge_truncates_at_cap() {
        let long = "x".repeat(PANE_BADGE_MAX_LEN as usize + 10);
        let p = encode_pane_badge(1, &long);
        let (_sid, badge) = decode_pane_badge(&p).unwrap();
        assert_eq!(badge.len(), PANE_BADGE_MAX_LEN as usize);
    }

    #[test]
    fn pane_badge_rejects_short_payload() {
        let err = decode_pane_badge(&[0u8; 9]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn pane_badge_clicked_roundtrip() {
        let p = encode_pane_badge_clicked(7);
        assert_eq!(decode_pane_badge_clicked(&p).unwrap(), 7);
        assert_eq!(MsgType::from_u32(42), Some(MsgType::PaneBadgeClicked));
    }

    #[test]
    fn pane_badge_clicked_rejects_bad_len() {
        let err = decode_pane_badge_clicked(&[0u8; 7]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn pane_session_begin_carries_caps() {
        let caps = PANE_SESSION_CAP_INPUT
            | PANE_SESSION_CAP_LOCK_KEYS
            | PANE_SESSION_CAP_FREEZE_GRID;
        let p = encode_pane_session_begin(11, caps);
        assert_eq!(decode_pane_session_begin(&p).unwrap(), (11, caps));
        assert_eq!(MsgType::from_u32(43), Some(MsgType::PaneSessionBegin));
    }

    #[test]
    fn pane_session_end_roundtrip() {
        let p = encode_pane_session_end(42);
        assert_eq!(decode_pane_session_end(&p).unwrap(), 42);
        assert_eq!(MsgType::from_u32(44), Some(MsgType::PaneSessionEnd));
    }

    #[test]
    fn pane_session_key_wraps_wire_key_event() {
        let ev = WireKeyEvent {
            state: WireKeyState::Pressed,
            mods: 0,
            kind: WireLogicalKind::Char,
            key_data: '\r' as u32,
            text: "\r".to_string(),
        };
        let p = encode_pane_session_key(7, &ev);
        let (sid, decoded) = decode_pane_session_key(&p).unwrap();
        assert_eq!(sid, 7);
        assert_eq!(decoded.text, "\r");
        assert_eq!(decoded.key_data, '\r' as u32);
        assert_eq!(MsgType::from_u32(45), Some(MsgType::PaneSessionKey));
    }

    #[test]
    fn pane_session_user_escape_roundtrip() {
        let p = encode_pane_session_user_escape(3);
        assert_eq!(decode_pane_session_user_escape(&p).unwrap(), 3);
        assert_eq!(MsgType::from_u32(46), Some(MsgType::PaneSessionUserEscape));
    }

    #[test]
    fn pane_session_overlay_roundtrip() {
        let body = b"hello overlay";
        let p = encode_pane_session_overlay(9, (10.0, 20.0, 100.0, 30.0), body);
        let (sid, region, decoded) = decode_pane_session_overlay(&p).unwrap();
        assert_eq!(sid, 9);
        assert_eq!(region, (10.0, 20.0, 100.0, 30.0));
        assert_eq!(decoded, body);
        assert_eq!(MsgType::from_u32(47), Some(MsgType::PaneSessionOverlay));
    }

    #[test]
    fn pane_session_overlay_rejects_short_header() {
        let err = decode_pane_session_overlay(&[0u8; 27]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn pane_session_overlay_rejects_truncated_body() {
        // 28-byte valid header claiming body_len=100, but no body.
        let mut p = encode_pane_session_overlay(1, (0.0, 0.0, 0.0, 0.0), &[0u8; 100]);
        p.truncate(28 + 50);
        let err = decode_pane_session_overlay(&p).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn selection_text_frames_roundtrip() {
        let p = encode_get_selection_text(7, (3, 100), (40, 7), true);
        let (seq, a, f, b) = decode_get_selection_text(&p).unwrap();
        assert_eq!((seq, a, f, b), (7, (3, 100), (40, 7), true));
        assert_eq!(MsgType::from_u32(37), Some(MsgType::GetSelectionText));

        let r = encode_selection_text(7, "hello\n世界");
        assert_eq!(decode_selection_text(&r).unwrap(), (7, "hello\n世界".to_string()));
        assert_eq!(
            decode_selection_text(&encode_selection_text(9, "")).unwrap(),
            (9, String::new())
        );
        assert_eq!(MsgType::from_u32(38), Some(MsgType::SelectionText));
    }

    #[test]
    fn surface_ready_roundtrip() {
        let p = encode_surface_ready(0xCAFE_BABE);
        assert_eq!(decode_surface_ready(&p).unwrap(), 0xCAFE_BABE);
    }

    #[test]
    fn surface_attach_roundtrip() {
        let p = encode_surface_attach(0xDEAD_BEEF, 0xFEED_FACE, 2160.0, 3753.0, 2.0);
        let (front, back, w, h, scale) = decode_surface_attach(&p).unwrap();
        assert_eq!(front, 0xDEAD_BEEF);
        assert_eq!(back, 0xFEED_FACE);
        assert_eq!(w, 2160.0);
        assert_eq!(h, 3753.0);
        assert_eq!(scale, 2.0);
        assert_eq!(MsgType::from_u32(40), Some(MsgType::SurfaceAttach));
    }

    #[test]
    fn preedit_roundtrip() {
        let p = encode_preedit("你好", 4);
        let (back, win) = decode_preedit(&p).unwrap();
        assert_eq!(back, "你好");
        assert_eq!(win, 4);
    }

    #[test]
    fn grid_ready_is_an_empty_poke() {
        // GridReady carries no payload — it's a pure L3→L2 wake. A
        // frame round-trips through the wire with type preserved and a
        // zero-length body.
        assert_eq!(MsgType::from_u32(34), Some(MsgType::GridReady));
        let f = Frame::new(MsgType::GridReady, Vec::new());
        let mut buf = Vec::new();
        f.write_to(&mut buf).unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let back = Frame::read_from(&mut cur).unwrap().unwrap();
        assert_eq!(back.msg_type, MsgType::GridReady);
        assert!(back.payload.is_empty());
    }

    // ─── B2: search wire roundtrips ─────────────────────────────

    #[test]
    fn search_msg_type_ids_resolve() {
        assert_eq!(MsgType::from_u32(50), Some(MsgType::SearchScrollback));
        assert_eq!(MsgType::from_u32(51), Some(MsgType::SearchResults));
        assert_eq!(MsgType::from_u32(52), Some(MsgType::SearchMore));
        assert_eq!(MsgType::from_u32(53), Some(MsgType::SearchCancel));
        // Forward-compat hole between 48 and 50: 49 stays None per
        // the silently-skip rule.
        assert_eq!(MsgType::from_u32(49), None);
    }

    #[test]
    fn search_scrollback_roundtrip_ascii() {
        let payload = encode_search_scrollback(123, false, 64, "hello world");
        let (qid, case, mt, q) = decode_search_scrollback(&payload).unwrap();
        assert_eq!(qid, 123);
        assert!(!case);
        assert_eq!(mt, 64);
        assert_eq!(q, "hello world");
    }

    #[test]
    fn search_scrollback_roundtrip_cjk() {
        let payload = encode_search_scrollback(7, true, 128, "中文 query 你好");
        let (qid, case, mt, q) = decode_search_scrollback(&payload).unwrap();
        assert_eq!(qid, 7);
        assert!(case);
        assert_eq!(mt, 128);
        assert_eq!(q, "中文 query 你好");
    }

    #[test]
    fn search_results_roundtrip_empty() {
        let payload = encode_search_results(99, false, 0, &[]);
        let (qid, more, total, hits) = decode_search_results(&payload).unwrap();
        assert_eq!(qid, 99);
        assert!(!more);
        assert_eq!(total, 0);
        assert!(hits.is_empty());
    }

    #[test]
    fn search_results_roundtrip_two_hits_two_spans() {
        let hits = vec![
            WireSearchHit {
                logical_line_idx: 1234,
                char_offset: 5,
                char_len: 3,
                snippet_match_start: 5,
                snippet_match_end: 8,
                snippet: "abcde foo xyz".into(),
                spans: vec![
                    WirePhysicalSpan { phys_row_idx: 1234, col_start: 5, col_end_inclusive: 7 },
                ],
            },
            WireSearchHit {
                logical_line_idx: 5000,
                char_offset: 0,
                char_len: 6,
                snippet_match_start: 0,
                snippet_match_end: 6,
                snippet: "foobar 中文".into(),
                spans: vec![
                    WirePhysicalSpan { phys_row_idx: 4999, col_start: 80, col_end_inclusive: 99 },
                    WirePhysicalSpan { phys_row_idx: 5000, col_start: 0, col_end_inclusive: 5 },
                ],
            },
        ];
        let payload = encode_search_results(42, true, 7, &hits);
        let (qid, more, total, decoded) = decode_search_results(&payload).unwrap();
        assert_eq!(qid, 42);
        assert!(more);
        assert_eq!(total, 7);
        assert_eq!(decoded, hits);
    }

    #[test]
    fn search_more_roundtrip() {
        let payload = encode_search_more(11, 32, 0);
        let (qid, count, dir) = decode_search_more(&payload).unwrap();
        assert_eq!(qid, 11);
        assert_eq!(count, 32);
        assert_eq!(dir, 0);
    }

    #[test]
    fn search_cancel_roundtrip() {
        let payload = encode_search_cancel(0xdead_beef);
        let qid = decode_search_cancel(&payload).unwrap();
        assert_eq!(qid, 0xdead_beef);
    }

    #[test]
    fn search_scrollback_decode_truncated_returns_err() {
        let payload = vec![1, 2, 3]; // far too short
        assert!(decode_search_scrollback(&payload).is_err());
    }

    #[test]
    fn search_results_decode_trailing_junk_rejected() {
        let mut payload = encode_search_results(1, false, 0, &[]);
        payload.push(0xff);
        assert!(decode_search_results(&payload).is_err());
    }

    #[test]
    fn file_drop_roundtrip() {
        let paths = vec![
            "/Users/x/My File.txt".to_string(),
            "/tmp/GOLIA-代表取缔役印.png".to_string(),
        ];
        let payload = encode_file_drop(123.5, -0.25, &paths, 2);
        let (x, y, got, win) = decode_file_drop(&payload).unwrap();
        assert_eq!(x, 123.5);
        assert_eq!(y, -0.25);
        assert_eq!(got, paths);
        assert_eq!(win, 2);
    }

    #[test]
    fn file_drop_empty_paths_roundtrip() {
        let payload = encode_file_drop(0.0, 0.0, &[], FIRST_WINDOW_ID);
        let (_, _, got, _) = decode_file_drop(&payload).unwrap();
        assert!(got.is_empty());
    }

    // ── RFC-005 window id: cross-version safety ───────────────
    //
    // The whole design rests on one claim: a peer that predates the
    // window id and a peer that knows about it can talk to each other
    // in either direction without losing anything.  These pin both
    // directions with hand-built payloads, because "old peer" cannot
    // be produced from this source tree.

    /// New reader ← old writer.  A payload without the trailing id
    /// must decode as the first window, not fail and not read garbage.
    #[test]
    fn input_frames_without_a_window_id_decode_as_the_first_window() {
        // Old encode_mouse: x, y, mods — 17 bytes, no tail.
        let mut legacy = Vec::new();
        legacy.extend_from_slice(&12.5f64.to_le_bytes());
        legacy.extend_from_slice(&34.5f64.to_le_bytes());
        legacy.push(0b0100);
        assert_eq!(legacy.len(), 17);
        let (x, y, m, win) = decode_mouse(&legacy).unwrap();
        assert_eq!((x, y, m), (12.5, 34.5, 0b0100));
        assert_eq!(win, FIRST_WINDOW_ID);

        // Old encode_scroll: dx, dy, precise — also 17.
        let mut legacy = Vec::new();
        legacy.extend_from_slice(&0.0f64.to_le_bytes());
        legacy.extend_from_slice(&(-3.5f64).to_le_bytes());
        legacy.push(1);
        let (_, dy, precise, win) = decode_scroll(&legacy).unwrap();
        assert_eq!(dy, -3.5);
        assert!(precise);
        assert_eq!(win, FIRST_WINDOW_ID);

        // Old encode_preedit: len-prefixed utf8, no tail.
        let text = "ねこ";
        let mut legacy = Vec::new();
        legacy.extend_from_slice(&(text.len() as u16).to_le_bytes());
        legacy.extend_from_slice(text.as_bytes());
        let (got, win) = decode_preedit(&legacy).unwrap();
        assert_eq!(got, text);
        assert_eq!(win, FIRST_WINDOW_ID);

        // Old encode_caret_rect, both variants.
        let (rect, win) = decode_caret_rect(&[0]).unwrap();
        assert!(rect.is_none());
        assert_eq!(win, FIRST_WINDOW_ID);
        let mut legacy = vec![1u8];
        for v in [1.0f64, 2.0, 3.0, 4.0] {
            legacy.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(legacy.len(), 33);
        let (rect, win) = decode_caret_rect(&legacy).unwrap();
        assert_eq!(rect, Some((1.0, 2.0, 3.0, 4.0)));
        assert_eq!(win, FIRST_WINDOW_ID);
    }

    /// Old reader ← new writer.  Every input payload must keep its
    /// legacy prefix byte-for-byte, so a peer that stops reading at the
    /// old length still gets the right values.
    #[test]
    fn window_id_is_appended_so_old_readers_see_an_unchanged_prefix() {
        let p = encode_mouse(12.5, 34.5, 0b0100, 42);
        assert_eq!(p.len(), 21, "17 legacy bytes + 4");
        assert_eq!(f64::from_le_bytes(p[0..8].try_into().unwrap()), 12.5);
        assert_eq!(f64::from_le_bytes(p[8..16].try_into().unwrap()), 34.5);
        assert_eq!(p[16], 0b0100);

        let p = encode_scroll(0.0, -3.5, true, 42);
        assert_eq!(p.len(), 21);
        assert_eq!(f64::from_le_bytes(p[8..16].try_into().unwrap()), -3.5);
        assert_eq!(p[16], 1);

        let p = encode_caret_rect(Some((1.0, 2.0, 3.0, 4.0)), 42);
        assert_eq!(p.len(), 37, "33 legacy bytes + 4");
        assert_eq!(p[0], 1);
        assert_eq!(f64::from_le_bytes(p[25..33].try_into().unwrap()), 4.0);

        // The `None` caret is a single 0 byte to an old reader, which
        // returns early on it and never looks at the tail.
        let p = encode_caret_rect(None, 42);
        assert_eq!(p[0], 0);
    }

    /// `SurfaceAttach` is the one frame that could not grow: its
    /// decoder demands exactly 32 bytes.  The window-aware form is a
    /// separate message whose first 32 bytes are still a valid legacy
    /// payload, and the legacy encoder must stay exactly 32 bytes.
    #[test]
    fn surface_attach_window_extends_a_still_valid_legacy_payload() {
        let legacy = encode_surface_attach(11, 22, 800.0, 600.0, 2.0);
        assert_eq!(legacy.len(), 32, "growing this breaks every old core");
        assert!(decode_surface_attach(&legacy).is_ok());

        let p = encode_surface_attach_window(11, 22, 800.0, 600.0, 2.0, 5);
        assert_eq!(p.len(), 36);
        assert_eq!(&p[..32], &legacy[..], "prefix must stay legacy-shaped");
        assert!(
            decode_surface_attach(&p).is_err(),
            "the strict legacy decoder rejects the longer body — which \
             is exactly why this needed its own msg type"
        );
        let (f, b, w, h, sc, win) = decode_surface_attach_window(&p).unwrap();
        assert_eq!((f, b, w, h, sc, win), (11, 22, 800.0, 600.0, 2.0, 5));
    }

    /// The window lifecycle messages must be decodable by number, and
    /// an old peer must treat them as unknown-and-skippable rather than
    /// as a stream error.
    #[test]
    fn window_lifecycle_frames_round_trip() {
        assert_eq!(
            decode_window_closed(&encode_window_closed(9)).unwrap(),
            9
        );
        assert_eq!(decode_window_focus(&encode_window_focus(3)).unwrap(), 3);
        assert_eq!(MsgType::from_u32(61), Some(MsgType::SurfaceAttachWindow));
        assert_eq!(MsgType::from_u32(62), Some(MsgType::WindowClosed));
        assert_eq!(MsgType::from_u32(63), Some(MsgType::WindowFocus));
        assert_eq!(MsgType::from_u32(64), Some(MsgType::WindowOpenRequest));
        assert_eq!(MsgType::from_u32(68), Some(MsgType::PaneHoldGrid));
        assert_eq!(MsgType::from_u32(69), Some(MsgType::PaneInjectPaste));

        assert_eq!(MsgType::from_u32(65), Some(MsgType::WindowCloseRequest));
    }

    #[test]
    fn file_drop_truncated_rejected() {
        let payload = encode_file_drop(1.0, 2.0, &["/tmp/a".to_string()], FIRST_WINDOW_ID);
        // Chopping the trailing window id is fine — that is exactly the
        // shape an old writer produces.  Cutting into the path list is
        // not.
        assert!(decode_file_drop(&payload[..10]).is_err());
        assert!(decode_file_drop(&payload[..payload.len() - 8]).is_err());
    }
}

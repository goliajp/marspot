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

pub fn encode_key_event(ev: &WireKeyEvent) -> Vec<u8> {
    let text_bytes = ev.text.as_bytes();
    let mut out = Vec::with_capacity(10 + text_bytes.len());
    out.push(ev.state as u8);
    out.push(ev.mods);
    out.push(ev.kind as u8);
    out.push(0); // pad
    out.extend_from_slice(&ev.key_data.to_le_bytes());
    out.extend_from_slice(&(text_bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(text_bytes);
    out
}

pub fn decode_key_event(payload: &[u8]) -> io::Result<WireKeyEvent> {
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
    Ok(WireKeyEvent {
        state,
        mods,
        kind,
        key_data,
        text,
    })
}

// ── Mouse / Scroll ──

pub fn encode_mouse(x: f64, y: f64, mods: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(17);
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    out.push(mods);
    out
}

pub fn decode_mouse(payload: &[u8]) -> io::Result<(f64, f64, u8)> {
    if payload.len() < 17 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "mouse payload < 17 bytes",
        ));
    }
    let x = f64::from_le_bytes(payload[0..8].try_into().unwrap());
    let y = f64::from_le_bytes(payload[8..16].try_into().unwrap());
    let mods = payload[16];
    Ok((x, y, mods))
}

pub fn encode_scroll(dx: f64, dy: f64, precise: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(17);
    out.extend_from_slice(&dx.to_le_bytes());
    out.extend_from_slice(&dy.to_le_bytes());
    out.push(if precise { 1 } else { 0 });
    out
}

pub fn decode_scroll(payload: &[u8]) -> io::Result<(f64, f64, bool)> {
    if payload.len() < 17 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "scroll payload < 17 bytes",
        ));
    }
    let dx = f64::from_le_bytes(payload[0..8].try_into().unwrap());
    let dy = f64::from_le_bytes(payload[8..16].try_into().unwrap());
    let precise = payload[16] != 0;
    Ok((dx, dy, precise))
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
    let key_payload = encode_key_event(ev);
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
    let ev = decode_key_event(&payload[8..])?;
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
pub fn encode_caret_rect(rect: Option<(f64, f64, f64, f64)>) -> Vec<u8> {
    match rect {
        None => vec![0],
        Some((x, y, w, h)) => {
            let mut out = Vec::with_capacity(33);
            out.push(1);
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
            out.extend_from_slice(&w.to_le_bytes());
            out.extend_from_slice(&h.to_le_bytes());
            out
        }
    }
}

pub fn decode_caret_rect(payload: &[u8]) -> io::Result<Option<(f64, f64, f64, f64)>> {
    if payload.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "caret_rect payload empty",
        ));
    }
    if payload[0] == 0 {
        return Ok(None);
    }
    if payload.len() < 33 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "caret_rect payload < 33 bytes",
        ));
    }
    let f = |i: usize| f64::from_le_bytes(payload[i..i + 8].try_into().unwrap());
    Ok(Some((f(1), f(9), f(17), f(25))))
}

pub fn encode_preedit(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(2 + bytes.len());
    out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(bytes);
    out
}

pub fn decode_preedit(payload: &[u8]) -> io::Result<String> {
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
    std::str::from_utf8(&payload[2..2 + len])
        .map(|s| s.to_string())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
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
        let p = encode_key_event(&ev);
        let back = decode_key_event(&p).unwrap();
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
        let p = encode_key_event(&ev);
        let back = decode_key_event(&p).unwrap();
        assert_eq!(back.kind, WireLogicalKind::Named);
        assert_eq!(
            WireNamedKey::from_u8(back.key_data as u8),
            Some(WireNamedKey::ArrowUp)
        );
        assert_eq!(back.text, "");
    }

    #[test]
    fn mouse_roundtrip() {
        let p = encode_mouse(123.5, -45.25, 0b0010);
        let (x, y, m) = decode_mouse(&p).unwrap();
        assert_eq!(x, 123.5);
        assert_eq!(y, -45.25);
        assert_eq!(m, 0b0010);
    }

    #[test]
    fn scroll_roundtrip() {
        let p = encode_scroll(0.0, 12.5, true);
        let (dx, dy, precise) = decode_scroll(&p).unwrap();
        assert_eq!(dx, 0.0);
        assert_eq!(dy, 12.5);
        assert!(precise);
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
        let p = encode_preedit("你好");
        let back = decode_preedit(&p).unwrap();
        assert_eq!(back, "你好");
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
}

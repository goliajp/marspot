// CARVE-OUT: pre-existing god file: the L1 window owner, supervisor and self-update path in one place; to be split by concern; shrink on every touch
//! marspot-shell — the outer process.
//!
//! Owns the NSWindow + NSApp event loop and a CAMetalLayer that
//! displays one IOSurface.  Spawns `marspot-core` as a child, hands it
//! the IOSurface ID via environment variables, and presents whatever
//! the child writes.
//!
//! Designed to be *boring* — anything substantive (parser, renderer,
//! input handling, layout) lives in the core process, behind a binary
//! we hot-swap during silent updates.  Keeping shell logic minimal
//! and dependencies thin means the shell almost never has to update,
//! which is what makes "no window flicker on upgrade" reachable.
//!
//! Step 1 scope: bring up the IOSurface link with a stub child that
//! draws a rotating gradient — proves the cross-process render path
//! before touching the real renderer.

use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};


use marspot::{lx_debug, lx_error, lx_event, lx_info, lx_warn};
use marspot::app::{run_app, EventProxy, MarspotApp, MarspotAppCtx, WindowAttrs};
use marspot::input::{MarspotKeyEvent, Modifiers};
use marspot::shell_proto::{
    encode_file_drop,
    encode_focus, encode_hello, encode_key_event, encode_mouse, encode_ping, encode_preedit,
    encode_scroll, encode_surface_attach, encode_surface_attach_window,
    encode_window_chrome, event_to_wire,
    struct_to_mods_byte, Frame, MsgType,
    DEFAULT_CONTROL_FD, ENV_CONTROL_FD, ENV_SURFACE_HEIGHT, ENV_SURFACE_ID,
    ENV_SURFACE_ID_BACK, ENV_SURFACE_SCALE, ENV_SURFACE_WIDTH, PROTO_VERSION,
};

mod apple_events;
mod banner;
mod core_icons;
mod launch;
mod ls_records;
mod cli_socket;
mod control_reader;
mod dev_window;
mod frame_layer;
mod identity;
mod instance;
mod observe;
mod orphans;
mod ownership;
mod pane_decls;
mod pane_status;
mod pane_tables;
mod plugins;
mod present;
mod present_shaders;
mod pump;
mod receipts;
mod reclaim;
mod resident;
mod self_exec;
#[cfg(feature = "latency-trace")]
mod screen_check;
mod shader_warm;
mod sup_log;
mod supervisor;
mod supervisor_profile;
mod surfaces;
mod theme_follow;
mod windows;
mod core_spawn;
mod core_update;
mod lifecycle;
mod dev_seams;
mod active_msg;
mod pane_upkeep;
mod supervisor_tick;
use banner::BannerKind;
use control_reader::control_reader_loop;
use launch::*;
use plugins::host::ShellPluginHost;
use plugins::PluginRegistry;
use frame_layer::FrameLayers;
use surfaces::SurfacePair;
use supervisor::{BinaryTree, ProbeVerdict, SupervisorState};

const DEFAULT_TITLE: &str = "Marspot";
const DEFAULT_W_PT: f64 = 1200.0;
const DEFAULT_H_PT: f64 = 800.0;
/// Size of the window a launch with no saved layout opens — a first
/// run, or the launch after the user closed everything.  Smaller than
/// `DEFAULT_*` on purpose: one pane, so a window sized for a wall of
/// them would be mostly empty.
const FRESH_W_PT: f64 = 800.0;
const FRESH_H_PT: f64 = 600.0;
// How often the supervisor wakes with nothing to do, while there is
// anything to poll for (see `pump` for when there is not).
//
// Everything that has to happen promptly knocks: the core's reader
// thread wakes the loop when a frame or a reply arrives, the command
// socket after queueing a request, the plugin host after queueing an
// op.  What is left on this timer is polling, and the fastest poller
// is the pane sweep at one second (`pane_status::SWEEP_INTERVAL`); the
// plugins want two, the core healthcheck five, autorun thirty.  So the
// timer runs at the rate of its fastest consumer and no faster.
const REDRAW_INTERVAL_MS: u64 = 1_000;

/// Frames the reader thread parses off the control socket and hands
/// to the main thread.
enum ShellInbox {
    SurfaceReady(u32),
    FramePaceAck,
    HelloAck(u32),
    Pong(u32),
    /// Focused-pane caret rect from the core (view-local physical
    /// pixels), forwarded to AppKit so the IME candidate window
    /// anchors under the caret.
    /// Caret rect plus the window it belongs to — the shell parks the
    /// IME candidate panel against that window.
    CaretRect(Option<(f64, f64, f64, f64)>, u32),
    /// Core rendered a fresh frame into the IOSurface — present it.  Carries
    /// nothing; its arrival (via the reader's `proxy.wake()`) drives the
    /// per-frame present, replacing the blind ~60 fps redraw timer.
    FrameRendered,
    /// L2 → L1: user clicked the active prefix of a pane badge on the
    /// pane backing this shelld session.  Dispatched to every loaded
    /// plugin so whichever set the badge can react (claudecode → cycle
    /// the next profile and rerun `claudeN --resume <uuid>`).
    PaneBadgeClicked(u64),
    /// core → shell: the user's focus moved to this pane.
    PaneFocused(u64),
    /// L2 → L1: user right-clicked the badge prefix — collect menu
    /// rows from the plugins and reply with a `PaneBadgeMenu` frame.
    /// `(sid, anchor_x, anchor_y)`; the anchor is echoed back so the
    /// exchange is stateless.
    PaneBadgeMenuRequest(u64, f64, f64),
    /// L2 → L1: user picked a badge-menu row.  `(sid, tag)` where
    /// `tag` is the plugin-assigned id from the menu frame.
    PaneBadgeMenuAction(u64, u32),
    /// L2 → L1: a keystroke arrived on a pane held by a LOCK_KEYS
    /// PaneSession.  Routed to the matching session's on_user_key.
    PaneSessionKey(u64, marspot::shell_proto::WireKeyEvent),
    /// L2 → L1: user pressed Esc 3× in 5 s; force-end the session
    /// regardless of plugin opinion.
    PaneSessionUserEscape(u64),
    /// L2 → L1: reopen a window the last session had, restoring its
    /// geometry from this entry of `window-state.bin`.  RFC-005 step
    /// 6b — the core knows how many windows the user had, L1 owns the
    /// windows themselves, so the core asks.
    WindowOpenRequest(u32, Option<(f64, f64)>),
    /// L2 → L1: close this window (RFC-005 step 5 — its last pane was
    /// just moved out).  Runs the exact red-button path.
    WindowCloseRequest(u32),
    /// The core wants a toolbar icon drawn: `(symbol, point_px)`.
    IconMaskWanted(u16, u16),
}

/// How long after spawn we expect HELLO_ACK before declaring the core
/// hung at startup. Generous enough to cover a cold-cache first-run
/// of a freshly-built binary: macOS Gatekeeper provenance check on
/// the just-promoted current/marspot-core (~400 ms when warm, more
/// on cold cache) PLUS 9 cold L3 spawns (each another fresh-exec
/// provenance check) PLUS ShelldClient handshake PLUS list_sessions/
/// attach RPC roundtrips. 5 s used to be enough, but install-local's
/// silent update on a fresh release build occasionally tripped past
/// it (observed on 0.2.10 → 0.2.11), so bump to 15 s. A *real* hang
/// is still caught — the PONG_DEADLINE (15 s × 3 PING_INTERVALs) is
/// the steady-state liveness check.
const HELLO_TIMEOUT: Duration = Duration::from_secs(15);
/// How often we issue PING.
const PING_INTERVAL: Duration = Duration::from_secs(5);
/// PONG must arrive within this many `PING_INTERVAL`s before we call
/// the core hung.  3 = 15 s, which gives plenty of slack for a busy
/// terminal session without making a real hang feel sticky.
const PONG_DEADLINE: Duration = Duration::from_secs(15);
/// A supervisor tick that arrives this long after the previous one
/// means **we** stopped running — the machine slept, a build storm
/// took every core, the main thread sat in AppKit.  Silence measured
/// across such a gap says nothing about the core, because nobody was
/// listening; a watchdog has to know whether it was awake before it
/// accuses anyone else of being asleep.  Deadlines are handed the gap
/// back rather than charged for it.
///
/// Measured past the moment the clock was told to tick, which while
/// awake is never more than a ping interval away: far outside ordinary
/// jitter, far inside a real hang.
const SUPERVISOR_STALL_GAP: Duration = PING_INTERVAL;
/// Crash-budget window.  More than `MAX_CRASHES_IN_WINDOW` in this
/// span and we stop auto-restarting (binary is broken; user needs
/// to roll back or reinstall).
const CRASH_WINDOW: Duration = Duration::from_secs(300); // 5 min
const MAX_CRASHES_IN_WINDOW: usize = 3;

/// Has this core missed its PONG deadline?
///
/// A free function so the rule can be stated once and tested without
/// an AppKit window: the whole 2026-08-11 cascade was this predicate
/// being fed an anchor that predated the core's ability to answer.
fn pong_overdue(hello_acked: bool, last_pong_at: Instant, now: Instant) -> bool {
    hello_acked && now.duration_since(last_pong_at) > PONG_DEADLINE
}

/// Move a deadline anchor forward by a stall we didn't witness,
/// never past `now` — the core gets the time back, but no credit it
/// hasn't earned yet.
fn forgive_stall(anchor: Instant, gap: Duration, now: Instant) -> Instant {
    (anchor + gap).min(now)
}

/// How long to stay hands-off after the crash budget trips.
///
/// One `CRASH_WINDOW`, doubling per trip up to 8×.  The first wait is
/// the natural unit: after `CRASH_WINDOW` of quiet the crash record
/// would have aged out anyway, so trying again then is exactly as
/// safe as never having tripped.  The doubling is what stops a
/// genuinely broken binary turning this into a slow loop — four
/// restarts per five minutes, then per ten, per twenty, per forty,
/// and there it stays.
fn restart_cooldown(trips: u32) -> Duration {
    CRASH_WINDOW * (1 << trips.saturating_sub(1).min(3))
}

#[cfg(test)]
mod supervisor_deadline_tests;

/// Everything tied to one live core process: its child handle, the
/// control socket (both directions), and the liveness-handshake
/// bookkeeping.  Aggregating these is what makes a flash-free silent
/// update possible — the shell can hold an `active` and a `pending`
/// CoreConn at once, let a fresh core prove itself, and only then
/// atomic-swap the presenter and tear the old one down.
///
/// `child` is `Option` because the child can legitimately be gone
/// while the rest of the conn lives: after a HELLO mismatch we kill
/// the child but keep `spawned_at` so the HELLO-timeout path in
/// `poll_supervisor` respawns; and `poll_supervisor` reaps the child
/// (`child.take()`) before `restart_core` drops the whole conn.
struct CoreConn {
    /// The core child process, or `None` once killed/reaped.
    child: Option<Child>,
    /// Parent end of the AF_UNIX socketpair we share with the core.
    /// Wrapped in `Mutex` so the `MarspotApp` callbacks (all on the
    /// main thread, but the type system doesn't know that) can mutate
    /// it without splitting the struct.  Frames written here arrive
    /// at the core's stdin-side fd 3 / `MARSPOT_SHELL_CONTROL_FD`.
    control_tx: Mutex<UnixStream>,
    /// Receives parsed inbound frames from this core's reader thread.
    control_rx: Receiver<ShellInbox>,
    /// True after this core has confirmed at least one HelloAck.
    hello_acked: bool,
    /// The core answered `FramePace`: it waits for `FrameShown`, so its
    /// surfaces go on the window as they are.  Settled by the HelloAck,
    /// which such a core sends after that answer.
    paced: bool,
    /// Surfaces announced before the HelloAck said how to show them.
    early_ready: Vec<u32>,
    /// When this core was spawned — drives the HELLO timeout.
    spawned_at: Instant,
    /// When to fire the next PING to this core.
    next_ping_at: Instant,
    /// Nonce of the most recent PING we sent this core.  Pongs with a
    /// different nonce are stale and ignored.  Per-conn: a fresh core
    /// gets its own channel, so a previous core's Pong can't reach it.
    last_ping_nonce: u32,
    /// When the most recent matching Pong arrived — or, until the
    /// first one does, when the core became **able** to answer at all
    /// (its HelloAck).  Seeding this at spawn instead is what turned
    /// one slow boot into a bricked window: see the reset in the
    /// `HelloAck` arm.
    last_pong_at: Instant,
}

impl CoreConn {
    /// Write a frame to this core.  Logs and drops on EPIPE; a core
    /// dying mid-session is the supervisor's problem, not this path's.
    /// True when the frame was written.
    fn send(&self, msg_type: MsgType, payload: Vec<u8>) -> bool {
        let frame = Frame::new(msg_type, payload);
        let mut stream = match self.control_tx.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(), // poisoned: still try
        };
        if let Err(e) = frame.write_to(&mut *stream) {
            lx_warn!(
                "shell.control_frame_write_failed",
                &format!("{e}"),
                msg_type = format!("{:?}", msg_type)
            );
            return false;
        }
        true
    }

    /// Drop the control socket (clean EOF for the core's read side),
    /// then SIGKILL + reap the child.  Order matters: the EOF gives a
    /// well-behaved core the chance to exit on its own before the kill.
    fn shutdown(self) {
        let CoreConn { child, control_tx, control_rx, .. } = self;
        drop(control_tx);
        drop(control_rx);
        if let Some(mut child) = child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Everything the shell owns on behalf of ONE native window.
///
/// RFC-005 — these used to sit directly on `ShellApp`, which is how
/// "there is exactly one window" was encoded on the L1 side.  The core
/// stays single (one process drives every window); what multiplies is
/// the window-shaped kernel state: a surface pair, the presenter that
/// samples it, and the gates that say whether this window has anything
/// worth showing yet.
struct ShellWindow {
    /// Matches the id the view/delegate tag their events with, and the
    /// id carried on the wire.
    window_id: u32,
    /// Which entry of `window-state.bin` this window's geometry lives
    /// in — and, by construction, which entry of `shell-state.bin`'s
    /// window list holds its panes (the core keeps the same number in
    /// `WindowState::frame_index`).
    ///
    /// Fixed for the window's life, deliberately not its position in
    /// `self.windows`: writing frames by position meant closing a
    /// window shifted every later window down a slot, so the survivors
    /// wrote over the closed window's geometry and inherited each
    /// other's on the next launch.
    frame_index: usize,
    /// The pair the core is drawing: one half is on the window.
    surfaces: Option<SurfacePair>,
    /// Created in `resized` (or `restart_core`) and not yet promoted.
    /// Once the core confirms via `SurfaceReady(id)` matching either
    /// id of this pair, we install it as the live pair and let the
    /// presenter swap to the ready id.  A later resize replaces the
    /// pending entry; the dropped one is abandoned (`release`).
    pending_surfaces: Option<SurfacePair>,
    /// What puts a surface on this window's view.
    layers: Option<FrameLayers>,
    /// True after the core has confirmed at least one SurfaceReady for
    /// THIS window.  Until then `redraw` skips `present()` so the user
    /// sees the NSWindow's BG colour (the terminal's) instead of an
    /// unfilled black IOSurface — kills the cold-start flash.
    first_frame_ready: bool,
    /// `redraw()` only calls `present()` when this is true.  Set by
    /// every `FrameRendered` poke from the active core (its
    /// per-frame ack that the IOSurface has been fully written +
    /// waitUntilCompleted'd).  Cleared by `redraw` after `present()`.
    ///
    /// Why: the redraw pump (`start_redraw_pump`) wakes the AppKit
    /// redraw callback unconditionally — if the callback then samples
    /// the IOSurface, it can land MID-RENDER (between core's BG pass
    /// and FG glyph pass), producing the frequent "all 9 panes'
    /// contents momentarily disappear and reappear" flash documented in
    /// the 2026-06-15 debugging.  So a window is presented only when
    /// this is set: a new frame, or a banner change (`refresh_banner`),
    /// which is drawn by the presenter and has no frame of its own.
    /// It stays set when `present()` could not draw (no drawable), so
    /// the next wake tries again.  A poke the main thread missed is not
    /// lost either: every wake drains the core's inbox first.
    ///
    /// There used to be a second net here, a present every 5 s with
    /// nothing pending, for "core froze mid-render and no poke is
    /// coming".  A frozen core is the PONG deadline's to catch, and
    /// presenting the same surface again shows nothing new; what that
    /// net really covered was the skipped present and the banner, both
    /// handled above.  Each spurious present made WindowServer
    /// composite again.
    frame_pending: bool,
    /// Whether this window has been presented once, for the one-line
    /// `WINDOW_FIRST_PRESENT` log.
    presented: bool,
    /// Dev seam only: this window is to be closed again as soon as it
    /// has painted (`MARSPOT_DEV_CLOSE_EXTRA`).  Latched so the close
    /// runs once.
    dev_close_after_paint: bool,
    /// F3+6.1 — last saved (x, y, w, h, display_id) so per-frame
    /// `resized` / `moved` callbacks dedup: only the saves where the
    /// frame actually changed since the last bin write actually fire
    /// I/O.  Eliminates the live-resize 60+ writes/s storm.
    last_saved_window: Option<(f64, f64, f64, f64, u32)>,
    /// What the last `SurfaceAttach` for this window described.  A
    /// resize notification carrying none of it new must not allocate a
    /// pair: AppKit sends streams of them (2026-09-27, ~144/s at the
    /// display rate for a window nobody was resizing), and each pair
    /// costs two IOSurfaces here, two Metal textures in the core, and
    /// a presenter swap onto a surface the core has only just started
    /// painting — which is the flicker that was reported.
    last_attached: Option<AttachedGeometry>,
    /// Resize notifications skipped since the last real one, counted so
    /// the log says how many arrived without saying it 144 times a
    /// second.
    skipped_resizes: u32,
}

/// What a `SurfaceAttach` told the core about a window.
#[derive(Clone, Copy)]
struct AttachedGeometry {
    w_px: usize,
    h_px: usize,
    scale: f64,
    lights_right_phys: f64,
}

impl AttachedGeometry {
    /// Does this describe the same pixels as `other`?  The chrome
    /// measurement is deliberately excluded: it travels in a frame of
    /// its own that costs no surfaces.
    fn same_pixels(&self, other: &Self) -> bool {
        self.w_px == other.w_px && self.h_px == other.h_px && self.scale == other.scale
    }
}

/// What a resize notification is actually worth.
#[derive(Debug, PartialEq)]
enum ResizeWork {
    /// The pixels moved: a fresh pair, and the core is told.
    Attach,
    /// Only the window-button cluster moved (a full-screen
    /// transition): one small frame, no surfaces.
    ChromeOnly,
    /// Nothing in it is new.
    Nothing,
}

/// Decide it from the last attach alone, so the rule is one place and
/// testable away from AppKit.
fn resize_work(prev: Option<AttachedGeometry>, now: &AttachedGeometry) -> ResizeWork {
    let Some(prev) = prev else { return ResizeWork::Attach };
    if !prev.same_pixels(now) {
        return ResizeWork::Attach;
    }
    if (prev.lights_right_phys - now.lights_right_phys).abs() > 0.5 {
        return ResizeWork::ChromeOnly;
    }
    ResizeWork::Nothing
}

#[cfg(test)]
mod resize_work_tests;

impl ShellWindow {
    fn new(window_id: u32, frame_index: usize) -> Self {
        Self {
            window_id,
            frame_index,
            surfaces: None,
            pending_surfaces: None,
            layers: None,
            first_frame_ready: false,
            frame_pending: false,
            dev_close_after_paint: false,
            presented: false,
            last_saved_window: None,
            last_attached: None,
            skipped_resizes: 0,
        }
    }
}

struct ShellApp {
    proxy: EventProxy,
    /// One entry per open native window, in creation order.  Never
    /// empty while the app runs; `windows[0]` is the boot window.
    windows: Vec<ShellWindow>,
    /// Panes to wake now that the user is back, one at a time.
    ///
    /// Coming back to marspot after a while means every parked pane is
    /// about to be wanted, and each wake costs seconds — claude has to
    /// start and read its whole transcript before it paints.  Paying
    /// that on the click is what reads as "the first pane I touch
    /// hangs".  Paid on the way in instead, it overlaps with the user
    /// reading whatever they came back for.
    ///
    /// Staggered rather than fanned out: sixteen claudes starting at
    /// once is a CPU spike on a machine whose whole point is not
    /// having one.
    wake_queue: std::collections::VecDeque<u64>,
    /// When the next entry of `wake_queue` may go.
    wake_next_at: Option<Instant>,
    /// Was the app focused at the last `focused` callback?  The queue
    /// is filled on the *edge* — a click inside an already-focused
    /// window is not a return.
    app_focused: bool,
    /// Slot promised to a window that has been asked for but has not
    /// opened yet, keyed by the id it will carry.  A restore knows its
    /// slot (the core named it); a Cmd-N window takes the next free
    /// one.  Consumed by `window_opened`.
    pending_frame_index: std::collections::HashMap<u32, usize>,
    /// Next id to hand a window.  Monotonic and never reused within a
    /// run: the core keys `WindowState` off it, and a recycled id
    /// would let a closed window's frames land in its successor.
    ///
    next_window_id: u32,
    /// The live core: child process, control socket (both directions),
    /// and liveness-handshake state aggregated into `CoreConn`.
    /// `None` before the first spawn and in the brief gap between a
    /// crash (or single-core swap retirement) and the respawn.
    active: Option<CoreConn>,
    pump: Option<pump::Pump>,
    /// Binary slot manager: current / prev / pending.  Used to find
    /// the core binary at spawn time and to atomic-swap when a
    /// silent update fires.
    binaries: BinaryTree,
    /// Where in the silent-update lifecycle we are.  `Idle` most of
    /// the time; flips to `Probing` while a staged binary's Gatekeeper
    /// verdict is being warmed on a background thread.
    sup_state: SupervisorState,
    /// Receives the verdict from the in-flight probe thread.  `Some`
    /// exactly while `sup_state` is `Probing`; the layer being probed
    /// rides in the state, so the channel only carries the outcome.
    probe_rx: Option<mpsc::Receiver<ProbeVerdict>>,
    /// Recent crash timestamps inside the `CRASH_WINDOW` rolling
    /// window.  Used to refuse auto-restart on a binary that's
    /// flapping.
    crashes: std::collections::VecDeque<Instant>,
    /// True if the crash budget has been blown.  We stop trying to
    /// restart until the cool-down below expires.
    auto_restart_disabled: bool,
    /// When the budget tripped, and how many times it has tripped
    /// this boot.  Together they set the cool-down before we try
    /// again — because "stop restarting" used to mean *forever*, and
    /// a load storm that passed in ninety seconds left the window
    /// frozen behind a banner until the user noticed and quit the
    /// app.  Every extra trip doubles the wait, so a genuinely broken
    /// binary still can't loop.
    disabled_at: Option<Instant>,
    budget_trips: u32,
    /// When `poll_supervisor` last ran — the witness for
    /// [`SUPERVISOR_STALL_GAP`].  `None` until the first tick: the
    /// sixteen seconds between constructing this struct and the first
    /// supervisor pass are the shell *booting*, not the supervisor
    /// falling behind, and logging that as a stall would leave a
    /// false witness in every log for anyone who later reads one.
    last_tick_at: Option<Instant>,
    /// Currently-displayed banner, or `None` for clear.  Kept on
    /// the shell so `poll_supervisor` can recompute it from state
    /// transitions and call `presenter.set_banner` only when it
    /// actually changes.
    banner_kind: Option<banner::Shown>,
    /// This boot came up under the crash-loop brake: reattach only,
    /// spawn nothing fresh, restore no extra windows.  Read once from
    /// `MARSPOT_SAFE_MODE` (set in `main`, inherited by the core).
    safe_mode: bool,
    /// Dev seam only (`MARSPOT_DEV_CLOSE_EXTRA`): close each extra
    /// window again once it has painted.  A script cannot deliver the
    /// red button or Cmd-W to the sandbox app, and closing a second
    /// window is what crashed the app on 2026-07-28 — so the path has
    /// to be reachable from a test somehow.  Unset in the installed
    /// app.
    dev_close_extra: bool,
    /// Dev seam only (`MARSPOT_DEV_CLOSE_SEQUENCE=2,1`): press the red
    /// button on these windows in this order, one every couple of
    /// seconds, once they have all painted.  Exists because "which
    /// window you closed first changed what survived" is the shape of
    /// the 2026-08-03 report, and the only way to reproduce it is to
    /// actually close windows in a given order and quit.  Unset in the
    /// installed app.
    dev_close_sequence: std::collections::VecDeque<u32>,
    /// Dev seam (`MARSPOT_DEV_BADGE_MENU=<path>`): a file a test drops
    /// `<sid> <tag>` into to pick one badge-menu entry, as if the
    /// pointer had.  A file rather than an env value because the pick
    /// only means anything once the pane's agent is up, which is
    /// minutes of shell activity after boot — and a script cannot move
    /// the pointer in the sandbox app.  Unset in the installed app.
    dev_badge_menu_file: Option<std::path::PathBuf>,
    /// When the next entry of `dev_close_sequence` is due.  `None`
    /// until every window named in it is up.
    dev_close_next_at: Option<Instant>,
    /// Dev seam only (`MARSPOT_DEV_QUIT_AFTER_PAINT=<n>`): once `n`
    /// windows have painted, quit the way the dock's Quit item does.
    /// A script cannot open a dock menu, and what has to be tested is
    /// the route — `terminate:` used to land on the boot window, and
    /// reading that as "close that window" is what left every
    /// other window on screen (2026-09-28 report).  Unset in the
    /// installed app.
    dev_quit_after_paint: Option<usize>,
    /// When the dev-seam quit is due.  `None` until the windows it
    /// waits for have painted — the app has to be all the way up, the
    /// way it is when someone reaches for the dock.
    dev_quit_at: Option<Instant>,
    /// How many cores this shell has spawned.  1 = the boot core; any
    /// higher value means a replacement (silent update, crash
    /// respawn), which must not re-run the saved-window restore.
    core_generation: u32,
    /// Rolling 30 s ring of CORE_SPAWN timestamps.  When this fills
    /// (≥3 entries inside the window) we emit a `CORE_BOOT_LOOP`
    /// WARN — the alarm the 2026-06-15 incident lacked.  In that
    /// case six cores booted in three minutes and the operator had
    /// to manually count `grep CORE_BOOT` lines to spot the loop.
    /// Bound at 16 so even a runaway respawn can't grow the ring.
    core_boot_ring: std::collections::VecDeque<Instant>,
    /// RFC-001 plugin host.  Plugins read pane / pty info through it.
    /// Registry owns the loaded plugins + drives their lifecycle.
    /// Both `None`-able so a misbuilt host (env-var disabled etc.)
    /// gracefully degrades to plugin-less marspot.
    plugin_host: ShellPluginHost,
    plugin_registry: PluginRegistry,
    /// When the supervisor last drove plugin ticks.  Coarse — the
    /// supervisor runs on the 1 s redraw pump plus whatever knocks it
    /// (core frames, queued plugin work), so a plugin asking for less
    /// than a second gets the supervisor's cadence.  Their per-plugin interval
    /// gating lives in the registry.
    last_plugin_tick: Instant,
    /// Generic per-pane foreground status (shell at prompt / job
    /// running / unknown), swept once a second from the supervisor
    /// tick and published to `plugin_host` so plugins read a snapshot.
    /// The pane-status layer that knows about specific programs lives
    /// in the plugins on top of this one.
    pane_status: pane_status::PaneStateTracker,
    /// Recede level, badge and title per session, sent to the core on
    /// change and all again to a new core.
    pane_look: pane_decls::PaneLook,
    /// Plugin → shell reports of what the program in a pane is doing.
    /// Drained at the top of each supervisor tick, straight into the
    /// state machines.  Same shape as the badge channel: plugins push,
    /// the main loop owns the state.
    pane_activity_rx:
        std::sync::mpsc::Receiver<(u64, marspot::pane_state::Activity)>,
    /// Receiver half of the channel `ShellPluginHost::set_pane_badge`
    /// pushes into; drained each `poll_supervisor` tick and forwarded
    /// to the active core as `MsgType::PaneBadge` frames.
    pane_badge_rx: std::sync::mpsc::Receiver<plugins::host::PaneBadgeUpdate>,
    pane_wheel_keys_rx: std::sync::mpsc::Receiver<plugins::host::PaneWheelKeysUpdate>,
    pane_agent_tui_rx: std::sync::mpsc::Receiver<plugins::host::PaneAgentTuiUpdate>,
    pane_render_markup_rx:
        std::sync::mpsc::Receiver<plugins::host::PaneRenderMarkupUpdate>,
    /// Per-pane declarations and autorun memory, dropped with the pane.
    pane_tables: pane_tables::PaneTables,
    /// Same shape as `pane_badge_rx` but for plugin-set pane titles.
    pane_title_rx: std::sync::mpsc::Receiver<plugins::host::PaneTitleUpdate>,
    /// The core's toolbar icons, drawn by a short-lived drawer.
    core_icons: core_icons::CoreIcons,
    /// Receiver for `begin_pane_session` requests.
    pane_session_begin_rx:
        std::sync::mpsc::Receiver<plugins::host::PaneSessionBeginRequest>,
    /// Submitted PTY operations, on their way into `pty_ops`.
    pty_op_rx: std::sync::mpsc::Receiver<plugins::host::PtyOpRequest>,
    /// Requests from the command socket (`marspot-shell --send`).
    cli_rx: std::sync::mpsc::Receiver<cli_socket::CliRequest>,
    /// Panes the autorun policy is switched on for, by working
    /// directory — the one identity that survives a restart, a rename
    /// (there are none) and a move between windows.
    autorun_panes: std::collections::HashSet<String>,
    /// When the panes were last looked at.  Once a minute is plenty:
    /// every trigger requires the pane to have been quiet for longer
    /// than that already.
    autorun_last_look: Option<Instant>,
    /// The one queue that keeps two scripts from typing into the same
    /// pane at once.  Lives here because plugins are not its only
    /// submitter — the `--send` CLI is another.
    pty_ops: plugins::pty_op::PtyOps,
    /// How that queue starts a run.  Not the plugin host: that one
    /// checks the current *plugin's* permissions, and a run started by
    /// the supervisor has no plugin behind it.
    op_host: SupervisorOpHost,
    /// Receiver for cc-plugin inject-input requests; drained each tick
    /// and forwarded to the active core as `MsgType::InjectInput` frames.
    inject_input_rx: std::sync::mpsc::Receiver<plugins::host::InjectInputRequest>,
    /// Active PaneSessions held by L1 plugins, keyed by shelld
    /// session_id.  At most one per pane.
    active_pane_sessions:
        std::collections::HashMap<u64, ActivePaneSession>,
    /// In-memory mirror of `window-state.bin`, seeded from disk at
    /// startup and updated on every save.
    ///
    /// It exists because the file is a positional list and this process
    /// does not own all of it yet: on a cold launch (including the boot
    /// after an L1 self-execv) window 0 opens first and saves, while
    /// window 1's geometry is still only on disk, waiting for the core
    /// to ask for it.  Writing just the live windows there deleted that
    /// entry — see `save_window_frames`.
    saved_window_frames: Vec<marspot::state::SavedWindow>,
}

/// Bundle: the plugin's session object + the metadata we need to log
/// + cleanly tear it down (plugin name for log namespace).
///
/// Starts the queue's runs.
///
/// One method's worth of type, but it must not be the plugin host:
/// that one gates `begin_pane_session` on the *current plugin's*
/// permissions, and a run the supervisor starts has no plugin behind
/// it.  Routing through it denied the first CLI delivery with
/// `missing permission: PermissionSet(16)`, from the permission model
/// since removed — the supervisor asking
/// itself for permission it has no identity to hold.
struct SupervisorOpHost {
    begin_tx: std::sync::mpsc::Sender<plugins::host::PaneSessionBeginRequest>,
}

impl plugins::pty_op::OpHost for SupervisorOpHost {
    fn begin(
        &self,
        shelld_session_id: u64,
        session: Box<dyn plugins::PaneSession>,
    ) -> Result<(), String> {
        self.begin_tx
            .send(plugins::host::PaneSessionBeginRequest {
                shelld_session_id,
                plugin_name: "pty_op",
                session,
            })
            .map_err(|_| "main loop dropped".to_string())
    }

    fn log(&self, level: plugins::LogLevel, tag: &str, msg: &str) {
        match level {
            plugins::LogLevel::Warn => {
                lx_warn!("shell.pty_op", msg, tag = tag)
            }
            _ => lx_info!("shell.pty_op", msg, tag = tag),
        }
    }
}

struct ActivePaneSession {
    session: Box<dyn plugins::PaneSession>,
    plugin_name: &'static str,
}

/// Concrete `PaneSessionHost` constructed per-dispatch; lives only
/// for the duration of one plugin callback.  set_badge → the plugin
/// host's badge path (the same change check a plugin's badge goes
/// through), end → flips a `Cell` the main-loop checks after the
/// callback returns.  log → logx via the plugin namespace.
struct ConcretePaneSessionHost<'a> {
    sid: u64,
    plugin_name: &'static str,
    plugin_host: &'a ShellPluginHost,
    end_requested: &'a std::cell::Cell<bool>,
}

impl<'a> plugins::PaneSessionHost for ConcretePaneSessionHost<'a> {
    fn shelld_session_id(&self) -> u64 {
        self.sid
    }
    fn end(&self) {
        self.end_requested.set(true);
    }
    fn set_badge(&self, text: &str) {
        self.plugin_host.send_pane_badge(self.sid, text);
    }
    fn log(&self, level: plugins::LogLevel, tag: &str, msg: &str) {
        let composed = format!("plugin.{}.{}", self.plugin_name, tag);
        match level {
            plugins::LogLevel::Debug => marspot::lx_debug!(&*composed, msg),
            plugins::LogLevel::Info => marspot::lx_info!(&*composed, msg),
            plugins::LogLevel::Warn => marspot::lx_warn!(&*composed, msg),
        }
    }
}


impl ShellApp {
    fn new(proxy: EventProxy) -> Self {
        let binaries = BinaryTree::default_for("marspot-core")
            .expect("HOME must be set to manage binary slots");
        let (pane_badge_tx, pane_badge_rx) = std::sync::mpsc::channel();
        let (pane_wheel_keys_tx, pane_wheel_keys_rx) = std::sync::mpsc::channel();
        let (pane_agent_tui_tx, pane_agent_tui_rx) = std::sync::mpsc::channel();
        let (pane_render_markup_tx, pane_render_markup_rx) = std::sync::mpsc::channel();
        let (pane_title_tx, pane_title_rx) = std::sync::mpsc::channel();
        let (pane_session_begin_tx, pane_session_begin_rx) = std::sync::mpsc::channel();
        let pane_session_begin_tx_for_ops = pane_session_begin_tx.clone();
        let (pty_op_tx, pty_op_rx) = std::sync::mpsc::channel();
        let (cli_tx, cli_rx) = std::sync::mpsc::channel();
        // Best-effort: a shell that cannot bind still runs the
        // terminal, it just cannot be asked to type into it.
        let wake_for_cli: cli_socket::Wake = {
            let p = proxy.clone();
            std::sync::Arc::new(move || p.wake())
        };
        if let Err(e) = cli_socket::serve(cli_tx, wake_for_cli) {
            lx_warn!("shell.cli.bind_failed", &format!("{e}"));
        }
        let wake_for_front = proxy.clone();
        instance::serve_front(move || wake_for_front.wake());
        // Best-effort like the command socket above: a shell that
        // cannot bind this one still runs the terminal, and submits
        // fall back to reading the pane.
        // The secret both layers derive pane tokens from. L1 makes it
        // because L1 owns the state root; L3 only reads it, so that
        // spawning a pane stays free of filesystem side effects.
        marspot_term::submit_receipt::ensure_secret();
        let receipts = receipts::Receipts::new();
        if let Err(e) = receipts::serve(std::sync::Arc::clone(&receipts)) {
            lx_warn!("shell.receipt.bind_failed", &format!("{e}"));
        }
        let (inject_input_tx, inject_input_rx) = std::sync::mpsc::channel();
        // The queue's own route to a PTY: the same channel plugins use,
        // so nothing has a private path to a pane.
        let inject_input_tx_for_ops = inject_input_tx.clone();
        let (pane_activity_tx, pane_activity_rx) = std::sync::mpsc::channel();
        // Cloned before the move: the plugin host gets its own knock.
        let wake_for_host: std::sync::Arc<dyn Fn() + Send + Sync> = {
            let p = proxy.clone();
            std::sync::Arc::new(move || p.wake())
        };
        Self {
            proxy,
            // The boot window is always entry 0 of both saved lists.
            windows: vec![ShellWindow::new(
                marspot::shell_proto::FIRST_WINDOW_ID,
                0,
            )],
            wake_queue: std::collections::VecDeque::new(),
            wake_next_at: None,
            app_focused: true,
            pending_frame_index: std::collections::HashMap::new(),
            next_window_id: marspot::shell_proto::FIRST_WINDOW_ID + 1,
            active: None,
            pump: None,
            binaries,
            safe_mode: std::env::var("MARSPOT_SAFE_MODE").is_ok(),
            dev_close_extra: false,
            dev_close_sequence: std::env::var("MARSPOT_DEV_CLOSE_SEQUENCE")
                .ok()
                .map(|s| s.split(',').filter_map(|p| p.trim().parse().ok()).collect())
                .unwrap_or_default(),
            dev_badge_menu_file: std::env::var_os("MARSPOT_DEV_BADGE_MENU")
                .map(std::path::PathBuf::from),
            dev_close_next_at: None,
            dev_quit_after_paint: std::env::var("MARSPOT_DEV_QUIT_AFTER_PAINT")
                .ok()
                .and_then(|v| v.parse().ok()),
            dev_quit_at: None,
            core_generation: 0,
            sup_state: SupervisorState::Idle,
            probe_rx: None,
            crashes: std::collections::VecDeque::new(),
            auto_restart_disabled: false,
            disabled_at: None,
            budget_trips: 0,
            last_tick_at: None,
            banner_kind: None,
            core_boot_ring: std::collections::VecDeque::with_capacity(16),
            plugin_host: {
                let h = ShellPluginHost::new();
                h.attach_pane_badge_tx(pane_badge_tx);
                h.attach_pane_wheel_keys_tx(pane_wheel_keys_tx);
                h.attach_pane_agent_tui_tx(pane_agent_tui_tx);
                h.attach_pane_render_markup_tx(pane_render_markup_tx);
                h.attach_pane_title_tx(pane_title_tx);
                h.attach_pane_session_begin_tx(pane_session_begin_tx);
                h.attach_pty_op_tx(pty_op_tx);
                h.attach_inject_input_tx(inject_input_tx);
                h.attach_pane_activity_tx(pane_activity_tx);
                h.attach_wake(wake_for_host);
                h
            },
            plugin_registry: PluginRegistry::new(),
            last_plugin_tick: Instant::now() - Duration::from_secs(1),
            pane_look: pane_decls::PaneLook::new(),
            pane_status: {
                // Idle is a property of the pane, not of this process:
                // pick up the clocks the previous image left so a
                // silent update doesn't tell every pane it just woke.
                let mut t = pane_status::PaneStateTracker::new();
                t.read_reports(|sid| control_reader::pane_wakes::latest(sid).map(|w| w.output_at));
                let n = t.load_clocks();
                if n > 0 {
                    lx_info!(
                        "shell.pane_state.clocks_restored",
                        "carried pane idle clocks across the restart",
                        panes = n
                    );
                }
                t
            },
            pane_activity_rx,
            // Mirror of `window-state.bin` as it was on disk when this
            // process started.  Load it BEFORE any window opens: the
            // first window's own save must not be allowed to shorten
            // the list past the windows this boot is about to restore.
            saved_window_frames: marspot::state::read_windows().unwrap_or_default(),
            pane_badge_rx,
            pane_wheel_keys_rx,
            pane_tables: pane_tables::PaneTables::default(),
            pane_agent_tui_rx,
            pane_render_markup_rx,
            pane_title_rx,
            core_icons: core_icons::CoreIcons::new(),
            pane_session_begin_rx,
            pty_op_rx,
            cli_rx,
            autorun_panes: cli_socket::load_autorun(),
            autorun_last_look: None,
            op_host: SupervisorOpHost { begin_tx: pane_session_begin_tx_for_ops },
            pty_ops: plugins::pty_op::PtyOps::new(
                std::sync::Arc::new(plugins::host::HostPtyIo::new(inject_input_tx_for_ops))
                    as std::sync::Arc<dyn plugins::pty_op::PtyIo>,
            )
            .with_receipts(Some(std::sync::Arc::clone(&receipts))),
            inject_input_rx,
            active_pane_sessions: std::collections::HashMap::new(),
        }
    }

    /// Step every pane's state machine (gated to
    /// `pane_status::SWEEP_INTERVAL` inside the tracker), publish the
    /// result for plugins, and log the committed transitions.
    /// How long between two prefetch wakes.
    ///
    /// One claude starting is a fork, an exec and a transcript read;
    /// sixteen of them together is a stall on the machine the user just
    /// came back to.  A second apart spreads a full window over the
    /// time it takes to read one pane, which is the window that matters
    /// — by the third click the third pane is already up.
    const WAKE_STAGGER: Duration = Duration::from_millis(1_000);

    /// Drain queued PaneSession take-over requests + drive each
    /// active session's on_tick.  Called by the supervisor poll
    /// alongside the regular plugin tick.
    fn process_pane_sessions(&mut self) {
        // Drain begin requests → register + emit PaneSessionBegin.
        while let Ok(req) = self.pane_session_begin_rx.try_recv() {
            if self.active_pane_sessions.contains_key(&req.shelld_session_id) {
                lx_warn!(
                    "shell.pane_session.duplicate_begin",
                    "rejecting duplicate PaneSession begin",
                    shelld_session_id = req.shelld_session_id,
                    plugin = req.plugin_name
                );
                continue;
            }
            let caps = req.session.caps();
            if let Some(conn) = self.active.as_ref() {
                conn.send(
                    MsgType::PaneSessionBegin,
                    marspot::shell_proto::encode_pane_session_begin(
                        req.shelld_session_id,
                        caps,
                    ),
                );
            }
            self.active_pane_sessions.insert(
                req.shelld_session_id,
                ActivePaneSession {
                    session: req.session,
                    plugin_name: req.plugin_name,
                },
            );
            lx_event!(
                "PANE_SESSION_BEGIN",
                "plugin took over pane",
                shelld_session_id = req.shelld_session_id,
                plugin = req.plugin_name,
                caps = caps
            );
        }
        // Tick every active session.  end_requested flag flushed after.
        let sids: Vec<u64> = self.active_pane_sessions.keys().copied().collect();
        for sid in sids {
            let end_flag = std::cell::Cell::new(false);
            let host = ConcretePaneSessionHost {
                sid,
                plugin_name: self.active_pane_sessions[&sid].plugin_name,
                plugin_host: &self.plugin_host,
                end_requested: &end_flag,
            };
            if let Some(active) = self.active_pane_sessions.get_mut(&sid) {
                active.session.on_tick(&host);
            }
            if end_flag.get() {
                self.end_pane_session(sid, plugins::EndReason::PluginRequested);
            }
        }
    }

    /// Every pane in `obs` as `(session id, cwd, title)`.  The cwd is
    /// read live off the pane's foreground program (the shell when it is
    /// in front) rather than taken from the
    /// registry: the registry's copy is from spawn time and says the
    /// launch directory for every pane that has since cd'd somewhere.
    /// Only a live session's shell is read: a dead one's pid may name
    /// any process by now.
    fn live_panes(obs: &observe::Observation) -> Vec<(u64, String, String)> {
        obs.sessions
            .iter()
            .map(|s| {
                let e = &s.entry;
                let cwd = s.shell.and_then(|id| marspot::pidtree::pane_cwd(id.pid));
                let cwd = cwd.map(|p| p.to_string_lossy().into_owned());
                (e.id, cwd.unwrap_or_else(|| e.cwd.clone()), e.title.clone())
            })
            .collect()
    }

    /// Every pane with what naming needs: its directory and its code.
    /// One place builds this, so the listing and `--send` cannot
    /// disagree about which pane a name means.
    fn pane_refs() -> Vec<marspot::pane_name::PaneRef> {
        Self::pane_refs_in(&observe::shared().fresh(observe::NOW))
    }

    /// `pane_refs` over a given observation.  May read the code file,
    /// so not for the main thread's hot paths.
    pub(crate) fn pane_refs_in(obs: &observe::Observation) -> Vec<marspot::pane_name::PaneRef> {
        Self::live_panes(obs)
            .into_iter()
            .map(|(sid, cwd, _)| marspot::pane_name::PaneRef::new(sid, cwd))
            .collect()
    }

    /// Every pane as `(id, cwd, address)` — name and cell both, so a
    /// listing answers "how do I say this one again?" without the
    /// reader having to work out the numbering.
    fn addressed_panes() -> Vec<(u64, String, String)> {
        let refs = Self::pane_refs();
        let named: std::collections::HashMap<u64, String> =
            cli_socket::assign_names(&refs).into_iter().collect();
        let cells = Self::pane_cells();
        refs.into_iter()
            .map(|marspot::pane_name::PaneRef { sid, cwd, .. }| {
                let name = named.get(&sid).cloned().unwrap_or_default();
                let addr = match cells.get(&sid) {
                    Some((w, x, y)) => format!("{name}  w({w},{x},{y})"),
                    None => name,
                };
                (sid, cwd, addr)
            })
            .collect()
    }

    /// `sid → (window, x, y)`, 1-based, from the layout L2 persists.
    fn pane_cells() -> std::collections::HashMap<u64, (usize, usize, usize)> {
        let mut out = std::collections::HashMap::new();
        let Some(saved) = marspot::state::read() else {
            return out;
        };
        for (wi, win) in saved.windows.iter().enumerate() {
            let cols = win.grid_cols.max(1) as usize;
            for (i, p) in win.panes.iter().enumerate() {
                if p.sid != 0 {
                    out.insert(p.sid, (wi + 1, i % cols + 1, i / cols + 1));
                }
            }
        }
        out
    }

    /// Switch the policy for a pane, or report where it is on.
    ///
    /// Stored by working directory: that is what "the pane for this
    /// project" means, and it is the one identity that survives a
    /// restart, a move between windows, and a twin appearing (which
    /// changes the pane's name).
    fn run_cli_autorun(&mut self, target: &str, on: Option<bool>) -> Result<String, String> {
        if target.trim().is_empty() {
            if self.autorun_panes.is_empty() {
                return Ok("autorun is off everywhere".into());
            }
            let mut on: Vec<&String> = self.autorun_panes.iter().collect();
            on.sort();
            return Ok(on
                .iter()
                .map(|cwd| format!("{cwd}\n"))
                .collect::<String>()
                .trim_end()
                .to_string());
        }
        let sid = self.resolve_target(target)?;
        let cwd = Self::live_panes(&observe::shared().fresh(observe::NOW))
            .into_iter()
            .find(|(s, _, _)| *s == sid)
            .map(|(_, cwd, _)| cwd)
            .ok_or_else(|| format!("pane {sid} is gone"))?;
        match on {
            Some(true) => {
                self.autorun_panes.insert(cwd.clone());
                let _ = cli_socket::save_autorun(&self.autorun_panes);
                Ok(format!("autorun on for {cwd} (pane {sid})"))
            }
            Some(false) => {
                self.autorun_panes.remove(&cwd);
                self.pane_tables.autorun_mem.remove(&sid);
                let _ = cli_socket::save_autorun(&self.autorun_panes);
                Ok(format!("autorun off for {cwd}"))
            }
            None => Ok(if self.autorun_panes.contains(&cwd) {
                format!("autorun is on for {cwd} (pane {sid})")
            } else {
                format!("autorun is off for {cwd}")
            }),
        }
    }

    /// How often the autorun policy looks at its panes.
    ///
    /// Deliberately slow.  Every trigger it can fire on requires the
    /// pane to have been quiet for longer than this already, so looking
    /// more often could only make it act sooner on evidence it should
    /// be sure of.
    const AUTORUN_INTERVAL: Duration = Duration::from_secs(30);

    /// Look at every pane the policy is on for, and act if it says to.
    fn sweep_autorun(&mut self) {
        if self.autorun_panes.is_empty() {
            return;
        }
        let now = Instant::now();
        if self
            .autorun_last_look
            .is_some_and(|t| now.duration_since(t) < Self::AUTORUN_INTERVAL)
        {
            return;
        }
        self.autorun_last_look = Some(now);

        // The state machine's own view — the same one the reclamation
        // policy acts on, so "quiet" means one thing in this process.
        let statuses = self.pane_status.snapshot(now);
        let wall = std::time::SystemTime::now();
        let panes: Vec<(u64, String)> = Self::live_panes(&observe::shared().latest())
            .into_iter()
            .filter(|(_, cwd, _)| self.autorun_panes.contains(cwd))
            .map(|(sid, cwd, _)| (sid, cwd))
            .collect();
        self.pane_tables.autorun_mem.retain(|sid, _| panes.iter().any(|(s, _)| s == sid));

        for (sid, cwd) in panes {
            let Some((status, _held, _quiescent)) = statuses.get(&sid) else { continue };
            // The screen, as a person would read it.  Cheap enough at
            // this cadence (one file tail + a replay) and the only way
            // to see what the session actually said.
            let screen = Self::read_pane_screen(sid).unwrap_or_default();
            let look = plugins::autorun::Look {
                // Exactly one state is a pane worth typing into: a
                // bound session that finished its turn.  Empty, parked,
                // contradictory and unknown are all quiet too, and
                // typing `/clear` at any of them is a shell command
                // that does not exist.
                awaiting_user: matches!(
                    status,
                    marspot::pane_state::PaneStatus::AwaitingUser
                ),
                // `Busy` covers both halves of "it is doing something":
                // a job the shell is holding, and a program that is
                // still drawing.  A rotation is not over while its
                // build is running, however quiet the terminal looks.
                work_in_flight: matches!(status, marspot::pane_state::PaneStatus::Busy(_)),
                screen: &screen,
            };
            let mem = self.pane_tables.autorun_mem.entry(sid).or_default();
            let was_exhausted = mem.is_exhausted();
            let (action, why) = plugins::autorun::decide(&look, mem, wall);
            if mem.is_exhausted() && !was_exhausted {
                lx_warn!(
                    "shell.autorun.exhausted",
                    "no response after every retry; leaving this pane alone",
                    shelld_session_id = sid,
                    cwd = cwd.as_str()
                );
            }
            if let Err(waiting) = why {
                // Say why, once per change of reason.  Without this the
                // log is silent until something fires, and "nothing
                // happened" reads the same whether the policy is
                // waiting or wedged.  That question came back on day
                // one — *why has torajs stopped?* — and there was
                // nothing to answer it with.
                let reason = format!("{waiting:?}");
                if self.pane_tables.autorun_why.get(&sid) != Some(&reason) {
                    lx_info!(
                        "shell.autorun.waiting",
                        &reason,
                        shelld_session_id = sid,
                        cwd = cwd.as_str()
                    );
                    self.pane_tables.autorun_why.insert(sid, reason);
                }
                continue;
            }
            self.pane_tables.autorun_why.remove(&sid);
            let lines: Vec<&str> = match action {
                plugins::autorun::Action::ClearAndContinue => {
                    vec!["/clear", plugins::autorun::CONTINUE_AUTORUN]
                }
                plugins::autorun::Action::Continue => vec![plugins::autorun::CONTINUE],
                // The line is already in the box; all it needs is the
                // Enter that went missing.
                plugins::autorun::Action::SubmitPending => vec![""],
                plugins::autorun::Action::Nothing => continue,
            };
            lx_info!(
                "shell.autorun.act",
                &format!("{why:?} → {lines:?}"),
                shelld_session_id = sid,
                cwd = cwd.as_str(),
                attempt = mem.attempts()
            );
            if let Err(e) = self.autorun_type(sid, &lines) {
                lx_warn!("shell.autorun.send_failed", &e, shelld_session_id = sid);
            }
        }
    }

    /// Type each line into the pane, in order, as one operation.
    ///
    /// One operation, not one per line: the queue serialises whole
    /// operations, so splitting them would let something else in
    /// between `/clear` and what follows it.  Each line waits for the
    /// pane to draw before the next goes in — `/clear` restarts the
    /// session's UI, and a line typed into that gap lands nowhere.
    fn autorun_type(&mut self, sid: u64, lines: &[&str]) -> Result<(), String> {
        if self.pty_ops.is_busy(sid) {
            return Err(format!(
                "pane {sid} is busy ({})",
                self.pty_ops.running(sid).unwrap_or("?")
            ));
        }
        let mut op = plugins::pty_op::PtyOp::new("autorun")
            // A delivery, not a takeover: the pane is not frozen and
            // the keyboard is not locked, so a person who walks up
            // mid-sequence keeps control of their own session.
            .lock_keys(false);
        for (i, line) in lines.iter().enumerate() {
            if line.is_empty() {
                // Just the Enter: the text is already in the box.
                op = op.step(plugins::pty_op::Step::send(b"\r".to_vec()).named("enter"));
                continue;
            }
            if i > 0 {
                // Let the previous line land and the UI settle before
                // the next one.  Typing `继续 autorun` into a session
                // that is still processing `/clear` loses it.
                op = op
                    .step(plugins::pty_op::Step::await_quiet(Duration::from_millis(700))
                        .timeout(Duration::from_secs(20)));
            }
            op = op
                .step(plugins::pty_op::Step::paste(*line))
                .step(plugins::pty_op::Step::send(b"\r".to_vec()).named("enter"));
        }
        self.pty_ops
            .submit(sid, op)
            .map(|_| ())
            .ok_or_else(|| format!("pane {sid} has too much queued"))
    }

    /// The pane's screen as it is showing it.
    fn read_pane_screen(sid: u64) -> Option<String> {
        marspot::pane_read::screen(sid, 0).ok()
    }

    /// Turn what a caller said into one pane.
    ///
    /// Three forms, because the useful address depends on what the
    /// caller knows: a **name** when it knows the project, an **id**
    /// when it was handed one, and a **cell** when it means "whatever
    /// is in that slot of that window".
    fn resolve_target(&self, target: &str) -> Result<u64, String> {
        let panes = Self::pane_refs();
        match cli_socket::parse_target(target)? {
            cli_socket::Target::Id(sid) => {
                if panes.iter().any(|p| p.sid == sid) {
                    Ok(sid)
                } else {
                    Err(format!("no pane with session id {sid}"))
                }
            }
            cli_socket::Target::Name(n) => cli_socket::resolve_name(&n, &panes),
            cli_socket::Target::Cell { w, x, y } => Self::pane_at_cell(w, x, y),
        }
    }

    /// The pane occupying window `w`'s cell `(x, y)`, all 1-based.
    ///
    /// Read from the layout L2 persists on every spawn / close / focus
    /// change / layout apply, which is the only place the arrangement
    /// is known — L1 owns the windows, L2 decides what sits where.
    fn pane_at_cell(w: usize, x: usize, y: usize) -> Result<u64, String> {
        let saved = marspot::state::read()
            .ok_or_else(|| "no window layout on disk yet".to_string())?;
        let win = saved
            .windows
            .get(w - 1)
            .ok_or_else(|| format!("there is no window {w} (there are {})", saved.windows.len()))?;
        let (cols, rows) = (win.grid_cols as usize, win.grid_rows as usize);
        if x > cols || y > rows {
            return Err(format!("window {w} is {cols}×{rows}; ({x},{y}) is outside it"));
        }
        let idx = (y - 1) * cols + (x - 1);
        match win.panes.get(idx) {
            Some(p) if p.sid != 0 => Ok(p.sid),
            _ => Err(format!("window {w} cell ({x},{y}) is empty")),
        }
    }

    /// What the pane called `target` says.
    ///
    /// Read from what the session has already left behind -- its
    /// published frame, and its bytelog for history -- rather than asked
    /// of L3: no round trip, no state, and a pane that is
    /// mid-reclamation answers as readily as one nobody has touched.
    fn run_cli_read(&self, target: &str, extra_lines: u32) -> Result<String, String> {
        let sid = self.resolve_target(target)?;
        marspot::pane_read::screen(sid, extra_lines.min(u16::MAX as u32) as u16)
            .map_err(|e| format!("cannot read pane {sid}: {e}"))
    }

    /// Type `text` into the pane called `target`, then Enter.
    ///
    /// The whole of "session-to-session communication" so far, and
    /// deliberately so: it is the same delivery every future protocol
    /// needs, and everything hard about it is already here — naming a
    /// pane that has no name, not colliding with whatever that pane is
    /// already doing, and putting multi-line text in front of a program
    /// without it being executed a line at a time.
    fn run_cli_send(&mut self, target: &str, text: &str) -> (bool, String) {
        let sid = match self.resolve_target(target) {
            Ok(sid) => sid,
            Err(e) => return (false, e),
        };
        if self.pty_ops.is_busy(sid) {
            // Refuse rather than queue: the caller is a person or a
            // program that wants to know its message went in *now*, and
            // a message that lands after a reclamation finishes has
            // landed somewhere else than the sender meant.
            return (
                false,
                format!(
                    "pane {sid} is busy ({})",
                    self.pty_ops.running(sid).unwrap_or("?")
                ),
            );
        }
        let op = plugins::pty_op::PtyOp::new("cli.send")
            // Nothing is frozen and no keys are locked: this is a
            // delivery, not a takeover.  The user can keep typing in
            // that pane while it happens.
            .lock_keys(false)
            .step(plugins::pty_op::Step::paste(text))
            // Enter separately, and *after* the paste has been handed
            // over: inside the pasted text it would be part of the
            // message, and bracketed paste is exactly the mechanism
            // that stops a newline from submitting.
            .step(plugins::pty_op::Step::send(b"\r".to_vec()).named("enter"));
        match self.pty_ops.submit(sid, op) {
            Some(id) => (true, format!("queued on pane {sid} (id={})", id.0)),
            None => (false, format!("pane {sid} has too much queued")),
        }
    }

    /// Hand a focus change to the pane's own session, if it has one.
    ///
    /// Mirrors `dispatch_pane_session_key` — same host, same
    /// end-on-request handling — because a restore that starts when
    /// the user looks at the pane is the difference between "it came
    /// back" and "it took forever".
    fn dispatch_pane_session_focus(&mut self, sid: u64) {
        let Some(active) = self.active_pane_sessions.get_mut(&sid) else {
            return;
        };
        let plugin_name = active.plugin_name;
        let end_flag = std::cell::Cell::new(false);
        let host = ConcretePaneSessionHost {
            sid,
            plugin_name,
            plugin_host: &self.plugin_host,
            end_requested: &end_flag,
        };
        active.session.on_focus(&host);
        if end_flag.get() {
            self.end_pane_session(sid, plugins::EndReason::PluginRequested);
        }
    }

    fn dispatch_pane_session_key(
        &mut self,
        sid: u64,
        ev: marspot::shell_proto::WireKeyEvent,
    ) {
        let Some(active) = self.active_pane_sessions.get_mut(&sid) else {
            return;
        };
        let plugin_name = active.plugin_name;
        let end_flag = std::cell::Cell::new(false);
        let host = ConcretePaneSessionHost {
            sid,
            plugin_name,
            plugin_host: &self.plugin_host,
            end_requested: &end_flag,
        };
        let handling = active.session.on_user_key(&host, &ev);
        if end_flag.get() || handling == plugins::KeyHandling::EndSession {
            self.end_pane_session(sid, plugins::EndReason::PluginRequested);
        }
    }

    fn end_pane_session(&mut self, sid: u64, reason: plugins::EndReason) {
        let Some(mut active) = self.active_pane_sessions.remove(&sid) else {
            return;
        };
        let plugin_name = active.plugin_name;
        // Read before `on_end` — that is where the hold is released,
        // and after it this session no longer speaks for the pane.
        let froze_picture = active.session.caps()
            & marspot::shell_proto::PANE_SESSION_CAP_FREEZE_GRID
            != 0;
        let end_flag = std::cell::Cell::new(false);
        let host = ConcretePaneSessionHost {
            sid,
            plugin_name,
            plugin_host: &self.plugin_host,
            end_requested: &end_flag,
        };
        active.session.on_end(&host, reason);
        // Tell L2 to leave the locked / frozen state.
        if let Some(conn) = self.active.as_ref() {
            conn.send(
                MsgType::PaneSessionEnd,
                marspot::shell_proto::encode_pane_session_end(sid),
            );
        }
        lx_event!(
            "PANE_SESSION_END",
            "pane session torn down",
            shelld_session_id = sid,
            plugin = plugin_name,
            reason = format!("{:?}", reason)
        );
        // The freeze just lifted.  `sweep_pane_status` held this pane's
        // brightness for as long as its picture was held, so the pane
        // is still wearing the level it was parked at — and the sweep
        // that would notice is up to a second away.  A second is long
        // enough to watch the content come back and the pane light up
        // afterwards as two separate events.
        //
        // Sweeping *here* rather than sending a level from the last
        // snapshot: that snapshot describes the parked pane (the
        // machines kept running while only the presentation was
        // frozen), so it would dim the pane at the exact moment its
        // program came back.  Stepping the machines first is what makes
        // the level true.
        if froze_picture {
            self.pane_status.force_next_sweep();
            self.sweep_pane_status();
        }
    }

}

/// Fold this process's live window frames into the list already on
/// disk: index *i* takes the live frame when there is one, otherwise
/// keeps whatever was saved there, and entries past the live windows
/// are preserved.
///
/// Both halves of that matter, and both were bugs:
///   - a live window with no frame yet (`None`) must NOT be skipped —
///     skipping shifts every later window's entry by one;
///   - the on-disk tail must NOT be truncated — during a cold-launch
///     restore those entries are the geometry the core is about to ask
///     for, and they are gone by the time it does.
fn merge_window_frames(
    prev: &[marspot::state::SavedWindow],
    live: &[Option<marspot::state::SavedWindow>],
) -> Vec<marspot::state::SavedWindow> {
    let mut out: Vec<marspot::state::SavedWindow> =
        Vec::with_capacity(prev.len().max(live.len()));
    for i in 0..prev.len().max(live.len()) {
        match live.get(i).and_then(|v| v.clone()) {
            Some(fresh) => out.push(fresh),
            // No live frame for this slot: keep the saved one.  With
            // nothing saved either (a window that somehow hasn't
            // reported its frame and has no history), write a zeroed
            // hole rather than skipping the slot — a skip would shift
            // every later entry, while a hole is rejected by the
            // restore path's own `w > 50 && h > 50` check and just
            // means "this one opens at the default rect".
            None => out.push(
                prev.get(i).cloned().unwrap_or(marspot::state::SavedWindow {
                    display_id: 0,
                    x: 0.0,
                    y: 0.0,
                    w: 0.0,
                    h: 0.0,
                }),
            ),
        }
    }
    out
}

/// Lay the open windows out by their own slot, ready to be merged
/// over what is already on disk.
///
/// Indexed by `frame_index`, not by position: a window that closed
/// leaves a gap here, and the gap is what keeps the windows that are
/// still open pointing at their own geometry.  Writing by position
/// meant closing the first of two windows moved the second into slot
/// 0 — so the two swapped frames on the next launch.
fn live_frames_by_slot(
    windows: impl Iterator<Item = (usize, Option<marspot::state::SavedWindow>)>,
    saved_len: usize,
) -> Vec<Option<marspot::state::SavedWindow>> {
    let mut out: Vec<Option<marspot::state::SavedWindow>> = vec![None; saved_len];
    for (slot, frame) in windows {
        if out.len() <= slot {
            out.resize(slot + 1, None);
        }
        out[slot] = frame;
    }
    out
}

#[cfg(test)]
mod window_frame_merge_tests;

impl ShellApp {
    /// F3+6.1 — read the NSWindow's current frame + screen, atomic-
    /// write to `window-state.bin` ONLY when the values changed since
    /// the last save.  Without dedup the `resized` callback fires
    /// 60+ times during a live drag = a write storm; tracking the
    /// last-saved tuple turns that into a single save per genuine
    /// change.  Frames compared at 1-pt granularity (sub-pt diffs
    /// from AppKit's internal float math get coalesced).
    fn save_window_state_if_changed(&mut self, ctx: &MarspotAppCtx) {
        let Some(wi) = self.window_index(ctx.window_id()) else { return };
        let (x, y, w, h) = ctx.window_frame_pt();
        let display_id = ctx.window_display_id().unwrap_or(0);
        let cur = (x.round(), y.round(), w.round(), h.round(), display_id);
        if self.windows[wi].last_saved_window == Some(cur) {
            return;
        }
        self.windows[wi].last_saved_window = Some(cur);
        self.save_window_frames();
    }

    /// RFC-005 step 6 — write every window's frame, in creation order,
    /// so entry *i* keeps pairing with window *i* of `shell-state.bin`.
    ///
    /// Merged against the previous list rather than replacing it.  The
    /// replacing version broke every silent update: an L1 self-execv
    /// tears down all the NSWindows, the successor opens window 0 (at
    /// the frame handed over in `MARSPOT_RESTORE_FRAME`) and saves — at
    /// which point `self.windows.len() == 1`, so the file was rewritten
    /// with one entry and window 1's geometry was deleted.  Moments
    /// later the core asked for window 1 to be reopened, the restore
    /// found no entry for index 1, and the window came back at the
    /// default 1200×800 rect.  `shell.window.restore_no_frame
    /// frame_index=1` was in the log for every single update.
    ///
    /// So: overwrite by index, never shorten.  A window that closed
    /// leaves its last geometry behind, which is harmless — the core
    /// drives restores from its own per-window layout records, and
    /// nothing asks for an index it has no window for.
    fn save_window_frames(&mut self) {
        let live = live_frames_by_slot(
            self.windows.iter().map(|w| {
                (
                    w.frame_index,
                    w.last_saved_window.map(|(x, y, ww, h, display_id)| {
                        marspot::state::SavedWindow { display_id, x, y, w: ww, h }
                    }),
                )
            }),
            self.saved_window_frames.len(),
        );
        let frames = merge_window_frames(&self.saved_window_frames, &live);
        if frames.is_empty() {
            return;
        }
        if let Err(e) = marspot::state::write_windows(&frames) {
            marspot::lx_warn!(
                "shell.window_state.write_failed",
                &format!("{e}")
            );
            return;
        }
        self.saved_window_frames = frames;
    }
}

impl MarspotApp for ShellApp {
    fn resumed(&mut self, ctx: &MarspotAppCtx) {
        let (w_phys, h_phys) = ctx.inner_size_phys();
        let scale = ctx.scale();
        let w_px = w_phys.max(64.0) as usize;
        let h_px = h_phys.max(64.0) as usize;

        // RFC-001 plugin bootstrap: register first-party plugins,
        // run init then start.  Plugins are passive until the core
        // has booted + sessions attached, so start is fine right
        // here — plugin tick is what drives per-pane work later.
        if self.plugin_registry.is_empty() {
            self.plugin_registry
                .register(Box::new(plugins::claudecode::ClaudecodePlugin::new()));
            // RFC-008 — the same pane affordances for the OpenAI CLI and
            // for Google's.  A pane runs one agent at a time, and each
            // plugin claims only the sessions whose process tree it
            // recognises, so all can be registered unconditionally.
            self.plugin_registry.register(Box::new(plugins::codex::CodexPlugin::new()));
            self.plugin_registry.register(Box::new(plugins::gemini::GeminiPlugin::new()));
            self.plugin_registry.register(Box::new(plugins::kimi::KimiPlugin::new()));
            // Codex panes as messaging peers of Claude Code sessions.
            self.plugin_registry.register(Box::new(plugins::peerbus::PeerbusPlugin::new()));
            // The account usage feed, when no external collector
            // writes one.
            self.plugin_registry
                .register(Box::new(plugins::usage::UsagePlugin::new()));
            self.plugin_registry.init_all_with(&self.plugin_host);
            self.plugin_registry.start_all_with(&self.plugin_host);
        }

        let pair = match SurfacePair::create(w_px, h_px) {
            Ok(p) => p,
            Err(e) => {
                lx_error!(
                    "shell.resumed.pair_create_failed",
                    &e.to_string(),
                    w = w_px,
                    h = h_px
                );
                ctx.exit();
                return;
            }
        };

        let (front_id, back_id) = pair.ids();
        // `resumed` fires for the boot window, which the app state
        // created before any callback could run.
        let wi = self.window_index(ctx.window_id()).unwrap_or(0);
        self.windows[wi].surfaces = Some(pair);
        self.windows[wi].layers = Some(FrameLayers::new(ctx.ns_view(), ctx.window_id(), scale));

        self.active = self.spawn_core(front_id, back_id, w_px, h_px, scale);
        if self.active.is_some() {
            self.record_core_boot();
        }
        self.start_redraw_pump();
        ctx.request_redraw();
        // Dev-only seam: open N extra windows exactly as Cmd-N does.
        // A keystroke cannot be delivered to the sandbox app from a
        // script, and the fresh-window path (1×1 grid, one brand-new
        // session) is otherwise untestable — the restore path covers
        // everything except that.  Unset in the installed app.
        if std::env::var("MARSPOT_DEV_CLOSE_EXTRA").is_ok() {
            self.dev_close_extra = true;
        }
        if let Ok(n) = std::env::var("MARSPOT_DEV_EXTRA_WINDOWS") {
            let n: usize = n.parse().unwrap_or(0);
            for _ in 0..n.min(8) {
                self.open_new_window();
            }
            if n > 0 {
                lx_event!(
                    "DEV_EXTRA_WINDOWS",
                    "opened extra windows on request (dev seam)",
                    n = n
                );
            }
        }
    }

    fn user_event(&mut self, ctx: &MarspotAppCtx) {
        // Drain both cores' inboxes — each reader thread calls
        // `proxy.wake()` after a push, so at least one message is ready.
        // Active and pending get separate routing: the active core
        // drives what's on screen (resize swap, caret, healthcheck);
        // the pending core only reports its probation progress.
        let active_inbox: Vec<ShellInbox> = match self.active.as_ref() {
            Some(conn) => conn.control_rx.try_iter().collect(),
            None => Vec::new(),
        };
        let owed = self.frame_owed();
        let knocked = self.knocked(&active_inbox);
        for msg in active_inbox {
            self.handle_active_msg(msg, ctx);
        }
        self.poll_supervisor(ctx);
        self.settle_pump(knocked, owed);
        self.dev_pass();
        ctx.request_redraw();
    }

    fn key_event(&mut self, ctx: &MarspotAppCtx, event: MarspotKeyEvent, mods: Modifiers) {
        // Forensic anchor for "key X did the wrong thing" reports.
        // We log the LogicalKey and the modifier byte; the actual PTY
        // bytes are encoded downstream in L3 (input_core::key_event_to_bytes)
        // and the existing `~/.marspot-trace` hook keeps capturing them
        // if it ever existed.  This line being at DEBUG means production
        // (default INFO) pays nothing; `MARSPOT_LOG_SHELL=debug` opens
        // the floodgate when investigating.  Mod byte format mirrors
        // shell_proto::struct_to_mods_byte: shift=1 ctrl=2 alt=4 super=8.
        lx_debug!(
            "input.key",
            "key event forwarded to core",
            logical = format!("{:?}", event.logical),
            mods_byte = struct_to_mods_byte(mods),
            state = format!("{:?}", event.state)
        );
        // The shell owns the windows, so the one command that opens
        // one runs here; every other key is the core's to dispatch.
        if event.state == marspot::input::KeyState::Pressed
            && let Some(row) = marspot::commands::for_key(&event, mods)
            && row.command == marspot::commands::Command::NewWindow
        {
            self.open_new_window();
            return;
        }
        let wire = event_to_wire(&event, mods);
        let w = Self::event_window(ctx);
        self.send(MsgType::KeyEvent, encode_key_event(&wire, w));
        marspot::latency_trace::mark(marspot::latency_trace::Stage::KeySent, 0);
    }

    fn mouse_down(&mut self, ctx: &MarspotAppCtx, x: f64, y: f64, mods: Modifiers) {
        let w = Self::event_window(ctx);
        self.send(
            MsgType::MouseDown,
            encode_mouse(x, y, struct_to_mods_byte(mods), w),
        );
    }

    fn mouse_right_down(
        &mut self,
        ctx: &MarspotAppCtx,
        x: f64,
        y: f64,
        mods: Modifiers,
    ) {
        let w = Self::event_window(ctx);
        self.send(
            MsgType::MouseRightDown,
            encode_mouse(x, y, struct_to_mods_byte(mods), w),
        );
    }

    fn mouse_drag(
        &mut self,
        ctx: &MarspotAppCtx,
        x: f64,
        y: f64,
        hover_window_id: u32,
        hover_x: f64,
        hover_y: f64,
    ) {
        // No modifier info on drag — pass zero; the renderer doesn't
        // currently need mods for drag-extend selection.
        let w = Self::event_window(ctx);
        self.send(
            MsgType::MouseDrag,
            marspot::shell_proto::encode_mouse_drag(
                x, y, 0, w, hover_window_id, hover_x, hover_y,
            ),
        );
    }

    fn mouse_up(
        &mut self,
        ctx: &MarspotAppCtx,
        x: f64,
        y: f64,
        drop_window_id: u32,
        drop_x: f64,
        drop_y: f64,
    ) {
        let w = Self::event_window(ctx);
        self.send(
            MsgType::MouseUp,
            marspot::shell_proto::encode_mouse_up(
                x, y, 0, w, drop_window_id, drop_x, drop_y,
            ),
        );
    }

    fn file_drop(&mut self, ctx: &MarspotAppCtx, x: f64, y: f64, paths: &[String]) {
        // L2 owns the pane layout — forward drop point + raw paths;
        // it hit-tests the pane and shell-quotes before insertion.
        let w = Self::event_window(ctx);
        self.send(MsgType::FileDrop, encode_file_drop(x, y, paths, w));
    }

    fn mouse_moved(&mut self, ctx: &MarspotAppCtx, x: f64, y: f64) {
        // Forwarded raw — L2 hit-tests against chrome rects and
        // ignores moves that don't change its hover region (cheap).
        let w = Self::event_window(ctx);
        self.send(MsgType::MouseMove, encode_mouse(x, y, 0, w));
    }

    fn scroll(&mut self, ctx: &MarspotAppCtx, dx: f64, dy: f64, precise: bool) {
        let w = Self::event_window(ctx);
        self.send(MsgType::Scroll, encode_scroll(dx, dy, precise, w));
    }

    /// The OS's own controls moved (full-screen transition) while the
    /// window's pixels did not.  One small frame, no surface churn —
    /// see `MsgType::WindowChrome`.
    fn chrome_changed(&mut self, ctx: &MarspotAppCtx) {
        self.send(
            MsgType::WindowChrome,
            encode_window_chrome(
                Self::event_window(ctx),
                ctx.traffic_lights_right_phys(),
            ),
        );
    }

    fn resized(&mut self, ctx: &MarspotAppCtx, w_phys: f64, h_phys: f64) {
        // F3+6.1 — persist window frame on every resize step.  Atomic
        // rename means a live drag can fire 60+ saves/s and the file
        // is always valid; the cost (~50us memcpy + 1 syscall) is
        // well below the per-frame resize budget.
        self.save_window_state_if_changed(ctx);
        let Some(wi) = self.window_index(ctx.window_id()) else { return };
        let scale = ctx.scale();
        let w_px = w_phys.max(64.0) as usize;
        let h_px = h_phys.max(64.0) as usize;
        let geom = AttachedGeometry {
            w_px,
            h_px,
            scale,
            lights_right_phys: ctx.traffic_lights_right_phys(),
        };
        // What the notification is worth is decided before any of it is
        // acted on.  AppKit sends these in streams — 2026-09-27, ~144 a
        // second for a window nobody was resizing — and the old code
        // did the whole resize for each: a present, a fresh IOSurface
        // pair, two more Metal textures in the core, and a presenter
        // pointed at a surface the core had only just started painting.
        // That last one is a window flickering between two states, and
        // it was reported as exactly that.
        let work = resize_work(self.windows[wi].last_attached, &geom);
        if work != ResizeWork::Attach {
            self.windows[wi].skipped_resizes += 1;
            if self.windows[wi].skipped_resizes == 1 {
                lx_info!(
                    "shell.resize.no_new_geometry",
                    "resize notification with nothing new in it — no surface churn",
                    window_id = self.windows[wi].window_id,
                    w_px = w_px as u64,
                    h_px = h_px as u64
                );
            }
            if work == ResizeWork::ChromeOnly {
                // The buttons moved without the pixels (a full-screen
                // transition): one small frame, and a redraw to show it.
                self.windows[wi].last_attached = Some(geom);
                self.chrome_changed(ctx);
                ctx.request_redraw();
            }
            return;
        }
        // Read the gate before taking `&mut` on the same window.
        let ready = self.windows[wi].first_frame_ready;
        if let Some(l) = self.windows[wi].layers.as_mut() {
            l.resized(w_phys, h_phys, scale);
            // A copying presenter draws *synchronously* inside the
            // resize callback, so its drawable lands in the same
            // CATransaction AppKit is about to commit for the new
            // bounds: the window edge and the contents move together.
            if ready && let Some(p) = l.copying() {
                p.present();
            }
        }
        // Fire the IOSurface handoff *immediately* (no debounce).
        // With presents-with-transaction the swap between old and new
        // IOSurface lands in the same CATransaction as the window
        // resize, so the visual stays stable while the terminal grid
        // actually reflows.  Without this, the pane grid never gets
        // SIGWINCH during a drag and lines wrap at the old column
        // count — what the user observed as "换行没跟上".
        let skipped = std::mem::take(&mut self.windows[wi].skipped_resizes);
        match self.fresh_pending_pair(wi, w_px, h_px) {
            Ok((f, b)) => {
                self.windows[wi].last_attached = Some(geom);
                lx_info!(
                    "shell.resize.attached",
                    "new pair for a window whose pixels moved",
                    window_id = self.windows[wi].window_id,
                    w_px = w_px as u64,
                    h_px = h_px as u64,
                    skipped_since_last = skipped as u64
                );
                self.send_surface_attach(
                    Self::event_window(ctx), f, b, w_phys, h_phys, scale,
                    ctx.traffic_lights_right_phys(),
                );
            }
            Err(e) => {
                lx_error!("shell.resize.pair_create_failed", &e.to_string());
            }
        }
        ctx.request_redraw();
    }

    fn moved(&mut self, ctx: &MarspotAppCtx) {
        // F3+6.1 — drag-end has no explicit callback in AppKit's
        // delegate vocabulary; `windowDidMove:` fires per-step during
        // the drag instead.  Each step writes the bin — atomic rename
        // means the file is always consistent and the cost is cheap.
        self.save_window_state_if_changed(ctx);
    }

    fn focused(&mut self, _ctx: &MarspotAppCtx, focused: bool) {
        self.send(MsgType::Focus, encode_focus(focused));
        self.proxy.wake(); // a pass now: what is queued below is driven by the clock
        let returning = focused && !self.app_focused;
        self.app_focused = focused;
        if returning {
            self.queue_parked_wakes();
        }
        // Silent-update trigger: the user just left marspot's window
        // (cmd-tab, click on another app, minimise).  If a pending
        // binary is staged in `binaries/pending/`, this is the
        // cheapest moment to swap — they're not watching us repaint.
        // The shell window stays put through the swap, the new core
        // attaches to the same IOSurface and shelld session, so when
        // they come back they see the same content rendered by the
        // new version's renderer.
        //
        // Skipped while a probe is in flight: one staged binary at a
        // time, and the probe already decided what happens next.
        if !focused && matches!(self.sup_state, SupervisorState::Idle)
            && std::env::var_os("MARSPOT_MANUAL_UPDATE_ONLY").is_none() {
                self.apply_pending_update();
            }
    }

    fn ime_preedit_changed(&mut self, ctx: &MarspotAppCtx, text: &str) {
        let w = Self::event_window(ctx);
        self.send(MsgType::Preedit, encode_preedit(text, w));
    }

    fn window_opened(&mut self, ctx: &MarspotAppCtx) {
        // The window exists now, so its own metrics are readable — a
        // window opened on another display can have a different
        // backing scale, and sizing its pair off the key window would
        // hand it a mismatched surface.
        let window_id = ctx.window_id();
        let (w_phys, h_phys) = ctx.inner_size_phys();
        let scale = ctx.scale();
        let pair = match SurfacePair::create(
            w_phys.max(64.0) as usize,
            h_phys.max(64.0) as usize,
        ) {
            Ok(p) => p,
            Err(e) => {
                lx_error!("shell.window.pair_create_failed", &e.to_string());
                marspot::app::close_window(window_id);
                return;
            }
        };
        let (f, b) = pair.ids();
        let slot = self
            .pending_frame_index
            .remove(&window_id)
            .unwrap_or_else(|| self.next_frame_index());
        let mut win = ShellWindow::new(window_id, slot);
        // A window without this shows nothing, however faithfully the
        // core paints (2026-07-28: Cmd-N opened a black rectangle).
        win.layers = Some(FrameLayers::new(ctx.ns_view(), window_id, scale));
        lx_event!(
            "WINDOW_PRESENTER_READY",
            "layers attached to the new window's view",
            window_id = window_id
        );
        win.pending_surfaces = Some(pair);
        // Seed the geometry cache from the window as it actually
        // opened.  `window-state.bin` is a list written whole on every
        // change, so a window with no cached frame would leave a hole
        // in it and shift every later window's entry.
        let (fx, fy, fw, fh) = ctx.window_frame_pt();
        win.last_saved_window = Some((
            fx.round(),
            fy.round(),
            fw.round(),
            fh.round(),
            ctx.window_display_id().unwrap_or(0),
        ));
        if self.dev_close_extra && window_id != marspot::shell_proto::FIRST_WINDOW_ID {
            win.dev_close_after_paint = true;
        }
        self.windows.push(win);
        self.save_window_frames();
        // A `SurfaceAttachWindow` naming a window the core has not seen
        // IS that window's birth event — there is no separate create
        // frame.
        self.send_surface_attach(
            window_id, f, b, w_phys, h_phys, scale,
            ctx.traffic_lights_right_phys(),
        );
        lx_event!(
            "WINDOW_OPENED",
            "new window announced to core",
            window_id = window_id,
            windows = self.windows.len()
        );
    }

    fn quit_requested(&mut self, ctx: &MarspotAppCtx) {
        // Quit means the app.  It arrives with whichever window is
        // still open, and reading it as "close that window" is how
        // quitting from the dock used to leave every other window on
        // screen (2026-09-28 report).
        self.quit(ctx);
    }

    fn close_requested(&mut self, ctx: &MarspotAppCtx) {
        // RFC-005 — closing one of several windows closes THAT window
        // and the panes it holds; only the last window closing is a
        // quit.  Cmd-Q is `quit_requested`, not this.
        let window_id = Self::event_window(ctx);
        if self.another_window_than(window_id) {
            // The red button / Cmd-W: putting a window away, not
            // dismantling it.  Its slot is kept so the layout and
            // the geometry both come back on the next launch.
            self.close_window_by_id(window_id, false);
            return;
        }
        self.quit(ctx);
    }

    fn redraw(&mut self, _ctx: &MarspotAppCtx) {
        self.present_windows();
    }
}

fn main() {
    // the child that checks the registration: one event and out
    if std::env::args().nth(1).as_deref() == Some(apple_events::CHECK_ARG) {
        std::process::exit(apple_events::check_main(std::env::args().nth(2).as_deref()));
    }
    // before anything else: AppKit must not register this image first
    apple_events::settle_after_exec();
    // before anything reads or writes the state directory
    if std::env::args().nth(1).as_deref() == Some(identity::LS_REFRESH_ARG) {
        identity::ls_refresh_main();
    }
    // RFC-004 D.1 — one-time Caches → Application Support state-root
    // migration.  Must run before logx / any path computation.
    marspot::paths::migrate_legacy_state_root();
    // The claudecode status-line hook dispatches before everything
    // else — before logx, before the redirect into current/.  Claude
    // runs it on every render of every cc pane, so it stays a
    // read-stdin-write-one-file process: no log stream to open, no
    // supervisor bookkeeping, no window.  It needs the migration
    // above only so it computes the same state root the badge reads.
    match std::env::args().nth(1).as_deref() {
        Some("--cc-statusline") => std::process::exit(plugins::claudecode::statusline_ingest()),
        // A pane's agent reporting that it submitted something, for the
        // same reasons and with the same shape: stdin in, one socket out,
        // no log stream and no window.  See `receipts`.
        Some("--submit-receipt") => std::process::exit(receipts::report_from_stdin()),
        _ => {}
    }
    // Make `println!` to a closed pipe (e.g. `marspot-shell --status |
    // head`) exit cleanly with the standard EPIPE convention instead
    // of panicking with a Rust backtrace.  SIG_DFL on macOS for SIGPIPE
    // terminates the process — that's exactly the Unix contract.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    // Supervisor and shell events go to the shared marspot.log as
    // component="shell"; idempotent across self-exec restarts.
    marspot::logx::init("shell");
    marspot_term::signals::log_fatal_senders("L1_SIGNAL");
    apple_events::report();

    // Rollback subcommands dispatch BEFORE the redirect: when
    // current/ is broken, exec'ing into it would eat the command.
    match std::env::args().nth(1).as_deref() {
        Some("--rollback-shell") => std::process::exit(cmd_rollback("shell")),
        Some("--rollback-core") => std::process::exit(cmd_rollback("core")),
        _ => {}
    }

    // One shell per state dir, settled before the redirect into
    // `binaries/current/`: a launch that loses never execs and never
    // reaches the launch journal the crash-loop checks count from.
    // Subcommands talk to the running shell and take no lock.
    if !std::env::args().nth(1).is_some_and(|a| a.starts_with('-')) {
        identity::settle();
        instance::hold_or_exit();
    }
    maybe_redirect_to_current_shell();
    if std::env::var_os("MARSPOT_SHELL_SELF_UPDATE").is_some() {
        // Take it out of the environment before anything else is
        // spawned.  Children inherit it, and every child that runs
        // this binary logged the line again: 38 of them in a day
        // against 6 real updates, one a minute, and the log read as if
        // L1 were re-execing itself all day (2026-09-28 — it sent this
        // author chasing a self-update that never happened).
        // SAFETY: single-threaded, before any thread is started.
        unsafe { std::env::remove_var("MARSPOT_SHELL_SELF_UPDATE") };
        sup_log::log("SHELL_SELF_UPDATE", "new shell exec'd from current/");
        // Self-fire SIGUSR1 so the main loop, once it lands on Idle
        // after the post-execv boot dance (active L2 spawned +
        // HELLO_ACK + first SurfaceReady), proactively applies any
        // remaining pending/marspot-core + pending/marspot-session.
        // Without this, the just-finished L1 swap leaves L2/L3
        // pending/ sitting there until install-local's 8 s fallback
        // retrigger or the next user-initiated focus-loss — that's
        // the "L1 flashes, then several seconds before content
        // updates and input wakes up" wait the user sees.
        // (install-local DID stage all three together as one
        // operation; the post-execv shell should treat them as one
        // operation too.)
        SIGUSR1_FLAG.store(true, Ordering::Release);
    }

    // Dispatch CLI subcommands before we touch AppKit / start a
    // window.  --status / --trigger run against an already-running
    // shell, so they must not themselves take over the run loop.
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--version") => {
            print_version();
            return;
        }
        Some("--status") => {
            print_status();
            return;
        }
        Some("--trigger") => {
            let code = cmd_trigger();
            std::process::exit(code);
        }
        Some("--panes") => {
            match cli_socket::list_panes() {
                Ok(mut panes) => {
                    // Sorted by address, not by whatever order the
                    // registry happened to be in: a list you read is a
                    // list you scan.
                    panes.sort_by(|a, b| a.2.cmp(&b.2));
                    println!("{:>6}  {:<34}  directory", "id", "address");
                    for (sid, cwd, addr) in panes {
                        println!("{sid:>6}  {addr:<34}  {cwd}");
                    }
                }
                Err(e) => {
                    eprintln!("cannot reach the running shell: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("--autorun") => {
            // `--autorun` alone lists; `--autorun <pane> [on|off]`
            // switches or asks about one.
            let target = args.get(2).cloned().unwrap_or_default();
            let on = match args.get(3).map(String::as_str) {
                Some("on") => Some(true),
                Some("off") => Some(false),
                None => None,
                Some(other) => {
                    eprintln!("unknown mode {other:?}; use on or off");
                    std::process::exit(2);
                }
            };
            match cli_socket::autorun(&target, on) {
                Ok(Ok(text)) => println!("{text}"),
                Ok(Err(msg)) => {
                    eprintln!("{msg}");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("cannot reach the running shell: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("--read") => {
            // `marspot-shell --read <pane> [-n <lines>]` — the pane's
            // screen, plus that many lines of what scrolled off it.
            let target = args.get(2).cloned().unwrap_or_default();
            let extra = args
                .iter()
                .position(|a| a == "-n")
                .and_then(|i| args.get(i + 1))
                .and_then(|n| n.parse::<u32>().ok())
                .unwrap_or(0);
            if target.is_empty() {
                eprintln!("usage: marspot-shell --read <pane> [-n <lines>]");
                std::process::exit(2);
            }
            match cli_socket::read_pane(&target, extra) {
                Ok(Ok(text)) => println!("{text}"),
                Ok(Err(msg)) => {
                    eprintln!("{msg}");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("cannot reach the running shell: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("--send") => {
            // `marspot-shell --send <pane> <text…>` — the rest of argv
            // is the message, joined with spaces, so it can be written
            // without quoting in the common case.
            let target = args.get(2).cloned().unwrap_or_default();
            let text = args[3.min(args.len())..].join(" ");
            if target.is_empty() || text.is_empty() {
                eprintln!("usage: marspot-shell --send <pane> <text…>");
                std::process::exit(2);
            }
            match cli_socket::send_text(&target, &text) {
                Ok((true, msg)) => {
                    println!("{msg}");
                }
                Ok((false, msg)) => {
                    eprintln!("{msg}");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("cannot reach the running shell: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("--help") | Some("-h") => {
            println!(
"marspot-shell — supervisor for the marspot terminal.\n\
\n\
Usage:\n\
  marspot-shell                Start the supervisor (window + core).\n\
  marspot-shell --version      Print version / git / build info.\n\
  marspot-shell --status       Summarise state from supervisor.log + live PIDs.\n\
  marspot-shell --panes        List panes: session id, name, working directory.\n\
  marspot-shell --autorun [<pane> [on|off]]\n\
                               Keep a rotation going by itself: when the session\n\
                               says it is done, send /clear then 继续 autorun;\n\
                               when it stalls on a server error, nudge it with\n\
                               继续, backing off each time.  No argument lists\n\
                               the panes it is on for.\n\
  marspot-shell --read <pane> [-n <lines>]\n\
                               Print what the pane says: its screen, plus that\n\
                               many lines of what has scrolled off it.\n\
  marspot-shell --send <pane> <text…>\n\
                               Type text into a pane and press Enter.  The pane\n\
                               is named by its working directory's last component\n\
                               (`spg`), its full name (`spg#3381`), its agent and\n\
                               name (`claude:spg`), or a session id.  A name that\n\
                               fits several panes is refused with their names.\n\
  marspot-shell --trigger      Apply a staged pending update on a running shell\n\
                               (sends SIGUSR1 to the supervisor process).\n\
  marspot-shell --rollback-shell   Quarantine current/marspot-shell, restore prev/.\n\
  marspot-shell --rollback-core    Quarantine current/marspot-core, restore prev/.\n\
                               Both run offline — restart Marspot afterwards.\n"
            );
            return;
        }
        // An unrecognised option is a mistake, not a launch.
        //
        // Falling through to the GUI start meant `marspot-shell
        // --typo` opened a window; harmless enough for a typo, and
        // not at all harmless for a Claude Code status-line hook,
        // which runs on every render — pointed at a binary too old to
        // know `--cc-statusline`, that is one window per render.
        // (Measured: an 0.7.82 shell handed the flag came up as a
        // full supervisor — shell.pid, control socket, session dir —
        // and had to be killed.)  A bare `marspot-shell` is still the
        // GUI launch, and positional arguments still fall through.
        Some(a) if a.starts_with('-') => {
            eprintln!("marspot-shell: unknown option `{a}` — see --help");
            std::process::exit(2);
        }
        _ => {}
    }

    if resident::asked() {
        ownership::hand_over_if_outside(true);
        resident::run();
    }
    ownership::hand_over_if_outside(false);
    ownership::announce();
    if resident::update_was_waiting() {
        SIGUSR1_FLAG.store(true, Ordering::Release);
    }
    identity::take_coalition(SIGUSR1_FLAG.load(Ordering::Acquire));

    // The default SIGPIPE above is for the CLI's stdout.  The GUI shell
    // writes to its core's socket, and a core that just died makes the
    // next write a SIGPIPE: with the default action that ended the whole
    // app silently before the supervisor could see the core was gone and
    // start another.  Ignored, the write fails with EPIPE instead.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    // Same reasoning as L2's loop: this thread runs the supervisor\n    // tick and every AppKit callback, and it was measured 100.8 s\n    // late during the 2026-08-11 incident.
    // Measured on an idle bench host with background load as the only
    // variable: at default QoS a frame's p99 went from 293 µs (idle)
    // to 6,082 µs (load 11), while the GPU's own account of the same
    // frame never moved (111 → 115 µs).  Nothing got heavier; this
    // thread simply stopped being scheduled.  Raising it to
    // USER_INTERACTIVE, same machine and load, seconds apart, both
    // orderings: p99 325 µs — the tail is gone.  See `marspot::qos`.
    let qos_ok = marspot::qos::raise_current_thread_to_user_interactive();
    lx_event!(
        "STARTUP",
        "marspot-shell starting",
        qos_user_interactive = qos_ok as u32,
        version_shell = env!("MARSPOT_VERSION_SHELL"),
        version_core = env!("MARSPOT_VERSION_CORE"),
        git = option_env!("MARSPOT_GIT_SHA").unwrap_or("unknown"),
        build_ts = option_env!("MARSPOT_BUILD_TS").unwrap_or("unknown"),
        pid = std::process::id()
    );
    sup_log::log(
        "STARTUP",
        &format!(
            "shell={} core={} git={} pid={}",
            env!("MARSPOT_VERSION_SHELL"),
            env!("MARSPOT_VERSION_CORE"),
            option_env!("MARSPOT_GIT_SHA").unwrap_or("unknown"),
            std::process::id()
        ),
    );
    // 2026-07-28 — crash-loop brake.  Runs before anything expensive
    // (updater thread, AppKit, core spawn): if this launch is part of
    // a rapid-restart burst, sleep first so the burst cannot saturate
    // the machine, and past the safe-mode threshold flag the whole
    // boot as spawn-nothing-fresh (`MARSPOT_SAFE_MODE`, read by this
    // process AND inherited by the core it spawns).
    match assess_launch_history(&read_launch_stamps(), now_unix_f64()) {
        LaunchVerdict::Normal => {}
        LaunchVerdict::Backoff(secs) => {
            lx_warn!(
                "shell.crash_loop.backoff",
                "rapid relaunches detected — sleeping before boot",
                sleep_s = secs
            );
            std::thread::sleep(std::time::Duration::from_secs(secs));
        }
        LaunchVerdict::SafeMode(secs) => {
            lx_event!(
                "SHELL_SAFE_MODE",
                "crash loop detected — this boot reattaches but spawns nothing fresh",
                sleep_s = secs
            );
            sup_log::log("SHELL_SAFE_MODE", "crash loop — safe-mode boot");
            // SAFETY: startup path in main(), before any thread is
            // spawned; children inherit it.
            unsafe { std::env::set_var("MARSPOT_SAFE_MODE", "1") };
            std::thread::sleep(std::time::Duration::from_secs(secs));
        }
    }
    install_sigusr1_handler();
    // Record our pid for this state dir so `--trigger` / dev-push /
    // install-local signal THIS shell, not whichever marspot-shell
    // `ps` lists first.  execv on self-update keeps the same pid, so
    // the file stays valid across a shell swap.
    write_shell_pid();
    // The silent-update poller: downloads new releases into
    // `binaries/pending/` in the background, for as long as this is kept.
    // The product version, not a layer's build number: the updater
    // compares against a release tag.
    let updater = marspot::updater::spawn(marspot::PRODUCT_VERSION.to_string());

    // Frame restore: the predecessor shell (self-update execv) hands
    // its exact window frame over via env so this process reopens in
    // place instead of jumping to the default rect.  Consumed here —
    // removed from our env so spawned children (core) don't carry it.
    let restore_frame = std::env::var("MARSPOT_RESTORE_FRAME").ok().and_then(|s| {
        // SAFETY: startup path in main(), before any thread is spawned.
        unsafe { std::env::remove_var("MARSPOT_RESTORE_FRAME") };
        supervisor::parse_restore_frame(&s)
    });
    // F3+6.1 — fall back to the persisted window-state.bin when the
    // env-var path (used by L1 self-execv to round-trip through itself)
    // didn't carry a frame.  Sane bounds: w/h > 50 pt guards against a
    // corrupt file shrinking the window to a sliver.
    let restore_frame = restore_frame.or_else(|| {
        // The boot window is entry 0 of the list; the rest are applied
        // as their windows are reopened.
        let w = marspot::state::read_windows()?.into_iter().next()?;
        if w.w > 50.0 && w.h > 50.0 { Some((w.x, w.y, w.w, w.h)) } else { None }
    });
    // With nothing to restore this is a first run — or the launch
    // after the user closed the last pane of the last window, which
    // deletes both saved files.  A modest window centred on the main
    // screen is the right thing to hand someone with no layout of
    // their own yet; `run_app` centres it because there is no frame.
    let (boot_w, boot_h) = match restore_frame {
        Some(_) => (DEFAULT_W_PT, DEFAULT_H_PT),
        None => (FRESH_W_PT, FRESH_H_PT),
    };
    let attrs = WindowAttrs {
        title: DEFAULT_TITLE.to_string(),
        width_logical: boot_w,
        height_logical: boot_h,
        frame_pt: restore_frame,
        // Terminal-black from the first paint: a core (L2) swap that
        // briefly uncovers the layer must show steady black, never a
        // lighter chrome flash.
        bg: theme_follow::at_launch(),
    };
    let proxy = EventProxy::new();
    let app = ShellApp::new(proxy.clone());
    // in a pool that ends with the app: what AppKit was handed before
    // its loop ran (the boot window, its view, the layers and surfaces
    // behind them) is otherwise kept for the life of the process, and
    // the process may now outlive the app by days
    objc2::rc::autoreleasepool(|_| run_app(app, proxy, attrs));
    // what only the open app needs: the release poll, the table of the
    // host's processes, the megabyte the quit read their arguments into
    drop(updater);
    observe::put_away();
    marspot::pidtree::release_room();
    if resident::staying() {
        resident::run_in_place();
    }
}

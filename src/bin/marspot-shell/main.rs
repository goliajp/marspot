// CARVE-OUT: holds the ShellApp state, the supervisor's constants and inbox, and main(), whose ~390-line launch sequence keeps it over the limit; shrink on every touch
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
mod pane_sessions;
mod autorun_cli;
mod pane_cli;
mod shell_new;
mod window_frames;
mod app_events;
mod core_conn;
mod shell_window;
mod pane_upkeep;
mod supervisor_tick;
use banner::BannerKind;
use pane_sessions::{ActivePaneSession, SupervisorOpHost};
use control_reader::control_reader_loop;
use launch::*;
use plugins::host::ShellPluginHost;
use plugins::PluginRegistry;
use frame_layer::FrameLayers;
use surfaces::SurfacePair;
use core_conn::CoreConn;
use shell_window::{resize_work, AttachedGeometry, ResizeWork, ShellWindow};
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

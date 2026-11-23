//! The shell's whole state: its windows, the live core, the supervisor's
//! bookkeeping, the plugin host and the channels the main loop drains.

use super::*;

pub(crate) struct ShellApp {
    pub(crate) proxy: EventProxy,
    /// One entry per open native window, in creation order.  Never
    /// empty while the app runs; `windows[0]` is the boot window.
    pub(crate) windows: Vec<ShellWindow>,
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
    pub(crate) wake_queue: std::collections::VecDeque<u64>,
    /// When the next entry of `wake_queue` may go.
    pub(crate) wake_next_at: Option<Instant>,
    /// Was the app focused at the last `focused` callback?  The queue
    /// is filled on the *edge* — a click inside an already-focused
    /// window is not a return.
    pub(crate) app_focused: bool,
    /// Slot promised to a window that has been asked for but has not
    /// opened yet, keyed by the id it will carry.  A restore knows its
    /// slot (the core named it); a Cmd-N window takes the next free
    /// one.  Consumed by `window_opened`.
    pub(crate) pending_frame_index: std::collections::HashMap<u32, usize>,
    /// Next id to hand a window.  Monotonic and never reused within a
    /// run: the core keys `WindowState` off it, and a recycled id
    /// would let a closed window's frames land in its successor.
    ///
    pub(crate) next_window_id: u32,
    /// The live core: child process, control socket (both directions),
    /// and liveness-handshake state aggregated into `CoreConn`.
    /// `None` before the first spawn and in the brief gap between a
    /// crash (or single-core swap retirement) and the respawn.
    pub(crate) active: Option<CoreConn>,
    pub(crate) pump: Option<pump::Pump>,
    /// Binary slot manager: current / prev / pending.  Used to find
    /// the core binary at spawn time and to atomic-swap when a
    /// silent update fires.
    pub(crate) binaries: BinaryTree,
    /// Where in the silent-update lifecycle we are.  `Idle` most of
    /// the time; flips to `Probing` while a staged binary's Gatekeeper
    /// verdict is being warmed on a background thread.
    pub(crate) sup_state: SupervisorState,
    /// Receives the verdict from the in-flight probe thread.  `Some`
    /// exactly while `sup_state` is `Probing`; the layer being probed
    /// rides in the state, so the channel only carries the outcome.
    pub(crate) probe_rx: Option<mpsc::Receiver<ProbeVerdict>>,
    /// Recent crash timestamps inside the `CRASH_WINDOW` rolling
    /// window.  Used to refuse auto-restart on a binary that's
    /// flapping.
    pub(crate) crashes: std::collections::VecDeque<Instant>,
    /// True if the crash budget has been blown.  We stop trying to
    /// restart until the cool-down below expires.
    pub(crate) auto_restart_disabled: bool,
    /// When the budget tripped, and how many times it has tripped
    /// this boot.  Together they set the cool-down before we try
    /// again — because "stop restarting" used to mean *forever*, and
    /// a load storm that passed in ninety seconds left the window
    /// frozen behind a banner until the user noticed and quit the
    /// app.  Every extra trip doubles the wait, so a genuinely broken
    /// binary still can't loop.
    pub(crate) disabled_at: Option<Instant>,
    pub(crate) budget_trips: u32,
    /// When `poll_supervisor` last ran — the witness for
    /// [`SUPERVISOR_STALL_GAP`].  `None` until the first tick: the
    /// sixteen seconds between constructing this struct and the first
    /// supervisor pass are the shell *booting*, not the supervisor
    /// falling behind, and logging that as a stall would leave a
    /// false witness in every log for anyone who later reads one.
    pub(crate) last_tick_at: Option<Instant>,
    /// Currently-displayed banner, or `None` for clear.  Kept on
    /// the shell so `poll_supervisor` can recompute it from state
    /// transitions and call `presenter.set_banner` only when it
    /// actually changes.
    pub(crate) banner_kind: Option<banner::Shown>,
    /// This boot came up under the crash-loop brake: reattach only,
    /// spawn nothing fresh, restore no extra windows.  Read once from
    /// `MARSPOT_SAFE_MODE` (set in `main`, inherited by the core).
    pub(crate) safe_mode: bool,
    /// Dev seam only (`MARSPOT_DEV_CLOSE_EXTRA`): close each extra
    /// window again once it has painted.  A script cannot deliver the
    /// red button or Cmd-W to the sandbox app, and closing a second
    /// window is what crashed the app on 2026-07-28 — so the path has
    /// to be reachable from a test somehow.  Unset in the installed
    /// app.
    pub(crate) dev_close_extra: bool,
    /// Dev seam only (`MARSPOT_DEV_CLOSE_SEQUENCE=2,1`): press the red
    /// button on these windows in this order, one every couple of
    /// seconds, once they have all painted.  Exists because "which
    /// window you closed first changed what survived" is the shape of
    /// the 2026-08-03 report, and the only way to reproduce it is to
    /// actually close windows in a given order and quit.  Unset in the
    /// installed app.
    pub(crate) dev_close_sequence: std::collections::VecDeque<u32>,
    /// Dev seam (`MARSPOT_DEV_BADGE_MENU=<path>`): a file a test drops
    /// `<sid> <tag>` into to pick one badge-menu entry, as if the
    /// pointer had.  A file rather than an env value because the pick
    /// only means anything once the pane's agent is up, which is
    /// minutes of shell activity after boot — and a script cannot move
    /// the pointer in the sandbox app.  Unset in the installed app.
    pub(crate) dev_badge_menu_file: Option<std::path::PathBuf>,
    /// When the next entry of `dev_close_sequence` is due.  `None`
    /// until every window named in it is up.
    pub(crate) dev_close_next_at: Option<Instant>,
    /// Dev seam only (`MARSPOT_DEV_QUIT_AFTER_PAINT=<n>`): once `n`
    /// windows have painted, quit the way the dock's Quit item does.
    /// A script cannot open a dock menu, and what has to be tested is
    /// the route — `terminate:` used to land on the boot window, and
    /// reading that as "close that window" is what left every
    /// other window on screen (2026-09-28 report).  Unset in the
    /// installed app.
    pub(crate) dev_quit_after_paint: Option<usize>,
    /// When the dev-seam quit is due.  `None` until the windows it
    /// waits for have painted — the app has to be all the way up, the
    /// way it is when someone reaches for the dock.
    pub(crate) dev_quit_at: Option<Instant>,
    /// How many cores this shell has spawned.  1 = the boot core; any
    /// higher value means a replacement (silent update, crash
    /// respawn), which must not re-run the saved-window restore.
    pub(crate) core_generation: u32,
    /// Rolling 30 s ring of CORE_SPAWN timestamps.  When this fills
    /// (≥3 entries inside the window) we emit a `CORE_BOOT_LOOP`
    /// WARN — the alarm the 2026-06-15 incident lacked.  In that
    /// case six cores booted in three minutes and the operator had
    /// to manually count `grep CORE_BOOT` lines to spot the loop.
    /// Bound at 16 so even a runaway respawn can't grow the ring.
    pub(crate) core_boot_ring: std::collections::VecDeque<Instant>,
    /// RFC-001 plugin host.  Plugins read pane / pty info through it.
    /// Registry owns the loaded plugins + drives their lifecycle.
    /// Both `None`-able so a misbuilt host (env-var disabled etc.)
    /// gracefully degrades to plugin-less marspot.
    pub(crate) plugin_host: ShellPluginHost,
    pub(crate) plugin_registry: PluginRegistry,
    /// When the supervisor last drove plugin ticks.  Coarse — the
    /// supervisor runs on the 1 s redraw pump plus whatever knocks it
    /// (core frames, queued plugin work), so a plugin asking for less
    /// than a second gets the supervisor's cadence.  Their per-plugin interval
    /// gating lives in the registry.
    pub(crate) last_plugin_tick: Instant,
    /// Generic per-pane foreground status (shell at prompt / job
    /// running / unknown), swept once a second from the supervisor
    /// tick and published to `plugin_host` so plugins read a snapshot.
    /// The pane-status layer that knows about specific programs lives
    /// in the plugins on top of this one.
    pub(crate) pane_status: pane_status::PaneStateTracker,
    /// Recede level, badge and title per session, sent to the core on
    /// change and all again to a new core.
    pub(crate) pane_look: pane_decls::PaneLook,
    /// Plugin → shell reports of what the program in a pane is doing.
    /// Drained at the top of each supervisor tick, straight into the
    /// state machines.  Same shape as the badge channel: plugins push,
    /// the main loop owns the state.
    pub(crate) pane_activity_rx:
        std::sync::mpsc::Receiver<(u64, marspot::pane_state::Activity)>,
    /// Receiver half of the channel `ShellPluginHost::set_pane_badge`
    /// pushes into; drained each `poll_supervisor` tick and forwarded
    /// to the active core as `MsgType::PaneBadge` frames.
    pub(crate) pane_badge_rx: std::sync::mpsc::Receiver<plugins::host::PaneBadgeUpdate>,
    pub(crate) pane_wheel_keys_rx: std::sync::mpsc::Receiver<plugins::host::PaneWheelKeysUpdate>,
    pub(crate) pane_agent_tui_rx: std::sync::mpsc::Receiver<plugins::host::PaneAgentTuiUpdate>,
    pub(crate) pane_render_markup_rx:
        std::sync::mpsc::Receiver<plugins::host::PaneRenderMarkupUpdate>,
    /// Per-pane declarations and autorun memory, dropped with the pane.
    pub(crate) pane_tables: pane_tables::PaneTables,
    /// Same shape as `pane_badge_rx` but for plugin-set pane titles.
    pub(crate) pane_title_rx: std::sync::mpsc::Receiver<plugins::host::PaneTitleUpdate>,
    /// The core's toolbar icons, drawn by a short-lived drawer.
    pub(crate) core_icons: core_icons::CoreIcons,
    /// Receiver for `begin_pane_session` requests.
    pub(crate) pane_session_begin_rx:
        std::sync::mpsc::Receiver<plugins::host::PaneSessionBeginRequest>,
    /// Submitted PTY operations, on their way into `pty_ops`.
    pub(crate) pty_op_rx: std::sync::mpsc::Receiver<plugins::host::PtyOpRequest>,
    /// Requests from the command socket (`marspot-shell --send`).
    pub(crate) cli_rx: std::sync::mpsc::Receiver<cli_socket::CliRequest>,
    /// Frames the pane control socket's connections want sent.
    pub(crate) pane_control: pane_control::MainSide,
    /// Panes the autorun policy is switched on for, by working
    /// directory — the one identity that survives a restart, a rename
    /// (there are none) and a move between windows.
    pub(crate) autorun_panes: std::collections::HashSet<String>,
    /// When the panes were last looked at.  Once a minute is plenty:
    /// every trigger requires the pane to have been quiet for longer
    /// than that already.
    pub(crate) autorun_last_look: Option<Instant>,
    /// The one queue that keeps two scripts from typing into the same
    /// pane at once.  Lives here because plugins are not its only
    /// submitter — the `--send` CLI is another.
    pub(crate) pty_ops: plugins::pty_op::PtyOps,
    /// How that queue starts a run.  Not the plugin host: that one
    /// checks the current *plugin's* permissions, and a run started by
    /// the supervisor has no plugin behind it.
    pub(crate) op_host: SupervisorOpHost,
    /// Receiver for cc-plugin inject-input requests; drained each tick
    /// and forwarded to the active core as `MsgType::InjectInput` frames.
    pub(crate) inject_input_rx: std::sync::mpsc::Receiver<plugins::host::InjectInputRequest>,
    /// Active PaneSessions held by L1 plugins, keyed by shelld
    /// session_id.  At most one per pane.
    pub(crate) active_pane_sessions:
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
    pub(crate) saved_window_frames: Vec<marspot::state::SavedWindow>,
}

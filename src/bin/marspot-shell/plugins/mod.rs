//! marspot plugin system — see RFC-001.
//!
//! ## Why module-wide `#[allow(dead_code)]`
//!
//! This module is the RFC-001 protocol surface.  Most items
//! (`Capabilities` bit constants, `PluginMetadata.version`, `PtyChild`
//! fields, `EndReason::{Timeout, PaneClosed}`, `LogLevel::Error`,
//! `on_pty_bytes` default trait method, `PaneSessionHost`'s
//! `pane_count` / `pane_pty_device` / `pane_pty_pid_tree` /
//! `pane_focused`) aren't reached by the in-tree plugins (claudecode +
//! pidtree) today but are part of the public ABI a future plugin may
//! rely on.  Deleting them would be a silent protocol break; warning
//! on each is noise.  Suppress at the module level so the surface
//! stays loud-by-RFC and silent-by-compiler.
#![allow(dead_code)]

//!
//! ## Design
//!
//! - Plugins run **in the L1 shell process**.  GUI / status / notify
//!   is the dominant use case; L2 core is hot path and not exposed.
//! - Static link only in v1.0.0 — `libloading` postponed to v1.1.
//!   ABI versioning still embedded so a future dynamic-load layer
//!   doesn't need a redesign.
//! - Crash isolation: every `trait Plugin` method called from the
//!   host is wrapped in `catch_unwind`; a panicking plugin is
//!   logged + disabled, marspot continues.
//! - Performance budget: `tick` and event hooks measured; consecutive
//!   overshoots auto-disable.  Forensic via `plugin.<name>.*` tags
//!   in `marspot.log`.
//! - Capability boundary: set by what the code IS, not by what it
//!   declares.  Official plugins are Rust compiled into this binary and
//!   reviewed like the renderer is; a permission bit cannot hold a
//!   native call inside the same process, so there is none to read as a
//!   guarantee that is not there.  The design and the precedent are in
//!   `.dev/rfcs`.
//!
//! ## Module layout
//!
//! - `mod.rs` (this file) — public trait + types + `PluginRegistry`
//! - `host.rs` — `PluginHost` impl backed by the shell's state
//! - `claudecode.rs` — the first plugin, always-on
//!
//! ## API versioning
//!
//! Packed `u32`: high 16 = MAJOR, low 16 = MINOR.  Host rejects a
//! plugin whose MAJOR != host MAJOR.  New methods land as default-impl
//! `trait Plugin` items; MAJOR bumps reserved for breaking signatures.

use std::fmt;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use marspot::shell_proto::WireKeyEvent;
use marspot::{lx_debug, lx_error, lx_event, lx_warn};

pub mod autorun;
pub mod claudecode;
pub mod codex;
pub mod handoff;
pub mod host;
pub mod dispatch;
pub mod pty_op;
// F3+1 — pidtree moved to marspot lib (`src/pidtree.rs`) so L2
// (marspot-core) can use the same libproc walker for the process-
// tree panel.  Re-export under the old path keeps L1 plugin code
// unchanged (`crate::plugins::pidtree::list_all_procs(...)`).
pub use marspot::pidtree;

/// Packed `MAJOR:MINOR` u32.  v0.1.0 ⇒ `0x0001_0001` (MAJOR=1, MINOR=1).
/// Bump MINOR for additive default-impl methods; bump MAJOR only for
/// breaking signatures (a plugin built against an older MAJOR is
/// rejected by the host).
pub const PLUGIN_API_VERSION: u32 = 0x0001_0001;

/// Performance budget for any single plugin hook (init, start, tick,
/// stop, event).  Three consecutive overshoots auto-disable the
/// plugin.  50 ms accommodates the claudecode tick on a busy machine:
/// `proc_listpids` returns ~800 procs each followed by per-session
/// `proc_cmdline` + `proc_cwd` round-trips, which clusters around
/// 10-30 ms typical and spikes higher on a freshly-installed shell
/// (first tick, cold caches).  At a 2 s plugin interval, 50 ms is
/// 2.5 % CPU upper bound per plugin — still well-behaved for a
/// background watcher, but headroom to avoid auto-disabling claudecode
/// the moment the box gets even slightly busy.  Plugins that legitimately
/// need to do more work per tick should subscribe to events instead
/// of growing this budget further.
// Raised 2026-06-17 from 50ms after RFC-003 production install — the
// claudecode plugin's per-tick fs::read_dir walk over ~/.claude/projects
// (one stat per project + one per .jsonl) lands around 50-55ms on the
// dev box, tripping BUDGET_OVERSHOOT_LIMIT and auto-disabling the plugin.
// 100ms keeps the "plugin can't stall the L1 event loop" property
// (event loop targets one tick / vsync = 16ms; 100ms hooks still
// pre-empt a frame, just not three) while giving filesystem-walk hooks
// real headroom.  If a hook regularly burns this, switch to event
// subscription per the original comment.
pub const HOOK_BUDGET: Duration = Duration::from_millis(100);
const BUDGET_OVERSHOOT_LIMIT: u32 = 3;


#[derive(Clone, Debug)]
pub struct PluginMetadata {
    pub name: &'static str,
    pub version: &'static str,
    pub api_version: u32,
    /// Desired tick interval.  `0` means "no tick".  Host caps it at
    /// 100 ms minimum to keep idle CPU bounded; plugins wanting faster
    /// should subscribe to events instead.
    pub tick_interval_ms: u32,
}

#[derive(Debug)]
pub enum PluginError {
    Other(String),
    IoError(std::io::Error),
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PluginError::Other(s) => write!(f, "{}", s),
            PluginError::IoError(e) => write!(f, "io: {}", e),
        }
    }
}

impl From<std::io::Error> for PluginError {
    fn from(e: std::io::Error) -> Self {
        PluginError::IoError(e)
    }
}

/// What `PluginHost::pane_status` answers with.
///
/// `quiescent` is the decision surface and the only field a policy
/// should branch on: it means the machine has held a quiet state long
/// enough to be believed (see `pane_state::CONFIRM_TICKS`).  `status`
/// and `held` are for display and logs.
#[derive(Clone, Debug)]
pub struct PaneStatusView {
    pub status: marspot::pane_state::PaneStatus,
    pub held: Duration,
    pub quiescent: bool,
}

/// One PTY child observed by `pane_pty_pid_tree`.
#[derive(Clone, Debug)]
pub struct PtyChild {
    pub pid: u32,
    pub ppid: u32,
    pub cmdline: String,
    pub cwd: Option<PathBuf>,
}

/// Host → Plugin API.  All methods are `&self` so plugins can hold
/// the host in async contexts / pass to threads without lifetime
/// gymnastics; the underlying state uses interior mutability where
/// needed.
pub trait PluginHost: Send + Sync {
    // ── Info(READ_PANE_INFO / READ_PTY_TREE) ──────────────────────
    fn pane_count(&self) -> usize;
    fn pane_pty_device(&self, pane: usize) -> Result<Option<PathBuf>, PluginError>;
    fn pane_pty_pid_tree(&self, pane: usize) -> Result<Vec<PtyChild>, PluginError>;

    /// The pane's composed state — the kernel's view of the pane
    /// folded with whatever plugins have reported about it — plus how
    /// long it has held and whether it is safe to act on.
    ///
    /// `Ok(None)` = no machine for that session (it just appeared, or
    /// it is gone).  That is NOT "quiet": a caller deciding whether to
    /// touch a pane reads `quiescent` and nothing else.
    ///
    /// Sampled by the shell once per second, so a plugin may call this
    /// per pane without multiplying syscalls.
    fn pane_status(
        &self,
        _shelld_session_id: u64,
    ) -> Result<Option<PaneStatusView>, PluginError> {
        Ok(None)
    }

    /// Report what the program this plugin understands is doing in a
    /// pane.  The shell folds it with the kernel's view; a plugin does
    /// not compose the two itself, and does not decide what is
    /// actionable.
    ///
    /// Reporting `Activity::Absent` is meaningful — it says "my
    /// program is not in this pane", which is different from staying
    /// silent.
    fn report_pane_activity(
        &self,
        _shelld_session_id: u64,
        _activity: marspot::pane_state::Activity,
    ) -> Result<(), PluginError> {
        Ok(())
    }

    fn pane_focused(&self) -> Option<usize>;

    // ── Persistence(PERSIST_STATE) ───────────────────────────────
    /// Plugin-scoped writable directory under
    /// `~/Library/Caches/marspot/plugins/<plugin_name>/`.  Created on
    /// first call.  10 MB soft cap — overspend logs WARN but doesn't
    /// fail writes (Phase 2 will enforce; MVP just observes).
    fn state_dir(&self) -> Result<PathBuf, PluginError>;

    // ── Logging(always permitted) ────────────────────────────────
    /// Forward to logx with the plugin's tag namespace
    /// (`plugin.<name>.<tag>`).  Cheap when level is filtered.
    fn log(&self, level: LogLevel, tag: &str, msg: &str);

    // ── Plugin-context book-keeping ──────────────────────────────
    /// Registry calls this before invoking a hook so subsequent
    /// permission / log namespace checks know which plugin asked.
    /// Default no-op so non-tracking hosts (tests) don't need to
    /// implement it.
    ///
    /// The token is why this is safe to have on the trait every plugin
    /// holds. Without it a plugin could call this inside its own hook
    /// -- the implementation writes whatever it is given, without
    /// checking that the caller is the plugin it names or that the
    /// permissions are a subset of the ones it has -- and hand itself
    /// everything for the rest of that hook, which is long enough to
    /// submit an op or take a pane. `Dispatching` can only be made
    /// here, so a plugin cannot say it.
    fn set_active_plugin(&self, _who: &dispatch::Dispatching, _name: &'static str) {}
    /// Pairs with `set_active_plugin`.
    fn clear_active_plugin(&self, _who: &dispatch::Dispatching) {}

    /// Decorate the right side of the pane title strip for the pane
    /// backing this shelld session.  Empty `text` clears the badge.
    /// Requires `SET_STATUS_LINE`.  Default impl is a no-op so test
    /// hosts don't have to wire L2 control sockets.
    fn set_pane_badge(
        &self,
        _shelld_session_id: u64,
        _text: &str,
    ) -> Result<(), PluginError> {
        Ok(())
    }

    /// Set the main title text for the pane backing this shelld
    /// session.  Inserts into the title resolution chain ABOVE cwd
    /// basename, BELOW user-set custom title.  Empty `text` clears
    /// the plugin-set entry.  Requires `SET_STATUS_LINE`.  Default
    /// no-op for test hosts.
    fn set_pane_title(
        &self,
        _shelld_session_id: u64,
        _text: &str,
    ) -> Result<(), PluginError> {
        Ok(())
    }

    /// Declare how the wheel reaches the program in this pane.
    ///
    /// For a program that repaints in place and does not ask for mouse
    /// reporting, the terminal has nothing to scroll and no way to
    /// forward a tick — only the plugin knows which keys the program
    /// answers to.  `enter` is sent once when scrolling begins from
    /// the program's normal view; `up`/`down` go per tick.  Empty
    /// `up`/`down` clears the declaration.
    ///
    /// Requires `SET_STATUS_LINE`.  Default no-op for test hosts.
    /// Say that this pane's program prints markup it does not render,
    /// so the terminal should draw it.
    ///
    /// Per pane, because a terminal is where people TALK about markup:
    /// as a global setting it ate `<u>` out of the conversation that
    /// specified the feature (2026-09-06).  Only the plugin driving a
    /// program knows the program does not render its own HTML.
    fn set_pane_render_markup(
        &self,
        _shelld_session_id: u64,
        _on: bool,
    ) -> Result<(), PluginError> {
        Ok(())
    }

    /// Requires `SET_STATUS_LINE`.  Default no-op for test hosts.
    /// Say that an agent TUI paints this pane.
    ///
    /// L2 needs to know because such a program renders to its own
    /// fixed inner width and hard-wraps long tokens with a hanging
    /// indent — the link scanner has to merge those rows or a wrapped
    /// path stops being a link.
    ///
    /// Declared, not inferred, and re-issued every tick.  It used to
    /// be read off two proxies — a wheel-key declaration or a
    /// non-empty badge — and both are sent once or only when they
    /// change, so a core swap left the new L2 with neither: on
    /// 2026-09-07 a path broken at a hard wrap stopped being a link
    /// while the three unwrapped ones beside it still were.
    fn set_pane_agent_tui(
        &self,
        _shelld_session_id: u64,
        _on: bool,
    ) -> Result<(), PluginError> {
        Ok(())
    }

    fn set_pane_wheel_keys(
        &self,
        _shelld_session_id: u64,
        _enter: &[u8],
        _up: &[u8],
        _down: &[u8],
        _marker: &[u8],
    ) -> Result<(), PluginError> {
        Ok(())
    }

    /// RFC-003 Amendment 16 cc: hand the plugin a proxy it can use
    /// to forward raw bytes into a pane's PTY via the L1→L2→L3
    /// `InjectInput` wire frame.  Default `None` for test hosts.
    fn cc_inject_proxy(
        &self,
    ) -> Option<std::sync::Arc<dyn crate::plugins::claudecode::InjectInputProxy>> {
        None
    }

    /// RFC-003: take over the pane backing `shelld_session_id` for
    /// the duration of the returned PaneSession.  The host:
    ///   1. emits PaneSessionBegin to L2 with the session's caps
    ///   2. routes subsequent key / pty / escape events to the
    ///      session's callbacks until on_end fires
    ///   3. emits PaneSessionEnd on tear-down
    ///
    /// Returns Err when a session for the same pane is already in
    /// flight (one at a time, per RFC-003 § 9).  Default impl is a
    /// no-op Err for hosts that haven't wired L2.
    fn begin_pane_session(
        &self,
        shelld_session_id: u64,
        session: Box<dyn PaneSession>,
    ) -> Result<(), PluginError> {
        let _ = (shelld_session_id, session);
        Err(PluginError::Other("begin_pane_session unsupported".into()))
    }

    /// Queue a scripted PTY operation against a pane.
    ///
    /// Prefer this over `begin_pane_session` for anything that types:
    /// it goes through the one queue that keeps two scripts from
    /// interleaving their keystrokes on the same PTY, and that queue
    /// has submitters other than plugins.
    fn submit_pty_op(&self, shelld_session_id: u64, op: pty_op::PtyOp) -> Result<(), PluginError> {
        self.submit_pty_op_at(shelld_session_id, op, 0)
    }

    /// Queue an operation that starts partway in — how a parked script
    /// is re-armed after the process that owned it was replaced.
    fn submit_pty_op_at(
        &self,
        shelld_session_id: u64,
        op: pty_op::PtyOp,
        start_at: usize,
    ) -> Result<(), PluginError> {
        let _ = (shelld_session_id, op, start_at);
        Err(PluginError::Other("submit_pty_op unsupported".into()))
    }
}

#[derive(Clone, Copy, Debug)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// What every plugin implements.  See RFC-001 §4.
pub trait Plugin: Send + Sync {
    fn metadata(&self) -> PluginMetadata;

    /// Called once after registration.  Plugin can allocate state,
    /// validate its environment.  Failing here marks the plugin
    /// disabled for the session.
    fn init(&mut self, host: &dyn PluginHost) -> Result<(), PluginError> {
        let _ = host;
        Ok(())
    }

    /// Called when marspot is ready to use the plugin (after L2 core
    /// boot + sessions attached).  Multiple `start`/`stop` cycles
    /// allowed once hot-reload lands; MVP calls once.
    fn start(&mut self, host: &dyn PluginHost) -> Result<(), PluginError> {
        let _ = host;
        Ok(())
    }

    /// Periodic tick.  Frequency is `metadata.tick_interval_ms`,
    /// capped at 100 ms by host.  Plugin may early-return — host
    /// charges no cost for a bare return.
    fn tick(&mut self, host: &dyn PluginHost) {
        let _ = host;
    }

    /// Called before plugin is dropped (marspot quit / explicit
    /// disable).  Plugin should release resources here.
    fn stop(&mut self, host: &dyn PluginHost) {
        let _ = host;
    }

    /// L2 → L1 callback: the user clicked the active prefix of a
    /// pane badge this plugin set.  `shelld_session_id` identifies
    /// the pane.  Default no-op so plugins without a click-handle
    /// don't need to override.  Same panic / budget rules as the
    /// other hooks; called on the supervisor thread.
    /// The user's focus moved to this pane.
    ///
    /// Default no-op.  A plugin that parked something in the pane uses
    /// it to start restoring before the user types — the alternative
    /// (wait for a keystroke) begins the work after they have already
    /// tried to use the pane.
    fn on_pane_focused(&mut self, _host: &dyn PluginHost, _shelld_session_id: u64) {}

    fn on_pane_badge_click(
        &mut self,
        host: &dyn PluginHost,
        shelld_session_id: u64,
    ) {
        let _ = (host, shelld_session_id);
    }

    /// L2 → L1 callback: the user RIGHT-clicked the active prefix of
    /// a pane badge.  Return the context-menu rows this plugin offers
    /// for that pane (empty = nothing; the registry concatenates
    /// across plugins).  Tags are plugin-opaque — the pick comes back
    /// via `on_pane_badge_menu_action` with the same tag.
    fn pane_badge_menu(
        &mut self,
        host: &dyn PluginHost,
        shelld_session_id: u64,
    ) -> Vec<marspot::shell_proto::PaneBadgeMenuItem> {
        let _ = (host, shelld_session_id);
        Vec::new()
    }

    /// L2 → L1 callback: the user picked a `pane_badge_menu` row.
    /// `tag` is the id this plugin assigned when building the menu.
    /// Plugins must tolerate tags they didn't assign (another plugin
    /// may have contributed rows to the same menu).
    fn on_pane_badge_menu_action(
        &mut self,
        host: &dyn PluginHost,
        shelld_session_id: u64,
        tag: u32,
    ) {
        let _ = (host, shelld_session_id, tag);
    }
}

/// RFC-003 PaneSession — a plugin temporarily takes over a pane.
/// One instance binds one shelld_session_id; the host instantiates
/// the host-side counterpart and routes lifecycle events here.
pub trait PaneSession: Send {
    /// Capability bits this session wants the host to honour.
    /// See `marspot::shell_proto::PANE_SESSION_CAP_*`.
    fn caps(&self) -> u32;

    /// A keystroke arrived on the held pane.  Only fires when
    /// LOCK_KEYS is in `caps()`.  Return value decides what the host
    /// does next.
    fn on_user_key(
        &mut self,
        host: &dyn PaneSessionHost,
        ev: &WireKeyEvent,
    ) -> KeyHandling {
        let _ = (host, ev);
        KeyHandling::Swallow
    }

    /// PTY bytes for the held pane.  Only fires when OBSERVE_PTY is
    /// in `caps()` (wiring lands in C4 — until then, never called).
    /// The user's focus moved to this pane.  Default no-op.
    ///
    /// For a session that parked something here, this is the moment to
    /// start bringing it back — earlier than the first keystroke, and
    /// early enough that the work overlaps with the user reading the
    /// screen.
    fn on_focus(&mut self, _host: &dyn PaneSessionHost) {}

    /// Is this session parked, waiting for the user to come back?
    ///
    /// The one bit of a session's internals the shell needs from the
    /// outside: coming back to marspot after a while means every parked
    /// pane is about to be wanted, and waking them costs seconds each.
    /// Starting that on the way in rather than on the click is the
    /// difference between "it was ready" and "it hung".
    fn parked(&self) -> bool {
        false
    }

    fn on_pty_bytes(&mut self, host: &dyn PaneSessionHost, bytes: &[u8]) {
        let _ = (host, bytes);
    }

    /// Periodic tick at the plugin's normal cadence (or faster while
    /// any PaneSession is alive — see C5).
    fn on_tick(&mut self, host: &dyn PaneSessionHost) {
        let _ = host;
    }

    /// Final callback — session is being torn down.  Host has already
    /// emitted PaneSessionEnd to L2 by the time this fires; plugin
    /// drops any local state.
    fn on_end(&mut self, host: &dyn PaneSessionHost, reason: EndReason) {
        let _ = (host, reason);
    }
}

/// What the host should do after the plugin sees a keystroke.
/// "Forward to PTY" isn't a host concern — a plugin that wants the
/// key to reach the shell can call its own `ShelldClient::send_input_to`
/// from within `on_user_key` and then return `Swallow`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyHandling {
    /// Done — drop any further L2 processing.
    Swallow,
    /// End the session immediately; on_end fires with PluginRequested.
    EndSession,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    /// Plugin called `host.end()`.
    PluginRequested,
    /// User pressed Esc 3× in 5 s (L2 force-end).
    UserEscape,
    /// Host-side safety: stale session past max duration.
    Timeout,
    /// L2 told us the pane is gone (kill / detach).
    PaneClosed,
}

/// Per-PaneSession host handle passed to every plugin callback.
/// Binds (shelld_session_id, plugin name) implicitly; the plugin
/// doesn't pass it around.
pub trait PaneSessionHost {
    fn shelld_session_id(&self) -> u64;
    /// Tear down the session.  Triggers on_end with PluginRequested
    /// on the next tick (or immediately if we're already inside a
    /// dispatch — implementation can defer to avoid re-entrancy).
    fn end(&self);
    /// Update the pane's right-side badge text (typically a spinner /
    /// progress string while the session runs).  Empty clears.
    fn set_badge(&self, text: &str);
    /// Update the pane's main title text.  Inserts into the title
    /// resolution chain ABOVE cwd basename, BELOW user-set custom
    /// title.  Empty clears the plugin-set entry — chain falls back
    /// to the underlying basename / ordinal label.
    fn set_pane_title(&self, text: &str);
    /// Plugin-namespaced log proxy mirroring the regular PluginHost
    /// log so callbacks don't need to thread the outer host through.
    fn log(&self, level: LogLevel, tag: &str, msg: &str);
}

/// Per-plugin runtime state — wraps the user's `Box<dyn Plugin>` with
/// budget tracking, enable flag, and last-tick timestamp.
struct Slot {
    plugin: Box<dyn Plugin>,
    metadata: PluginMetadata,
    enabled: bool,
    /// Consecutive HOOK_BUDGET overshoots.  Reset on a clean run.
    consecutive_overshoots: u32,
    last_tick: Instant,
}

/// Manages a fixed set of plugins for the lifetime of one shell
/// process.  No add/remove after `init_all`; v1.1 dynamic-load story
/// adds that.
pub struct PluginRegistry {
    slots: Vec<Slot>,
    started: bool,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self { slots: Vec::new(), started: false }
    }

    /// Add a plugin.  Must be called before `init_all`.
    pub fn register(&mut self, plugin: Box<dyn Plugin>) {
        let metadata = plugin.metadata();
        self.slots.push(Slot {
            plugin,
            metadata,
            enabled: true,
            consecutive_overshoots: 0,
            last_tick: Instant::now() - Duration::from_secs(1),
        });
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Sugar for ShellPluginHost callers — they typically have a
    /// concrete reference and the `&dyn` coercion can be a pain in
    /// the lifetime-juggling.  Forwards to the trait-object impl.
    pub fn init_all_with<H: PluginHost + 'static>(&mut self, host: &H) {
        self.init_all(host as &dyn PluginHost);
    }
    pub fn start_all_with<H: PluginHost + 'static>(&mut self, host: &H) {
        self.start_all(host as &dyn PluginHost);
    }
    pub fn tick_all_with<H: PluginHost + 'static>(&mut self, host: &H) {
        self.tick_all(host as &dyn PluginHost);
    }
    pub fn stop_all_with<H: PluginHost + 'static>(&mut self, host: &H) {
        self.stop_all(host as &dyn PluginHost);
    }

    pub fn init_all(&mut self, host: &dyn PluginHost) {
        for slot in self.slots.iter_mut() {
            // API-version gate.  Plugin MAJOR != host MAJOR is the
            // only veto — MINOR mismatch is fine (additive methods
            // have default impls).
            let plugin_major = (slot.metadata.api_version >> 16) as u16;
            let host_major = (PLUGIN_API_VERSION >> 16) as u16;
            if plugin_major != host_major {
                lx_event!(
                    "plugin.api_version_mismatch",
                    "plugin rejected — API MAJOR mismatch",
                    name = slot.metadata.name,
                    plugin_major = plugin_major,
                    host_major = host_major
                );
                slot.enabled = false;
                continue;
            }
            dispatch::with(host, slot.metadata.name, || {
            run_hook(slot, "init", |p| p.init(host));
            });
        }
    }

    pub fn start_all(&mut self, host: &dyn PluginHost) {
        for slot in self.slots.iter_mut() {
            if !slot.enabled {
                continue;
            }
            dispatch::with(host, slot.metadata.name, || {
            run_hook(slot, "start", |p| p.start(host));
            });
        }
        self.started = true;
    }

    pub fn tick_all(&mut self, host: &dyn PluginHost) {
        if !self.started {
            return;
        }
        let now = Instant::now();
        for slot in self.slots.iter_mut() {
            if !slot.enabled || slot.metadata.tick_interval_ms == 0 {
                continue;
            }
            let interval = Duration::from_millis(
                (slot.metadata.tick_interval_ms as u64).max(100),
            );
            if now.duration_since(slot.last_tick) < interval {
                continue;
            }
            slot.last_tick = now;
            dispatch::with(host, slot.metadata.name, || {
            run_hook_void(slot, "tick", |p| p.tick(host));
            });
        }
    }

    /// Fan a PaneBadgeClicked event to every enabled plugin.  The L1
    /// shell main loop calls this when it receives the frame from L2;
    /// plugins that didn't set a badge ignore it via the default
    /// no-op.
    /// Fan a focus change out to every plugin, under the same
    /// crash-isolation + budget policing as any other hook.
    pub fn dispatch_pane_focused(&mut self, host: &dyn PluginHost, sid: u64) {
        for slot in self.slots.iter_mut() {
            if !slot.enabled {
                continue;
            }
            dispatch::with(host, slot.metadata.name, || {
            run_hook_void(slot, "on_pane_focused", |p| p.on_pane_focused(host, sid));
            });
        }
    }

    pub fn dispatch_pane_badge_click(
        &mut self,
        host: &dyn PluginHost,
        shelld_session_id: u64,
    ) {
        for slot in self.slots.iter_mut() {
            if !slot.enabled {
                continue;
            }
            dispatch::with(host, slot.metadata.name, || {
            run_hook_void(slot, "on_pane_badge_click", |p| {
                p.on_pane_badge_click(host, shelld_session_id)
            });
            });
        }
    }

    pub fn dispatch_pane_badge_click_with<H: PluginHost + 'static>(
        &mut self,
        host: &H,
        shelld_session_id: u64,
    ) {
        self.dispatch_pane_badge_click(host as &dyn PluginHost, shelld_session_id);
    }

    /// Collect badge context-menu rows for a pane across every
    /// enabled plugin (concatenated in registration order).  Called
    /// when L2 reports a right-click on the badge prefix; the result
    /// goes back over the wire as a `PaneBadgeMenu` frame.
    pub fn dispatch_pane_badge_menu(
        &mut self,
        host: &dyn PluginHost,
        shelld_session_id: u64,
    ) -> Vec<marspot::shell_proto::PaneBadgeMenuItem> {
        let mut out = Vec::new();
        for slot in self.slots.iter_mut() {
            if !slot.enabled {
                continue;
            }
            dispatch::with(host, slot.metadata.name, || {
            if let Some(items) = run_hook_ret(slot, "pane_badge_menu", |p| {
                p.pane_badge_menu(host, shelld_session_id)
            }) {
                out.extend(items);
            }
            });
        }
        out
    }

    /// Fan a badge-menu pick to every enabled plugin.  Plugins ignore
    /// tags they didn't assign, so fanning (vs routing to the item's
    /// author) keeps the registry free of per-row ownership state.
    pub fn dispatch_pane_badge_menu_action(
        &mut self,
        host: &dyn PluginHost,
        shelld_session_id: u64,
        tag: u32,
    ) {
        for slot in self.slots.iter_mut() {
            if !slot.enabled {
                continue;
            }
            dispatch::with(host, slot.metadata.name, || {
            run_hook_void(slot, "on_pane_badge_menu_action", |p| {
                p.on_pane_badge_menu_action(host, shelld_session_id, tag)
            });
            });
        }
    }

    pub fn stop_all(&mut self, host: &dyn PluginHost) {
        for slot in self.slots.iter_mut() {
            if !slot.enabled {
                continue;
            }
            dispatch::with(host, slot.metadata.name, || {
            run_hook_void(slot, "stop", |p| p.stop(host));
            });
        }
        self.started = false;
    }
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Run a Result-returning hook with budget tracking + panic catch.
/// On panic / Err / overshoot: log + (eventually) disable.
fn run_hook<F>(slot: &mut Slot, tag: &'static str, f: F)
where
    F: FnOnce(&mut Box<dyn Plugin>) -> Result<(), PluginError>,
{
    let t0 = Instant::now();
    let cpu0 = thread_cpu_now();
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| f(&mut slot.plugin)));
    let dur = t0.elapsed();
    let cpu = thread_cpu_now().saturating_sub(cpu0);
    match result {
        Ok(Ok(())) => {
            check_budget(slot, tag, dur, cpu);
        }
        Ok(Err(e)) => {
            lx_warn!(
                "plugin.hook_err",
                "plugin hook returned error",
                name = slot.metadata.name,
                hook = tag,
                error = format!("{}", e)
            );
            slot.enabled = false;
        }
        Err(_panic) => {
            lx_error!(
                "plugin.panic",
                "plugin panicked — disabling",
                name = slot.metadata.name,
                hook = tag
            );
            slot.enabled = false;
        }
    }
}

/// Same as `run_hook_void` but for value-returning hooks
/// (pane_badge_menu).  A panic disables the plugin and yields `None`.
fn run_hook_ret<T, F>(slot: &mut Slot, tag: &'static str, f: F) -> Option<T>
where
    F: FnOnce(&mut Box<dyn Plugin>) -> T,
{
    let t0 = Instant::now();
    let cpu0 = thread_cpu_now();
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| f(&mut slot.plugin)));
    let dur = t0.elapsed();
    let cpu = thread_cpu_now().saturating_sub(cpu0);
    match result {
        Ok(v) => {
            check_budget(slot, tag, dur, cpu);
            Some(v)
        }
        Err(_panic) => {
            lx_error!(
                "plugin.panic",
                "plugin panicked — disabling",
                name = slot.metadata.name,
                hook = tag
            );
            slot.enabled = false;
            None
        }
    }
}

/// Same as `run_hook` but for `()`-returning hooks (tick/stop).
fn run_hook_void<F>(slot: &mut Slot, tag: &'static str, f: F)
where
    F: FnOnce(&mut Box<dyn Plugin>),
{
    let t0 = Instant::now();
    let cpu0 = thread_cpu_now();
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| f(&mut slot.plugin)));
    let dur = t0.elapsed();
    let cpu = thread_cpu_now().saturating_sub(cpu0);
    match result {
        Ok(()) => {
            check_budget(slot, tag, dur, cpu);
        }
        Err(_panic) => {
            lx_error!(
                "plugin.panic",
                "plugin panicked — disabling",
                name = slot.metadata.name,
                hook = tag
            );
            slot.enabled = false;
        }
    }
}


/// CPU this thread has actually burned, as opposed to wall-clock.
///
/// The budget exists to catch a hook that hogs the thread the window
/// is drawn on. Wall-clock cannot tell that from a hook that was
/// descheduled while something else had the machine -- and on a busy
/// host it reads the second as the first. Two consecutive "slow"
/// ticks were recorded on 2026-09-30 at 17:26:00 and 17:26:02, both
/// around 70 ms, with nothing logged between them and a build running
/// on the same machine. One more and the plugin that reclaims panes
/// and switches accounts would have disabled itself over somebody
/// else's load.
///
/// So the strike is decided on CPU time and the log still reports the
/// wall-clock, because the wall-clock is what the user felt.
fn thread_cpu_now() -> Duration {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid, correctly-aligned timespec we own, and
    // the clock id is a constant this platform defines.
    let ok = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) } == 0;
    if ok {
        Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
    } else {
        // No clock: fall back to charging nothing, which cannot
        // disable a plugin. A missing clock is not a plugin's fault.
        Duration::ZERO
    }
}

fn check_budget(slot: &mut Slot, tag: &'static str, dur: Duration, cpu: Duration) {
    if cpu > HOOK_BUDGET {
        slot.consecutive_overshoots += 1;
        lx_warn!(
            "plugin.budget_overshoot",
            "plugin hook exceeded budget",
            name = slot.metadata.name,
            hook = tag,
            dur_us = dur.as_micros() as u64,
            cpu_us = cpu.as_micros() as u64,
            budget_us = HOOK_BUDGET.as_micros() as u64,
            count = slot.consecutive_overshoots
        );
        if slot.consecutive_overshoots >= BUDGET_OVERSHOOT_LIMIT {
            lx_event!(
                "plugin.disabled",
                "plugin auto-disabled — budget overshoot limit reached",
                name = slot.metadata.name,
                limit = BUDGET_OVERSHOOT_LIMIT
            );
            slot.enabled = false;
        }
    } else {
        // Clean run resets the counter — we only care about runaway
        // sustained overshoots, not the occasional GC pause.
        slot.consecutive_overshoots = 0;
        lx_debug!(
            "plugin.hook_dur",
            "plugin hook finished",
            name = slot.metadata.name,
            hook = tag,
            dur_us = dur.as_micros() as u64,
            cpu_us = cpu.as_micros() as u64
        );
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    /// A hook that is slow on the clock but used no CPU was waiting,
    /// not working. Striking it would disable a plugin over somebody
    /// else's load -- and the plugin that gets disabled is the one
    /// that reclaims panes and switches accounts.
    #[test]
    fn being_descheduled_is_not_an_overshoot() {
        let mut slot = test_slot();
        for _ in 0..(BUDGET_OVERSHOOT_LIMIT + 2) {
            check_budget(&mut slot, "tick", HOOK_BUDGET * 10, Duration::from_micros(200));
        }
        assert!(slot.enabled, "a plugin must not be disabled for waiting");
        assert_eq!(slot.consecutive_overshoots, 0);
    }

    #[test]
    fn burning_the_thread_still_strikes() {
        let mut slot = test_slot();
        for _ in 0..BUDGET_OVERSHOOT_LIMIT {
            check_budget(&mut slot, "tick", HOOK_BUDGET * 10, HOOK_BUDGET * 2);
        }
        assert!(!slot.enabled, "sustained CPU over budget is what this is for");
    }

    #[test]
    fn one_clean_run_clears_the_strikes() {
        let mut slot = test_slot();
        check_budget(&mut slot, "tick", HOOK_BUDGET * 10, HOOK_BUDGET * 2);
        check_budget(&mut slot, "tick", Duration::from_millis(1), Duration::from_micros(50));
        assert_eq!(slot.consecutive_overshoots, 0);
        check_budget(&mut slot, "tick", HOOK_BUDGET * 10, HOOK_BUDGET * 2);
        check_budget(&mut slot, "tick", HOOK_BUDGET * 10, HOOK_BUDGET * 2);
        assert!(slot.enabled, "the strikes have to be consecutive");
    }

    /// The clock has to move, or every hook looks free and the budget
    /// stops meaning anything.
    #[test]
    fn the_cpu_clock_actually_advances() {
        let before = thread_cpu_now();
        let mut n: u64 = 0;
        for i in 0..3_000_000u64 {
            n = n.wrapping_add(i).rotate_left(7);
        }
        assert_ne!(n, 0);
        assert!(
            thread_cpu_now() > before,
            "CLOCK_THREAD_CPUTIME_ID did not move across three million rotations"
        );
    }

    fn test_slot() -> Slot {
        Slot {
            plugin: Box::new(Nothing),
            metadata: PluginMetadata {
                name: "t",
                version: "0.0.0",
                api_version: PLUGIN_API_VERSION,
                tick_interval_ms: 1000,
            },
            enabled: true,
            consecutive_overshoots: 0,
            last_tick: Instant::now(),
        }
    }

    struct Nothing;
    impl Plugin for Nothing {
        fn metadata(&self) -> PluginMetadata {
            PluginMetadata {
                name: "t",
                version: "0.0.0",
                api_version: PLUGIN_API_VERSION,
                tick_interval_ms: 1000,
            }
        }
    }
}

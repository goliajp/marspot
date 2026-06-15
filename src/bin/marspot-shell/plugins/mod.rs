//! marspot plugin system — see RFC-001.
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
//! - Permissions: each plugin declares a `PermissionSet` in metadata;
//!   host API entry-points check before honouring requests.  Default
//!   is "no permission" — explicit opt-in only.
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

use marspot::{lx_debug, lx_error, lx_event, lx_warn};

pub mod claudecode;
pub mod host;
pub mod pidtree;

/// Packed `MAJOR:MINOR` u32.  v0.1.0 ⇒ `0x0001_0001` (MAJOR=1, MINOR=1).
/// Bump MINOR for additive default-impl methods; bump MAJOR only for
/// breaking signatures (a plugin built against an older MAJOR is
/// rejected by the host).
pub const PLUGIN_API_VERSION: u32 = 0x0001_0001;

/// Performance budget for any single plugin hook (init, start, tick,
/// stop, event).  Three consecutive overshoots auto-disable the
/// plugin.  10 ms accommodates the claudecode tick's per-tick
/// `proc_listpids` + per-session `proc_pidinfo` + cmdline reads on a
/// busy machine (a few ms even on M-series), while still small enough
/// that a runaway plugin can't visibly stall the shell — tick fires
/// every 2 s minimum, so 10 ms = 0.5 % CPU upper bound per plugin.
pub const HOOK_BUDGET: Duration = Duration::from_millis(10);
const BUDGET_OVERSHOOT_LIMIT: u32 = 3;

/// What a plugin is allowed to ask the host for.  Default = none.
/// Declared in `PluginMetadata::permissions`.  Each `PluginHost`
/// method that requires a permission checks via `bits & FLAG != 0`
/// and returns `Err(NotPermitted)` otherwise.  Hand-rolled u32
/// instead of pulling in `bitflags` — marspot stays self-build per
/// the project's dependency principle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PermissionSet(pub u32);

impl PermissionSet {
    pub const NONE: Self = Self(0);
    pub const READ_PANE_INFO: Self = Self(1 << 0);
    pub const READ_PTY_TREE: Self = Self(1 << 1);
    pub const READ_DISK_FS: Self = Self(1 << 2);
    pub const NOTIFY_USER: Self = Self(1 << 3);
    pub const SET_STATUS_LINE: Self = Self(1 << 4);
    pub const INJECT_INPUT: Self = Self(1 << 5);
    pub const SUBSCRIBE_GRID: Self = Self(1 << 6);
    pub const PERSIST_STATE: Self = Self(1 << 7);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub fn names(self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.contains(Self::READ_PANE_INFO) {
            out.push("READ_PANE_INFO");
        }
        if self.contains(Self::READ_PTY_TREE) {
            out.push("READ_PTY_TREE");
        }
        if self.contains(Self::READ_DISK_FS) {
            out.push("READ_DISK_FS");
        }
        if self.contains(Self::NOTIFY_USER) {
            out.push("NOTIFY_USER");
        }
        if self.contains(Self::SET_STATUS_LINE) {
            out.push("SET_STATUS_LINE");
        }
        if self.contains(Self::INJECT_INPUT) {
            out.push("INJECT_INPUT");
        }
        if self.contains(Self::SUBSCRIBE_GRID) {
            out.push("SUBSCRIBE_GRID");
        }
        if self.contains(Self::PERSIST_STATE) {
            out.push("PERSIST_STATE");
        }
        out
    }
}

impl std::ops::BitOr for PermissionSet {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for PermissionSet {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Clone, Debug)]
pub struct PluginMetadata {
    pub name: &'static str,
    pub version: &'static str,
    pub api_version: u32,
    pub permissions: PermissionSet,
    /// Desired tick interval.  `0` means "no tick".  Host caps it at
    /// 100 ms minimum to keep idle CPU bounded; plugins wanting faster
    /// should subscribe to events instead.
    pub tick_interval_ms: u32,
}

#[derive(Debug)]
pub enum PluginError {
    Other(String),
    NotPermitted(PermissionSet),
    IoError(std::io::Error),
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PluginError::Other(s) => write!(f, "{}", s),
            PluginError::NotPermitted(p) => {
                write!(f, "missing permission: {:?}", p)
            }
            PluginError::IoError(e) => write!(f, "io: {}", e),
        }
    }
}

impl From<std::io::Error> for PluginError {
    fn from(e: std::io::Error) -> Self {
        PluginError::IoError(e)
    }
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
    fn set_active_plugin(&self, _name: &'static str, _permissions: PermissionSet) {}
    /// Pairs with `set_active_plugin`.
    fn clear_active_plugin(&self) {}

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

    /// Wrap a hook with `set_active_plugin` / `clear_active_plugin`
    /// so permission checks + log namespacing see the right plugin.
    fn with_active<F, R>(host: &dyn PluginHost, slot: &Slot, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        host.set_active_plugin(slot.metadata.name, slot.metadata.permissions);
        let r = f();
        host.clear_active_plugin();
        r
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
            host.set_active_plugin(slot.metadata.name, slot.metadata.permissions);
            run_hook(slot, "init", |p| p.init(host));
            host.clear_active_plugin();
        }
    }

    pub fn start_all(&mut self, host: &dyn PluginHost) {
        for slot in self.slots.iter_mut() {
            if !slot.enabled {
                continue;
            }
            host.set_active_plugin(slot.metadata.name, slot.metadata.permissions);
            run_hook(slot, "start", |p| p.start(host));
            host.clear_active_plugin();
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
            host.set_active_plugin(slot.metadata.name, slot.metadata.permissions);
            run_hook_void(slot, "tick", |p| p.tick(host));
            host.clear_active_plugin();
        }
    }

    pub fn stop_all(&mut self, host: &dyn PluginHost) {
        for slot in self.slots.iter_mut() {
            if !slot.enabled {
                continue;
            }
            host.set_active_plugin(slot.metadata.name, slot.metadata.permissions);
            run_hook_void(slot, "stop", |p| p.stop(host));
            host.clear_active_plugin();
        }
        self.started = false;
    }
}

/// Suppress "unused" warning for the with_active helper while we wait
/// for Milestone 2 to use it from a fresh path.
#[allow(dead_code)]
fn _suppress_unused() {
    let _ = PluginRegistry::with_active::<fn(), ()>;
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
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| f(&mut slot.plugin)));
    let dur = t0.elapsed();
    match result {
        Ok(Ok(())) => {
            check_budget(slot, tag, dur);
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

/// Same as `run_hook` but for `()`-returning hooks (tick/stop).
fn run_hook_void<F>(slot: &mut Slot, tag: &'static str, f: F)
where
    F: FnOnce(&mut Box<dyn Plugin>),
{
    let t0 = Instant::now();
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| f(&mut slot.plugin)));
    let dur = t0.elapsed();
    match result {
        Ok(()) => {
            check_budget(slot, tag, dur);
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

fn check_budget(slot: &mut Slot, tag: &'static str, dur: Duration) {
    if dur > HOOK_BUDGET {
        slot.consecutive_overshoots += 1;
        lx_warn!(
            "plugin.budget_overshoot",
            "plugin hook exceeded budget",
            name = slot.metadata.name,
            hook = tag,
            dur_us = dur.as_micros() as u64,
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
            dur_us = dur.as_micros() as u64
        );
    }
}

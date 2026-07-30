//! marspot-claudecode plugin — first marspot plugin.
//!
//! ## What it does (M2)
//!
//! Each tick (every 2 s), walks `~/.claude/projects/<encoded-cwd>/`
//! and finds the newest `.jsonl` per project.  Parses the first line
//! of each, extracts `sessionId`, and surfaces:
//! - one `plugin.claudecode.session` INFO line per active session
//!   (idempotent — only re-logs when sessionId or mtime changes)
//! - a `plugin.claudecode.session_done` line when an assistant
//!   message appears at the tail (for OS notification later)
//!
//! ## Why not per-pane mapping yet (M3)
//!
//! Per-pane PTY → claude pid → cwd → project chain needs L4 shelld
//! cooperation (PTY device path) or libc-heavy `proc_pidinfo`.  M2
//! surfaces the global session list so the value is visible
//! immediately; M3 wires per-pane mapping when there's signal that
//! it's needed.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};
// RFC-003 Amendment 16 cc rewrite — read sessions from L3 entry.toml
// registry (where each L3 records its shell child pid), forward
// keystrokes via L1 → L2 → L3 InjectInput frame proxy.  Monitor
// (attach_raw_only) still stubbed until the L3 PTY-broadcast wire
// frame lands.

/// Replacement for the legacy ShelldClient surface — same shape so
/// the badge / profile-cycle code below didn't have to change.  Reads
/// from the L3 entry.toml registry directly; send_input proxies
/// through the PluginHost's inject_input path.
struct ShelldClient {
    /// PluginHost handle so send_input_to can forward through the
    /// L1 → L2 → L3 wire-frame proxy.  Set by `init_with_host`
    /// when the plugin is registered with a real host (tests use
    /// `None` to skip the proxy entirely).
    host_inject: Option<Arc<dyn InjectInputProxy>>,
}

/// Indirection trait so the plugin doesn't carry a `&dyn PluginHost`
/// (the trait isn't `'static`).  Implemented by `ShellPluginHost`.
pub trait InjectInputProxy: Send + Sync {
    fn inject_input(&self, session_id: u64, bytes: &[u8]) -> std::io::Result<()>;
}

#[allow(dead_code)]
struct CcSessionInfo {
    pub session_id: u64,
    pub alive: bool,
    pub child_pid: i32,
    pub title: String,
}

impl ShelldClient {
    fn new(host_inject: Option<Arc<dyn InjectInputProxy>>) -> Self {
        Self { host_inject }
    }

    /// Walk the L3 entry.toml registry for every live session and
    /// return CcSessionInfo per entry whose pid (L3 process) is still
    /// alive.  Each entry carries shell_child_pid (zsh) — the cc
    /// badge / profile-cycle code walks pidtree from there.
    fn list_sessions(&self) -> std::io::Result<Vec<CcSessionInfo>> {
        let entries = marspot_term::session_registry::list_session_entries();
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            let alive = unsafe { libc::kill(e.pid, 0) } == 0;
            out.push(CcSessionInfo {
                session_id: e.id,
                alive,
                child_pid: e.shell_child_pid,
                title: e.title,
            });
        }
        Ok(out)
    }

    /// Forward raw bytes into the PTY backing `sid`.  Routes through
    /// L1 → L2 control socket → L3 via the InjectInput wire frame
    /// the active core knows how to dispatch.  No bracketed-paste
    /// wrapping — the caller's bytes hit the PTY verbatim, so
    /// scripts like `claude5 --resume <uuid>\r` work even inside
    /// apps that have DECSET 2004 on.
    fn send_input_to(&self, sid: u64, bytes: &[u8]) -> std::io::Result<()> {
        match self.host_inject.as_ref() {
            Some(p) => p.inject_input(sid, bytes),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "no InjectInputProxy on this host (test build?)",
            )),
        }
    }

    /// PTY-broadcast subscribe — pending the L3-side PTY fan-out
    /// frame.  Returning Err disables the C7 API-error-retry monitor
    /// until that wire lands; the badge / profile-cycle paths above
    /// still work.
    fn attach_raw_only(&self, _sid: u64) -> std::io::Result<std::sync::mpsc::Receiver<Vec<u8>>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "PTY raw broadcast TODO (no L3 fan-out frame yet)",
        ))
    }

    fn detach_raw(&self, _sid: u64) -> std::io::Result<()> {
        Ok(())
    }
}

use crate::plugins::pidtree;
use crate::plugins::{
    LogLevel, PermissionSet, Plugin, PluginError, PluginHost, PluginMetadata,
    PLUGIN_API_VERSION,
};

/// Per-session state cached so we only re-log on real change.
#[derive(Clone, Debug)]
struct SessionInfo {
    session_id: String,
    project_dir: String, // encoded form, e.g. -Users-doracawl-workspace-...
    jsonl_path: PathBuf,
    last_mtime: SystemTime,
    last_size: u64,
    last_message_kind: Option<String>,
}

pub struct ClaudecodePlugin {
    initialised: bool,
    /// Lazy shelld client.  None until first successful connect.
    /// Used to walk shelld's session table → per-session zsh.pid →
    /// pidtree → cwd → encoded project dir → sessionId.
    shelld: Option<Arc<ShelldClient>>,
    /// Previous-tick mapping of `shelld_session_id → badge string`
    /// ("P<n> <uuid>").  Drives transition logs + per-tick re-push.
    last_mapping: HashMap<u64, String>,
    /// Richer per-binding meta we need to act on a badge click:
    /// profile number, sessionId, and the live claude pid.
    last_meta: HashMap<u64, BindMeta>,
    /// Previous tick's `shelld_session_id → ("<fg|bg>,<activity>",
    /// since)`, so `cc_status.changed` is transition-only and can say
    /// how long the state it replaces had held — the number any future
    /// idle threshold has to be calibrated against.  Pruned every tick
    /// to the sessions still reporting.
    last_activity: HashMap<u64, (String, Instant)>,
    /// RFC-003 C7 auto-retry monitor: one entry per shelld session
    /// currently running claudecode.  Created when a session first
    /// binds, dropped when the bind goes away.  See `MonitorState`.
    monitors: HashMap<u64, MonitorState>,
    /// F2+2a — set true after the first `attach_raw_only` call returns
    /// `ErrorKind::Unsupported`, so subsequent ticks skip the attach
    /// loop entirely instead of retrying for every (pane, tick) pair.
    /// Without this gate, 9 claudecode panes × 0.5 Hz plugin tick
    /// fires 4.5 attach attempts / 4.5 Warn logs per second forever
    /// — 24 765 of 26 371 (94 %) log lines in a 95-min window were
    /// this single retry loop.  Reset to false on host swap (cold L2
    /// reboot) since the new host could in principle wire the
    /// missing feature.
    monitor_unsupported: bool,
    /// 2026-06-22 — tick was sync disk-IO + pidtree, which spiked to
    /// 700ms-2s under disk contention and tripped the 100ms HOOK_BUDGET
    /// 3-strike rule (permanent disable).  Heavy work is now a
    /// background `claudecode-scan` worker thread; tick only drains
    /// the result channel and pushes badges through the host.  See
    /// `WorkerCtx` and `worker_main`.
    worker: Option<JoinHandle<()>>,
    /// Send a unit to ask the worker to run another full scan pass.
    /// `None` after `stop()` so `tick` skips the send.
    scan_req_tx: Option<Sender<()>>,
    /// Newest `ScanResult` arrives here.  `tick` drains it eagerly
    /// (only the latest result matters; older ones are stale).
    /// `Mutex` purely to satisfy `Plugin: Sync` — only the L1 main
    /// loop ever touches it, so the lock is uncontended.  Mirrors
    /// the same trick used for `MonitorState.rx`.
    scan_res_rx: Option<std::sync::Mutex<Receiver<ScanResult>>>,
    /// True after `tick` queued a scan request the worker hasn't
    /// answered yet.  Stops `tick` from piling up requests if the
    /// worker is slow / stuck (worker queue would otherwise grow
    /// unbounded while plugin tick keeps firing every 2 s).
    scan_inflight: bool,
}

/// Long-running watcher for one claudecode pane.  Receives raw PTY
/// bytes via OBSERVE_PTY, looks for retryable error patterns, sends a
/// carriage return back to claude on match (with throttling so a
/// stuck error doesn't loop forever).
struct MonitorState {
    /// Channel set up by `attach_raw_only`.  Drained on every plugin
    /// tick.  Wrapped in a Mutex purely so MonitorState satisfies
    /// `Sync` for the Plugin trait bound — only the tick thread ever
    /// touches it, so the lock is uncontended.
    rx: std::sync::Mutex<std::sync::mpsc::Receiver<Vec<u8>>>,
    /// Unfinished tail bytes from the last drain — patterns are
    /// line-anchored, so a line split across two Data frames needs
    /// the carry-over.  Capped to keep a runaway pattern from
    /// growing this without bound.
    line_buf: Vec<u8>,
    /// Wall-clock of every retry we triggered for this session.
    /// Bounded by `RETRY_WINDOW`; older entries get evicted.  Used to
    /// throttle so a hard-blocking error doesn't hot-loop.
    retry_history: Vec<std::time::Instant>,
}

/// Strip ANSI CSI / OSC escape sequences from a line so the pattern
/// match doesn't have to know about colour bytes claude prints around
/// the error glyph.  Keeps printable bytes; drops control runs.
fn strip_ansi(line: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(line.len());
    let mut i = 0;
    while i < line.len() {
        let b = line[i];
        if b == 0x1b && i + 1 < line.len() {
            let nxt = line[i + 1];
            if nxt == b'[' {
                // CSI — skip until a terminator in 0x40..=0x7e
                let mut j = i + 2;
                while j < line.len() && !(0x40..=0x7e).contains(&line[j]) {
                    j += 1;
                }
                i = j + 1;
                continue;
            }
            if nxt == b']' {
                // OSC — skip until BEL or ESC\
                let mut j = i + 2;
                while j < line.len() {
                    if line[j] == 0x07 {
                        j += 1;
                        break;
                    }
                    if line[j] == 0x1b && j + 1 < line.len() && line[j + 1] == b'\\' {
                        j += 2;
                        break;
                    }
                    j += 1;
                }
                i = j;
                continue;
            }
            // Unknown ESC; skip the ESC byte alone.
            i += 1;
            continue;
        }
        if b == b'\r' {
            i += 1;
            continue;
        }
        out.push(b);
        i += 1;
    }
    out
}

/// Does this buffer (one or more lines) contain a claudecode error
/// we should retry on?  Conservative — only matches the
/// "API Error: ... · <kind>" shape.  Returns the kind tag for
/// logging.  Buffer-level (not per-line) because claude wraps long
/// errors across two grid rows — the "Rate limited" marker often
/// straddles a newline.
fn retryable_error_kind(buf: &[u8]) -> Option<&'static str> {
    let stripped = strip_ansi(buf);
    let s = match std::str::from_utf8(&stripped) {
        Ok(s) => s,
        Err(_) => return None,
    };
    if !s.contains("API Error") {
        return None;
    }
    // Whitespace-normalise: collapse \n + indent so a wrapped marker
    // like "...Rate\n  limited" reads as "...Rate limited".  Cheap
    // — runs once per fire, not per byte.
    let flat: String = s
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if flat.contains("Rate limited") || flat.contains("rate_limit") {
        return Some("rate_limited");
    }
    if flat.contains("Network error") || flat.contains("network_error") {
        return Some("network");
    }
    if flat.contains("overloaded") {
        return Some("overloaded");
    }
    if flat.contains("Server error") || flat.contains("Internal server error") {
        return Some("server_error");
    }
    None
}

#[derive(Clone, Debug)]
struct BindMeta {
    /// 0 = default `.claude`, N = `.claude-profile-N`, 255 = unknown.
    profile_num: u8,
    uuid: String,
    claude_pid: i32,
    /// Basename of the claude process's cwd — used as the pane title
    /// so user sees "marspot" instead of "session-3" once cc binds.
    /// Empty when basename couldn't be resolved (e.g. process exited
    /// between scan and tick).
    project_basename: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CycleStage {
    /// SIGTERM not yet sent to the old claude pid.
    PendingKill,
    /// SIGTERM delivered; waiting for the pid to vanish from the
    /// process table.
    KillSent,
    /// `claudeN --resume <uuid>\r` written to the PTY; waiting a
    /// settle window before ending the PaneSession.
    ResumeSent,
}

/// RFC-003 §10 PaneSession that drives the claudecode profile cycle:
/// freeze the pane visually, lock the keyboard, exit → resume.
struct ProfileCyclePaneSession {
    client: Arc<ShelldClient>,
    next_profile: u8,
    uuid: String,
    /// claude pid we're waiting to die before sending the resume
    /// command.
    old_claude_pid: i32,
    stage: CycleStage,
    /// Used as a per-stage timer + overall watchdog.
    started_at: SystemTime,
    /// Tick counter for the badge spinner cycle.
    spin_phase: u8,
}

impl ProfileCyclePaneSession {
    /// Braille spinner — 8 frames, advanced one step per on_tick.
    /// Standard 1/8 turn frames, same characters most CLI spinners use.
    fn spinner_frame(phase: u8) -> char {
        const FRAMES: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];
        FRAMES[(phase as usize) % FRAMES.len()]
    }

    fn pid_alive(pid: i32) -> bool {
        // kill(pid, 0) — no signal, just permission/existence check.
        // 0 = process exists and we can signal; -1 with ESRCH = gone.
        unsafe {
            if libc::kill(pid, 0) == 0 {
                return true;
            }
            *libc::__error() != libc::ESRCH
        }
    }
}

impl crate::plugins::PaneSession for ProfileCyclePaneSession {
    fn caps(&self) -> u32 {
        marspot::shell_proto::PANE_SESSION_CAP_INPUT
            | marspot::shell_proto::PANE_SESSION_CAP_LOCK_KEYS
            | marspot::shell_proto::PANE_SESSION_CAP_FREEZE_GRID
    }

    fn on_user_key(
        &mut self,
        _host: &dyn crate::plugins::PaneSessionHost,
        ev: &marspot::shell_proto::WireKeyEvent,
    ) -> crate::plugins::KeyHandling {
        // Esc → bail out; everything else gets swallowed so the
        // user can't pollute the resume command mid-cycle.
        if matches!(
            ev.kind,
            marspot::shell_proto::WireLogicalKind::Named,
        ) && ev.key_data
            == marspot::shell_proto::WireNamedKey::Escape as u32
        {
            crate::plugins::KeyHandling::EndSession
        } else {
            crate::plugins::KeyHandling::Swallow
        }
    }

    fn on_tick(&mut self, host: &dyn crate::plugins::PaneSessionHost) {
        let sid = host.shelld_session_id();
        let now = SystemTime::now();
        let elapsed = now
            .duration_since(self.started_at)
            .unwrap_or_default();
        // Advance the spinner every tick, then refresh the badge so
        // the user sees motion even when the state machine is
        // mid-stage (e.g. waiting for the claude pid to die).
        self.spin_phase = self.spin_phase.wrapping_add(1);
        match self.stage {
            CycleStage::KillSent => host.set_badge(&format!(
                "→ P{} {}",
                self.next_profile,
                Self::spinner_frame(self.spin_phase)
            )),
            CycleStage::ResumeSent => host.set_badge(&format!(
                "P{} starting {}",
                self.next_profile,
                Self::spinner_frame(self.spin_phase)
            )),
            CycleStage::PendingKill => {} // first tick sets it below
        }
        // Global watchdog: 30 s of no-progress kills the session.
        if elapsed > std::time::Duration::from_secs(30) {
            host.log(
                crate::plugins::LogLevel::Warn,
                "cycle.timed_out",
                "stale cycle; aborting",
            );
            host.end();
            return;
        }
        match self.stage {
            CycleStage::PendingKill => {
                // SIGTERM directly to the claude PID — bypass the
                // PTY entirely so no `Bye!` / `/exit` echo lands in
                // the grid.  `bare exit` round-trip was ambiguous
                // (claude treats it as a user message and replies
                // politely without quitting), `/exit` works but
                // prints "Bye!", SIGTERM kills cleanly.
                let r = unsafe { libc::kill(self.old_claude_pid, libc::SIGTERM) };
                if r != 0 {
                    let errno = unsafe { *libc::__error() };
                    if errno == libc::ESRCH {
                        // Already gone — race with normal exit.
                        // Treat as success.
                    } else {
                        host.log(
                            crate::plugins::LogLevel::Warn,
                            "cycle.kill_failed",
                            &format!("errno={}", errno),
                        );
                        host.end();
                        return;
                    }
                }
                host.log(
                    crate::plugins::LogLevel::Info,
                    "cycle.kill_sent",
                    &format!(
                        "shelld_session={} → P{} (uuid={}, claude_pid={})",
                        sid, self.next_profile, self.uuid, self.old_claude_pid
                    ),
                );
                host.set_badge(&format!(
                    "→ P{} {}",
                    self.next_profile,
                    Self::spinner_frame(self.spin_phase)
                ));
                self.stage = CycleStage::KillSent;
                self.started_at = now;
            }
            CycleStage::KillSent => {
                if !Self::pid_alive(self.old_claude_pid) {
                    let cmd = format!(
                        "claude{} --resume {}\r",
                        self.next_profile, self.uuid
                    );
                    if let Err(e) = self.client.send_input_to(sid, cmd.as_bytes()) {
                        host.log(
                            crate::plugins::LogLevel::Warn,
                            "cycle.resume_send_failed",
                            &format!("{e}"),
                        );
                        host.end();
                        return;
                    }
                    host.log(
                        crate::plugins::LogLevel::Info,
                        "cycle.resume_sent",
                        &format!(
                            "shelld_session={} P{} resume (uuid={})",
                            sid, self.next_profile, self.uuid
                        ),
                    );
                    host.set_badge(&format!(
                        "P{} starting {}",
                        self.next_profile,
                        Self::spinner_frame(self.spin_phase)
                    ));
                    self.stage = CycleStage::ResumeSent;
                    self.started_at = now;
                } else if elapsed >= std::time::Duration::from_secs(3) {
                    // SIGTERM didn't take in 3 s — escalate to SIGKILL.
                    let r = unsafe { libc::kill(self.old_claude_pid, libc::SIGKILL) };
                    host.log(
                        crate::plugins::LogLevel::Warn,
                        "cycle.escalated_sigkill",
                        &format!("pid={} kill_r={}", self.old_claude_pid, r),
                    );
                }
            }
            CycleStage::ResumeSent => {
                // Short settle so the spawn echo doesn't flash.  Was
                // 2 s — but on big sessions claude's first frame takes
                // 10 s+ anyway (317 MB jsonl parse measured
                // 2026-07-13), so a long freeze here buys nothing and
                // just adds to the perceived switch lag; small
                // sessions paint within ~600 ms.
                if elapsed >= std::time::Duration::from_millis(600) {
                    host.end();
                }
            }
        }
    }

    fn on_end(
        &mut self,
        host: &dyn crate::plugins::PaneSessionHost,
        reason: crate::plugins::EndReason,
    ) {
        host.log(
            crate::plugins::LogLevel::Info,
            "cycle.end",
            &format!("sid={} reason={:?}", host.shelld_session_id(), reason),
        );
        // Don't clear the badge here — the regular plugin tick will
        // re-bind the new claude pid → re-issue a fresh badge with
        // the new profile tag.  Clearing causes a brief blank between
        // end and next tick.
    }
}

impl ClaudecodePlugin {
    pub fn new() -> Self {
        Self {
            initialised: false,
            shelld: None,
            last_mapping: HashMap::new(),
            last_meta: HashMap::new(),
            last_activity: HashMap::new(),
            monitors: HashMap::new(),
            monitor_unsupported: false,
            worker: None,
            scan_req_tx: None,
            scan_res_rx: None,
            scan_inflight: false,
        }
    }

    /// RFC-003 C7 (observe-only): drain raw PTY bytes for every
    /// active monitor and log any retryable error markers.  No
    /// auto-action yet — user wants the wire proven without the
    /// "press Enter for them" half landing as policy.  Action is
    /// added later when there's a concrete decision to make per
    /// `kind`; the throttle counter still ticks so we can see how
    /// often patterns fire in practice.
    fn pump_monitors(&mut self, host: &dyn PluginHost) {
        const MATCH_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
        const MAX_LINE_BUF: usize = 16 * 1024;

        let now = std::time::Instant::now();
        for (sid, mon) in self.monitors.iter_mut() {
            // Drain raw bytes; cheap when nothing new.
            let mut pending: Vec<u8> = Vec::new();
            {
                let rx = mon.rx.lock().unwrap();
                while let Ok(chunk) = rx.try_recv() {
                    pending.extend_from_slice(&chunk);
                }
            }
            if pending.is_empty() {
                continue;
            }
            mon.line_buf.extend_from_slice(&pending);
            if mon.line_buf.len() > MAX_LINE_BUF {
                let keep = mon.line_buf.len() - MAX_LINE_BUF / 2;
                mon.line_buf.drain(..keep);
            }
            let Some(kind) = retryable_error_kind(&mon.line_buf) else { continue };
            mon.line_buf.clear();
            // Update the rolling window so we can SEE rate even if
            // we don't act on it.
            mon.retry_history.retain(|t| now.duration_since(*t) <= MATCH_WINDOW);
            mon.retry_history.push(now);
            host.log(
                LogLevel::Info,
                "monitor.match",
                &format!(
                    "shelld_session={} kind={} count_in_window={}",
                    sid,
                    kind,
                    mon.retry_history.len()
                ),
            );
        }
    }

    /// Spin up monitors for newly-bound sessions; drop monitors whose
    /// bind went away.  Called from tick() after `last_meta` is
    /// updated, so it reflects the current bound set.
    fn refresh_monitors(&mut self, host: &dyn PluginHost) {
        let Some(client) = self.shelld.as_ref() else { return };
        // F2+2a — `attach_raw_only` is a hard-coded `Err(Unsupported)`
        // until the L3 PTY fan-out wire ships (see line 105 TODO).
        // Without this gate, every plugin tick re-attempts attach for
        // every bound session and logs `monitor.attach_failed` each
        // time — measured as 4.5 lines / s across 9 panes, 94 % of
        // all log volume.  Once the first attempt comes back
        // `Unsupported`, latch the flag and skip the whole loop.
        if self.monitor_unsupported {
            return;
        }
        // Add monitors for newly-bound sessions.
        let mut new_keys: Vec<u64> = Vec::new();
        for sid in self.last_meta.keys() {
            if !self.monitors.contains_key(sid) {
                new_keys.push(*sid);
            }
        }
        for sid in new_keys {
            match client.attach_raw_only(sid) {
                Ok(rx) => {
                    self.monitors.insert(
                        sid,
                        MonitorState {
                            rx: std::sync::Mutex::new(rx),
                            line_buf: Vec::new(),
                            retry_history: Vec::new(),
                        },
                    );
                    host.log(
                        LogLevel::Info,
                        "monitor.start",
                        &format!("shelld_session={}", sid),
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
                    // Permanent: the host doesn't (yet) expose PTY
                    // raw broadcast.  Log once at Info, latch the
                    // flag so subsequent ticks short-circuit.
                    host.log(
                        LogLevel::Info,
                        "monitor.disabled",
                        &format!(
                            "attach_raw_only unsupported by host ({}); \
                             monitor feature disabled until wire lands",
                            e
                        ),
                    );
                    self.monitor_unsupported = true;
                    break;
                }
                Err(e) => {
                    host.log(
                        LogLevel::Warn,
                        "monitor.attach_failed",
                        &format!("shelld_session={} err={}", sid, e),
                    );
                }
            }
        }
        // Drop monitors whose bind went away.
        let stale: Vec<u64> = self
            .monitors
            .keys()
            .copied()
            .filter(|sid| !self.last_meta.contains_key(sid))
            .collect();
        for sid in stale {
            self.monitors.remove(&sid);
            // detach_raw returns Result; ignore the error here — the
            // pane is going away regardless, and a stale ATTACH that
            // failed to clean up is harmless (next L3 reattach overrides).
            let _ = client.detach_raw(sid);
            host.log(
                LogLevel::Info,
                "monitor.stop",
                &format!("shelld_session={}", sid),
            );
        }
    }

    /// Kick off the profile cycle for `shelld_session_id`.  Called
    /// from `on_pane_badge_click`.  Builds a ProfileCyclePaneSession
    /// and hands it to the host; the host freezes the grid + locks
    /// the keyboard while the state machine runs.
    fn start_profile_cycle(
        &mut self,
        host: &dyn PluginHost,
        shelld_sid: u64,
    ) {
        let Some(meta) = self.last_meta.get(&shelld_sid).cloned() else {
            host.log(
                LogLevel::Warn,
                "cycle.no_bind",
                &format!("badge click on unbound shelld_session={}", shelld_sid),
            );
            return;
        };
        let profiles = discover_profiles();
        let Some(&lowest) = profiles.first() else {
            host.log(
                LogLevel::Warn,
                "cycle.no_profiles",
                "no ~/.claude-profile-N dirs; cannot cycle",
            );
            return;
        };
        // Next-highest existing profile, wrapping to the lowest.  P0
        // (default `.claude`) and unknown (255) both land on the
        // lowest numbered profile.
        let next_profile = profiles
            .iter()
            .copied()
            .find(|&n| n > meta.profile_num)
            .unwrap_or(lowest);
        host.log(
            LogLevel::Info,
            "cycle.profiles",
            &format!(
                "discovered={:?} current=P{} next=P{}",
                profiles, meta.profile_num, next_profile
            ),
        );
        self.start_profile_cycle_to(host, shelld_sid, meta, next_profile);
    }

    /// Start the exit → resume cycle towards an explicit target
    /// profile.  Shared tail of the badge left-click (auto-next) and
    /// the badge context-menu "switch to Px" pick (explicit target).
    fn start_profile_cycle_to(
        &mut self,
        host: &dyn PluginHost,
        shelld_sid: u64,
        meta: BindMeta,
        next_profile: u8,
    ) {
        let Some(client) = self.shelld.as_ref().cloned() else {
            host.log(
                LogLevel::Warn,
                "cycle.no_shelld",
                "no shelld client; cannot start cycle",
            );
            return;
        };
        let session = Box::new(ProfileCyclePaneSession {
            client,
            next_profile,
            uuid: meta.uuid,
            old_claude_pid: meta.claude_pid,
            stage: CycleStage::PendingKill,
            started_at: SystemTime::now(),
            spin_phase: 0,
        });
        if let Err(e) = host.begin_pane_session(shelld_sid, session) {
            host.log(
                LogLevel::Warn,
                "cycle.begin_pane_session_failed",
                &format!("{e}"),
            );
        }
    }

}

/// Scan `$HOME` for `.claude-profile-N` directories and return the
/// profile numbers, sorted ascending.  This is the source of truth
/// for the badge-click cycle — add a `~/.claude-profile-5` dir (plus
/// its `claude5` shell alias) and the cycle picks it up on the next
/// click, no code change.  The default `.claude` (P0) is deliberately
/// not part of the cycle, matching the previous hardcoded 1→2→3 loop.
fn discover_profiles() -> Vec<u8> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    discover_profiles_in(std::path::Path::new(&home))
}

fn discover_profiles_in(home: &std::path::Path) -> Vec<u8> {
    let Ok(rd) = std::fs::read_dir(home) else {
        return Vec::new();
    };
    let mut nums: Vec<u8> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_prefix(".claude-profile-")?
                .parse::<u8>()
                .ok()
        })
        .collect();
    nums.sort_unstable();
    nums
}

/// Build the badge right-click menu rows: one "switch to Px" per
/// discovered profile except the pane's current one.  Tag = the
/// profile number (echoed back via `on_pane_badge_menu_action`).
/// Pure so the row shape is unit-testable without a $HOME fixture.
fn badge_menu_for(
    current: u8,
    profiles: &[u8],
) -> Vec<marspot::shell_proto::PaneBadgeMenuItem> {
    profiles
        .iter()
        .copied()
        .filter(|&n| n != current)
        .map(|n| marspot::shell_proto::PaneBadgeMenuItem {
            tag: n as u32,
            label: format!("switch to P{}", n),
        })
        .collect()
}

/// How much of the session jsonl tail to search for the active
/// model.  Single records (big tool_results) can run tens of KB, so
/// the window must comfortably span several of them.
const MODEL_TAIL_BYTES: u64 = 262_144;

/// Tail the session jsonl and return the active model as a short
/// display token (e.g. `fable-5`).  Two producers, newest-in-file
/// wins:
///
///   - assistant records — authoritative, the model that actually
///     served the turn: `"role":"assistant"` + `"model":"claude-…"`
///   - `/model` slash-command output — `"subtype":"local_command"`
///     with `Set model to …` / `Kept model as …` in its content,
///     written the moment the user switches, so the badge follows a
///     `/model` change on the next 2 s tick instead of waiting for
///     the next assistant turn
///
/// Returns None when neither appears in the tail window (fresh
/// session, or a single giant record swamping the window) — the
/// badge then renders without the `@model` part.
fn tail_model_short(path: &std::path::Path, min_offset: u64) -> Option<String> {
    use std::cell::RefCell;
    // (mtime, size)-keyed memo so the 2 s scan tick only re-reads a
    // session's tail when the jsonl actually grew — idle panes cost
    // one `stat` per tick, not a 256 KB read.  Worker-thread-local;
    // capped so dead sessions can't accumulate entries forever.
    thread_local! {
        static CACHE: RefCell<
            HashMap<PathBuf, (SystemTime, u64, u64, Option<String>)>,
        > = RefCell::new(HashMap::new());
    }
    const CACHE_CAP: usize = 64;
    let md = fs::metadata(path).ok()?;
    let mtime = md.modified().ok()?;
    let size = md.len();
    let hit = CACHE.with(|c| {
        c.borrow().get(path).and_then(|(t, s, off, v)| {
            (*t == mtime && *s == size && *off == min_offset).then(|| v.clone())
        })
    });
    if let Some(v) = hit {
        return v;
    }
    let result = tail_model_short_uncached(path, min_offset);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= CACHE_CAP {
            c.clear();
        }
        c.insert(path.to_path_buf(), (mtime, size, min_offset, result.clone()));
    });
    result
}

fn tail_model_short_uncached(path: &std::path::Path, min_offset: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    // Never read behind the fence — those records describe a process
    // that no longer owns this session.
    let start = len.saturating_sub(MODEL_TAIL_BYTES).max(min_offset.min(len));
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = String::new();
    // Lossy is fine: we only pattern-scan ASCII keys, and a torn
    // first line simply won't match.
    let mut raw = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut raw).ok()?;
    buf.push_str(&String::from_utf8_lossy(&raw));
    for line in buf.lines().rev() {
        // /model output records vary by claudecode version: some
        // write `"type":"system","subtype":"local_command"`, some a
        // `"type":"user"` record whose content is the
        // `<local-command-stdout>` block.  Anchoring on the marker as
        // the DIRECT value of a content field (`"content":"<local-…`,
        // quotes unescaped) covers both — and is what makes the match
        // collision-proof against conversation text that merely
        // QUOTES these strings (a session where marspot itself is
        // being developed does exactly that): inside a text field the
        // quotes around `"content":` are JSON-escaped to `\"`, so the
        // unescaped anchor cannot occur.  The display name may carry
        // a trailing remark ("… and saved as your default for new
        // sessions"), so the cut also stops at " and ".
        for marker in [
            "\"content\":\"<local-command-stdout>Set model to ",
            "\"content\":\"<local-command-stdout>Kept model as ",
        ] {
            if let Some(i) = line.find(marker) {
                let rest = &line[i + marker.len()..];
                let mut end = rest.find(['<', '"']).unwrap_or(rest.len());
                if let Some(a) = rest.find(" and ") {
                    end = end.min(a);
                }
                let name = short_model(&rest[..end]);
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
        if line.contains("\"role\":\"assistant\"") {
            if let Some(i) = line.find("\"model\":\"") {
                let rest = &line[i + 9..];
                if let Some(end) = rest.find('"') {
                    let name = short_model(&rest[..end]);
                    if !name.is_empty() {
                        return Some(name);
                    }
                }
            }
        }
    }
    None
}

/// Normalise a model identifier or display name into the short badge
/// token: strip ANSI escapes, the `claude-` prefix, a trailing
/// `-YYYYMMDD` snapshot date, and any parenthesised remark; lowercase
/// and map spaces / dots to `-` so `Opus 4.8` and `claude-opus-4-8`
/// both come out as `opus-4-8`.  Capped at 16 chars — the badge
/// shares the title strip with the session uuid.
fn short_model(raw: &str) -> String {
    // jsonl strings carry control chars JSON-escaped — decode the
    // literal `\u001b` spelling into a real ESC before stripping.
    let raw = raw.replace("\\u001b", "\u{1b}");
    let raw = raw.as_str();
    // Strip ANSI CSI sequences (`ESC [ … letter`).
    let mut cleaned = String::with_capacity(raw.len());
    let mut it = raw.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            if it.peek() == Some(&'[') {
                it.next();
                for e in it.by_ref() {
                    if e.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        cleaned.push(c);
    }
    let cleaned = cleaned.trim();
    // Drop a parenthesised remark: "Default (recommended)" → "Default".
    let cleaned = match cleaned.find('(') {
        Some(i) => cleaned[..i].trim_end(),
        None => cleaned,
    };
    let cleaned = cleaned
        .strip_prefix("claude-")
        .unwrap_or(cleaned);
    // Trailing snapshot date: "-20251001".
    let cleaned = match cleaned.rfind('-') {
        Some(i)
            if cleaned.len() - i == 9
                && cleaned[i + 1..].bytes().all(|b| b.is_ascii_digit()) =>
        {
            &cleaned[..i]
        }
        _ => cleaned,
    };
    // Model identifiers and display names are ASCII words — any
    // other character means we grabbed prose, not a model; reject
    // the whole thing rather than render garbage in the badge.
    let mut out = String::with_capacity(cleaned.len());
    for c in cleaned.chars() {
        let mapped = match c {
            ' ' | '.' => '-',
            c if c.is_ascii_alphanumeric() || c == '-' => c.to_ascii_lowercase(),
            _ => return String::new(),
        };
        out.push(mapped);
        if out.len() >= 16 {
            break;
        }
    }
    out.trim_matches('-').to_string()
}

/// Read `CLAUDE_CONFIG_DIR` off the running `claude` pid and parse a
/// short profile tag.  Returns:
///   * `Some("P1")` for `/Users/.../.claude-profile-1`
///   * `Some("P0")` for the default `/Users/.../.claude` (no -profile-N)
///   * `None`       when the env var isn't set / the read failed
fn profile_tag_for(claude_pid: i32) -> Option<String> {
    let dir = pidtree::proc_env_value(claude_pid, "CLAUDE_CONFIG_DIR")?;
    // Trailing slash tolerant; basename only.
    let base = std::path::Path::new(&dir).file_name()?.to_string_lossy().into_owned();
    if let Some(num) = base.strip_prefix(".claude-profile-") {
        return Some(format!("P{}", num));
    }
    if base == ".claude" {
        return Some("P0".to_string());
    }
    // Unknown shape — fall back to "P?" so the user can still tell
    // "the badge is missing" from "the badge is unrecognised".
    Some("P?".to_string())
}

/// Encode a filesystem path into claude's project directory naming
/// convention (`/Users/foo/bar` → `-Users-foo-bar`).
fn encode_project_dir(cwd: &std::path::Path) -> String {
    let mut s = String::with_capacity(cwd.as_os_str().len());
    for c in cwd.to_string_lossy().chars() {
        if c == '/' {
            s.push('-');
        } else {
            s.push(c);
        }
    }
    s
}

/// Heuristic: is this descendant the `claude` CLI?  Walks argv —
/// the comm fast-path was a dead end because recent claude builds
/// mangle the process name to the version string ("2.1.177") via
/// `exec -a`, so a `comm == "claude"` prefilter would miss every
/// real claudecode process.  argv[0] survives the rename (it's the
/// original exec path), so cmdline → split → check argv[0]'s
/// basename equals "claude" is the reliable signal.
fn looks_like_claudecode(d: &pidtree::ProcRow) -> bool {
    let line = match pidtree::proc_cmdline(d.pid) {
        Some(l) => l,
        None => return false,
    };
    let argv0 = match line.split(' ').next() {
        Some(s) => s,
        None => return false,
    };
    let basename = argv0.rsplit('/').next().unwrap_or(argv0);
    basename == "claude"
}

impl Default for ClaudecodePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for ClaudecodePlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: "claudecode",
            version: "0.1.0",
            api_version: PLUGIN_API_VERSION,
            permissions: PermissionSet::READ_PANE_INFO
                | PermissionSet::READ_PTY_TREE
                | PermissionSet::READ_DISK_FS
                | PermissionSet::NOTIFY_USER
                | PermissionSet::SET_STATUS_LINE
                | PermissionSet::PERSIST_STATE,
            // 2 s — long enough to be near-free,short enough to catch
            // "assistant message done" within a couple seconds for
            // future notification feature.
            tick_interval_ms: 2000,
        }
    }

    fn init(&mut self, host: &dyn PluginHost) -> Result<(), PluginError> {
        // Persistence dir set up now so PluginError::IoError surfaces
        // a bad config early instead of mid-tick.
        let _ = host.state_dir()?;
        // Resolve ~/.claude/projects.  Without HOME there's nothing
        // to do — disable cleanly via a soft Err.
        let home = std::env::var_os("HOME").ok_or_else(|| {
            PluginError::Other("HOME not set; claudecode plugin idle".into())
        })?;
        let projects_root = PathBuf::from(home).join(".claude").join("projects");
        // RFC-003 Amendment 16 cc: read sessions from L3 entry.toml
        // registry instead of shelld.  ShelldClient is now a thin
        // façade over `session_registry::list_session_entries()` and
        // the L1→L2→L3 InjectInput wire-frame proxy (set later by
        // the registry via `attach_inject_proxy`).
        let proxy = host.cc_inject_proxy();
        let shelld = Arc::new(ShelldClient::new(proxy));
        self.shelld = Some(shelld.clone());
        host.log(
            LogLevel::Info,
            "init.registry_walker",
            "cc reading sessions from L3 entry.toml + InjectInput proxy",
        );
        host.log(
            LogLevel::Info,
            "init",
            &format!(
                "claudecode plugin initialised (projects_root={})",
                projects_root.display()
            ),
        );

        // Spin up the background scan worker.  All disk IO + pidtree
        // walks happen there; `tick` only drains the result channel
        // and pushes badges through the host.  Without this, the
        // tick blocked on `~/.claude/projects` IO and spiked to
        // 700ms-2s under disk contention — 3 such ticks tripped the
        // 100ms HOOK_BUDGET strike rule and the host permanently
        // disabled the plugin.
        let (req_tx, req_rx) = mpsc::channel::<()>();
        let (res_tx, res_rx) = mpsc::channel::<ScanResult>();
        let ctx = WorkerCtx {
            projects_root,
            shelld,
            seen: HashMap::new(),
            model_cutoff: HashMap::new(),
        };
        let handle = std::thread::Builder::new()
            .name("claudecode-scan".into())
            .spawn(move || worker_main(ctx, req_rx, res_tx))
            .map_err(|e| {
                PluginError::Other(format!("spawn claudecode-scan worker: {e}"))
            })?;
        self.worker = Some(handle);
        self.scan_req_tx = Some(req_tx);
        self.scan_res_rx = Some(std::sync::Mutex::new(res_rx));
        self.scan_inflight = false;

        self.initialised = true;
        Ok(())
    }

    fn tick(&mut self, host: &dyn PluginHost) {
        if !self.initialised {
            return;
        }
        // (1) Drain any results the worker delivered since last tick.
        // Apply only the newest — older results are stale (the worker
        // already overrode their mapping in its next pass).
        let mut latest: Option<ScanResult> = None;
        let mut disconnected = false;
        if let Some(rx_lock) = &self.scan_res_rx {
            let rx = rx_lock.lock().unwrap();
            loop {
                match rx.try_recv() {
                    Ok(r) => {
                        latest = Some(r);
                        self.scan_inflight = false;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        if disconnected {
            host.log(
                LogLevel::Warn,
                "worker.gone",
                "scan worker channel closed; plugin idle until restart",
            );
            self.scan_res_rx = None;
            self.scan_req_tx = None;
            self.initialised = false;
            return;
        }
        if let Some(result) = latest {
            // Replay log lines the worker accumulated.  Stays cheap:
            // a typical scan emits 0 lines (no transitions); only
            // first-discovery / mapping change ticks have any.
            for (lvl, tag, msg) in &result.log_lines {
                host.log(*lvl, tag, msg);
            }
            // Log mapping transitions (new bindings / lost bindings)
            // here on the main side — worker can't diff vs the host-
            // owned `self.last_mapping` cheaply.  Steady-state ticks
            // emit zero lines, so the file stays quiet.
            for (sh_sid, cc_sid) in &result.new_mapping {
                let prev = self.last_mapping.get(sh_sid);
                if prev.map(|p| p != cc_sid).unwrap_or(true) {
                    host.log(
                        LogLevel::Info,
                        "session.bound",
                        &format!(
                            "shelld_session={} → claudecode sid={}",
                            sh_sid, cc_sid
                        ),
                    );
                }
            }
            for (sh_sid, cc_sid) in &self.last_mapping {
                if !result.new_mapping.contains_key(sh_sid) {
                    host.log(
                        LogLevel::Info,
                        "session.unbound",
                        &format!(
                            "shelld_session={} (was sid={})",
                            sh_sid, cc_sid
                        ),
                    );
                    let _ = host.set_pane_badge(*sh_sid, "");
                    let _ = host.set_pane_title(*sh_sid, "");
                }
            }
            // Re-push every active badge + title every tick (idempotent).
            // Why not transition-only: L2 core can spawn/crash/
            // respawn between ticks (CORE_BOOT_LOOP, silent update);
            // transition-only would leave the fresh core with no
            // badges until something changes.  Per tick ≤ 9 small
            // frames = a few hundred bytes.
            for (sh_sid, cc_sid) in &result.new_mapping {
                if let Err(e) = host.set_pane_badge(*sh_sid, cc_sid) {
                    host.log(
                        LogLevel::Warn,
                        "pane_badge.set_failed",
                        &format!("{e}"),
                    );
                }
                if let Some(meta) = result.new_meta.get(sh_sid) {
                    if !meta.project_basename.is_empty() {
                        // Title 只用 project basename — profile 已经在
                        // badge 里("P3 …"),title 再重复就冗余.
                        if let Err(e) = host.set_pane_title(
                            *sh_sid,
                            &meta.project_basename,
                        ) {
                            host.log(
                                LogLevel::Warn,
                                "pane_title.set_failed",
                                &format!("{e}"),
                            );
                        }
                    }
                }
            }
            // cc status transitions.  Logged on the main side because
            // that is where the previous tick's map lives — and where
            // the generic layer can be asked, which the worker thread
            // has no host to do.
            //
            // The two layers are reported together on purpose: the
            // generic one answers "does claude own the pane's keyboard"
            // (a suspended or backgrounded claude reads as `bg` even
            // while its jsonl says it was mid-turn), the cc one answers
            // "what is it doing in there".  Neither is derivable from
            // the other.
            for (sid, activity) in &result.new_activity {
                let fg = match host.pane_status(*sid).map(|o| o.map(|(fg, _age)| fg)) {
                    Ok(Some(marspot::pidtree::PaneForeground::Job { .. })) => "fg",
                    Ok(Some(marspot::pidtree::PaneForeground::AtPrompt)) => "bg",
                    // No entry yet / no controlling tty / the sweep
                    // can't say.  Not "fg", not "bg" — unknown.
                    _ => "fg?",
                };
                let next = format!("{},{}", fg, activity.describe());
                let prev = self.last_activity.get(sid);
                let since = match prev {
                    // Unchanged: keep the original stamp so the age
                    // measures the state, not the tick.
                    Some((p, since)) if p == &next => *since,
                    _ => {
                        host.log(
                            LogLevel::Info,
                            "cc_status.changed",
                            &format!(
                                "shelld_session={} {} → {} (held {}s)",
                                sid,
                                prev.map(|(s, _)| s.as_str()).unwrap_or("-"),
                                next,
                                prev.map(|(_, t): &(String, Instant)| {
                                    t.elapsed().as_secs()
                                })
                                .unwrap_or(0),
                            ),
                        );
                        Instant::now()
                    }
                };
                self.last_activity.insert(*sid, (next, since));
            }
            // Sessions that stopped reporting (claude exited, pane
            // closed) drop out — same bound-by-construction rule the
            // generic sweep follows.
            self.last_activity
                .retain(|sid, _| result.new_activity.contains_key(sid));
            self.last_mapping = result.new_mapping;
            self.last_meta = result.new_meta;
        }

        // (2) Kick off the next scan if the worker is idle.  At most
        // one in-flight request: if the worker is still chewing on
        // the previous (slow disk), we skip — preserves the worker
        // queue from growing unbounded.
        if !self.scan_inflight {
            if let Some(tx) = &self.scan_req_tx {
                if tx.send(()).is_ok() {
                    self.scan_inflight = true;
                } else {
                    host.log(
                        LogLevel::Warn,
                        "worker.send_failed",
                        "scan worker dropped request channel",
                    );
                    self.scan_req_tx = None;
                    self.initialised = false;
                    return;
                }
            }
        }

        // (3) RFC-003 profile-cycle state machine lives in
        // ProfileCyclePaneSession::on_tick, driven by the L1 plugin
        // dispatcher (not here).
        //
        // C7 auto-retry monitors: cheap host-side bookkeeping, no
        // disk IO.  `monitor_unsupported` latches true on first
        // tick (PTY raw broadcast not wired yet) so the body is a
        // no-op in steady state.
        self.refresh_monitors(host);
        self.pump_monitors(host);
    }

    fn on_pane_badge_click(
        &mut self,
        host: &dyn PluginHost,
        shelld_session_id: u64,
    ) {
        self.start_profile_cycle(host, shelld_session_id);
    }

    fn pane_badge_menu(
        &mut self,
        _host: &dyn PluginHost,
        shelld_session_id: u64,
    ) -> Vec<marspot::shell_proto::PaneBadgeMenuItem> {
        let Some(meta) = self.last_meta.get(&shelld_session_id) else {
            return Vec::new();
        };
        badge_menu_for(meta.profile_num, &discover_profiles())
    }

    fn on_pane_badge_menu_action(
        &mut self,
        host: &dyn PluginHost,
        shelld_session_id: u64,
        tag: u32,
    ) {
        // Tags this plugin assigns are profile numbers (fit in u8);
        // anything else belongs to another plugin's rows.
        let Ok(target) = u8::try_from(tag) else {
            return;
        };
        let Some(meta) = self.last_meta.get(&shelld_session_id).cloned() else {
            host.log(
                LogLevel::Warn,
                "cycle.menu_no_bind",
                &format!("menu pick on unbound shelld_session={}", shelld_session_id),
            );
            return;
        };
        if meta.profile_num == target {
            // Stale menu — the pane already cycled here.  Nothing to do.
            return;
        }
        host.log(
            LogLevel::Info,
            "cycle.menu_pick",
            &format!(
                "shelld_session={} P{} → P{}",
                shelld_session_id, meta.profile_num, target
            ),
        );
        self.start_profile_cycle_to(host, shelld_session_id, meta, target);
    }

    fn stop(&mut self, host: &dyn PluginHost) {
        host.log(LogLevel::Info, "stop", "plugin stopped");
        self.initialised = false;
        // Drop the request channel — the worker's recv loop sees Err
        // and exits cleanly.  Then join so the thread is fully torn
        // down before the plugin slot is dropped (otherwise the
        // thread would outlive the plugin briefly and the next
        // start would race the previous one's last send).
        self.scan_req_tx = None;
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
        self.scan_res_rx = None;
        self.scan_inflight = false;
    }
}

// ============================================================
// Background scan worker — owns all disk IO + pidtree walks
// that previously ran on the plugin tick.  See `ClaudecodePlugin`
// field docs and `init()` for the wiring.
// ============================================================

/// What `WorkerCtx::scan_once` returns to `tick`.  Owned, Send-safe.
/// What claudecode is doing in a pane — the cc-specific layer sitting
/// on top of the generic `PaneForeground` (which only knows "a job owns
/// this tty").  Read off the session jsonl's last record, whose shapes
/// were taken from a live transcript rather than guessed:
///
/// ```text
/// {"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use",…}]}}
/// {"type":"user",     "message":{"role":"user",     "content":[{"type":"tool_result",…}]}}
/// {"type":"assistant","message":{"role":"assistant","content":[{"type":"text"|"thinking",…}]}}
/// ```
///
/// The distinction that matters to anything acting on this: only
/// `AwaitingUser` means nothing is in flight.  Every other variant —
/// including `Unknown` — has to be treated as "do not touch this pane".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CcActivity {
    /// The assistant's turn ended with a message; the prompt belongs
    /// to the user now.
    AwaitingUser,
    /// A tool call is outstanding — the last record asks for one and
    /// no result followed it.  `executing` is best-effort: true when
    /// claude has a child process younger than that record (the tool
    /// is running), false when it has none (so claude is most likely
    /// parked on the approval prompt).  Both mean "in flight"; the
    /// split is for the log, not for permission.
    ToolPending { executing: bool },
    /// A turn is being produced: the last record is the user's message
    /// or a tool result, so the assistant is the one who owes the next
    /// record.
    Working,
    /// Tail unreadable, or a record shape this doesn't model.  Not a
    /// synonym for idle.
    Unknown,
}

impl CcActivity {
    fn describe(self) -> &'static str {
        match self {
            CcActivity::AwaitingUser => "awaiting_user",
            CcActivity::ToolPending { executing: true } => "tool_executing",
            CcActivity::ToolPending { executing: false } => "tool_awaiting_approval",
            CcActivity::Working => "working",
            CcActivity::Unknown => "unknown",
        }
    }
}

/// Value of a **top-level** string field of one jsonl record.
///
/// Depth-aware, and that is the whole point.  Measured off live
/// records, the field order is not what a reader assumes:
///
/// ```text
/// user:      {"parentUuid":…,"promptId":…,"type":"user","message":{…}}
/// assistant: {"parentUuid":…,"message":{"model":…,"type":"message",…},…,"type":"assistant",…}
/// ```
///
/// Assistant records put `message` — which has its own
/// `"type":"message"` — **before** their own `type`, so "first
/// `"type"` in the line" reads every assistant record as `message`.
/// That shipped in 0.7.38 and made all 9 live panes report `working`:
/// assistant records were being skipped as bookkeeping and the scan
/// fell through to the user record behind them.  Scanning at depth 1
/// is the fix, and it retires the whole class of field-order bugs.
fn top_level_str<'a>(line: &'a str, field: &str) -> Option<&'a str> {
    let b = line.as_bytes();
    // Content of the JSON string starting at `at` (which must be the
    // opening quote), plus the index just past its closing quote.
    // Escapes are stepped over, not decoded — the values this reads
    // ("assistant", "user", …) have none.
    fn scan_string(b: &[u8], at: usize) -> Option<(&str, usize)> {
        let mut i = at + 1;
        let start = i;
        while i < b.len() {
            match b[i] {
                b'\\' => i += 2,
                b'"' => {
                    return std::str::from_utf8(&b[start..i]).ok().map(|s| (s, i + 1))
                }
                _ => i += 1,
            }
        }
        None
    }
    let mut i = 0usize;
    let mut depth: i32 = 0;
    while i < b.len() {
        match b[i] {
            b'{' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                depth -= 1;
                i += 1;
            }
            b'"' => {
                let (s, after) = scan_string(b, i)?;
                let mut j = after;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                // A key is a string followed by ':'.  Only depth 1 is
                // the record's own object.
                if depth == 1 && j < b.len() && b[j] == b':' {
                    let mut k = j + 1;
                    while k < b.len() && b[k].is_ascii_whitespace() {
                        k += 1;
                    }
                    if s == field {
                        return if k < b.len() && b[k] == b'"' {
                            scan_string(b, k).map(|(v, _)| v)
                        } else {
                            None // present but not a string
                        };
                    }
                    i = k; // resume at the value; its braces adjust depth
                } else {
                    i = after;
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// The record's own `type` — `assistant`, `user`, `system`,
/// `last-prompt`, `attachment`, `ai-title`, `mode`, `permission-mode`,
/// … (that list is what live transcripts actually contain).
fn record_type(line: &str) -> Option<&str> {
    top_level_str(line, "type")
}

/// Classify one conversation record.  Split from the file read so the
/// shape rules are testable against literal records.
///
/// `has_young_child` answers "does claude have a process younger than
/// this record" — the caller resolves it from the proc table it already
/// walked, since a tool that shells out shows up as a child of claude
/// while an approval prompt shows nothing.
fn activity_from_last_record(line: &str, has_young_child: bool) -> CcActivity {
    match record_type(line) {
        Some("assistant") => {
            if line.contains("\"type\":\"tool_use\"") {
                CcActivity::ToolPending { executing: has_young_child }
            } else {
                CcActivity::AwaitingUser
            }
        }
        // A user record is either a real prompt or the transcript's
        // record of a tool result; both leave the assistant owing the
        // next record.
        Some("user") => CcActivity::Working,
        _ => CcActivity::Unknown,
    }
}

/// True for records that say nothing about whose turn it is and must be
/// skipped when scanning back for the conversation's last state.
///
/// This is not a hypothetical: claudecode writes
/// `{"type":"system","subtype":"turn_duration",…}` **after** the
/// assistant's closing message, so the single most common resting state
/// — a finished turn waiting on the user — sits behind one of these.
/// Classifying only the literal last line read `unknown` on 7 of 9 live
/// panes, which is what caught it.
fn is_bookkeeping_record(line: &str) -> bool {
    !matches!(record_type(line), Some("assistant") | Some("user"))
}

struct ScanResult {
    /// `shelld_session_id → badge string ("P<n> <uuid>")`.  Replaces
    /// `last_mapping` on the main side every time it arrives.
    new_mapping: HashMap<u64, String>,
    /// `shelld_session_id → what cc is doing there`.  Diffed against
    /// `last_activity` on the main side so only transitions are logged.
    new_activity: HashMap<u64, CcActivity>,
    /// `shelld_session_id → BindMeta`.  Drives `on_pane_badge_click`'s
    /// profile-cycle dispatch on the main side.
    new_meta: HashMap<u64, BindMeta>,
    /// Log lines the worker wanted to emit but can't (host is main-
    /// thread-only).  `tick` replays them through `host.log`.  Stays
    /// near-empty in steady state — only transitions add lines.
    log_lines: Vec<(LogLevel, &'static str, String)>,
}

/// Everything the worker owns.  No shared mutable state with the
/// plugin; the worker reads disk + procs and sends ScanResult back.
struct WorkerCtx {
    projects_root: PathBuf,
    shelld: Arc<ShelldClient>,
    /// Same role as the old `ClaudecodePlugin::seen` field, but the
    /// worker owns it now and the plugin never touches it.
    seen: HashMap<PathBuf, SessionInfo>,
    /// Per-jsonl: the claude pid last seen owning it, and the byte
    /// offset from which its model may be read.  See
    /// `model_cutoff_for`.
    model_cutoff: HashMap<PathBuf, (i32, u64)>,
}

/// How many of a project's session files stay in `seen`.
///
/// One was not enough: with a single candidate per project, two panes
/// in one project can never both be badged — the first takes it and the
/// second has nothing to fall back to even when its own session is live.
/// Four covers the realistic "a couple of panes in one repo" case with
/// headroom, and bounds the map at panes × 4 entries.
const SESSIONS_KEPT_PER_PROJECT: usize = 4;

impl WorkerCtx {
    /// Refresh `seen` for exactly the projects named in `wanted` —
    /// the ones that have a live pane.
    ///
    /// Scoping matters twice over.  It used to walk every directory
    /// under `~/.claude/projects` (~50 here, of which ~10 have panes),
    /// paying a `parse_session_id` + `tail_last_message_type` per newly
    /// changed file for projects nothing would ever ask about; and
    /// `seen` was insert-only, so a long-lived shell accumulated an
    /// entry per session file it had ever noticed.  Scoped + pruned,
    /// the map is bounded by `wanted.len() × SESSIONS_KEPT_PER_PROJECT`
    /// and the walk touches fewer directories than before despite
    /// keeping four files each instead of one.
    fn refresh_seen<'a>(
        &mut self,
        wanted: impl Iterator<Item = &'a str>,
        log_lines: &mut Vec<(LogLevel, &'static str, String)>,
    ) {
        let mut newly_seen = 0usize;
        let mut updates = 0usize;
        let mut alive: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();
        for project_dir in wanted {
            if !done.insert(project_dir.to_string()) {
                continue; // two panes, one project — walk it once
            }
            let project_path = self.projects_root.join(project_dir);
            let Ok(dir) = fs::read_dir(&project_path) else { continue };
            let mut cands: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
            for f in dir.flatten() {
                let p = f.path();
                if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }
                let Ok(meta) = f.metadata() else { continue };
                cands.push((
                    p,
                    meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    meta.len(),
                ));
            }
            cands.sort_by(|a, b| b.1.cmp(&a.1));
            cands.truncate(SESSIONS_KEPT_PER_PROJECT);
            for (jsonl_path, mtime, size) in cands {
                alive.insert(jsonl_path.clone());
                if let Some(prev) = self.seen.get(&jsonl_path) {
                    if prev.last_mtime == mtime && prev.last_size == size {
                        continue;
                    }
                }
                let Some(session_id) = parse_session_id(&jsonl_path) else { continue };
                let last_message_kind = tail_last_message_type(&jsonl_path);
                let is_new = !self.seen.contains_key(&jsonl_path);
                self.seen.insert(
                    jsonl_path.clone(),
                    SessionInfo {
                        session_id: session_id.clone(),
                        project_dir: project_dir.to_string(),
                        jsonl_path: jsonl_path.clone(),
                        last_mtime: mtime,
                        last_size: size,
                        last_message_kind: last_message_kind.clone(),
                    },
                );
                if is_new {
                    newly_seen += 1;
                    log_lines.push((
                        LogLevel::Info,
                        "session",
                        format!(
                            "session detected sid={} project={} kind={} size={}",
                            session_id,
                            project_dir,
                            last_message_kind.as_deref().unwrap_or("?"),
                            size
                        ),
                    ));
                } else {
                    updates += 1;
                    log_lines.push((
                        LogLevel::Debug,
                        "session.update",
                        format!(
                            "sid={} kind={} size={}",
                            session_id,
                            last_message_kind.as_deref().unwrap_or("?"),
                            size
                        ),
                    ));
                }
            }
        }
        // Bounded growth: anything that dropped out of a project's top
        // N (or whose project lost its last pane) leaves the map, and
        // the per-file model fence leaves with it.
        self.seen.retain(|p, _| alive.contains(p));
        self.model_cutoff.retain(|p, _| alive.contains(p));
        if newly_seen > 0 || updates > 0 {
            log_lines.push((
                LogLevel::Debug,
                "tick.summary",
                format!("new={} updated={}", newly_seen, updates),
            ));
        }
    }

    /// Byte offset in `path` from which an assistant record may be
    /// trusted to describe the model `claude_pid` is actually using.
    ///
    /// A profile switch kills claude and re-runs it as
    /// `claude<N> --resume <uuid>` — the **same** session, so the same
    /// jsonl simply keeps growing.  Nothing is written at resume that
    /// names the new model (checked: the only startup-ish records are
    /// `mode`, `last-prompt` and `system/turn_duration`, none of which
    /// carry one), so the newest assistant record in the file is still
    /// the one the *previous* profile served — with the previous
    /// profile's model.  Reading it back gives a badge that confidently
    /// states the wrong model until the user sends another message, and
    /// profiles really do differ here.
    ///
    /// A changed pid is the signal that the file's existing contents
    /// belong to a process that is gone.  Everything before that point
    /// is fenced off; until the new process answers a turn, the badge
    /// renders without `@model` — which is the honest state, and one
    /// the badge already knows how to draw.
    fn model_cutoff_for(&mut self, path: &std::path::Path, claude_pid: i32) -> u64 {
        match self.model_cutoff.get(path) {
            Some(&(pid, cutoff)) if pid == claude_pid => cutoff,
            _ => {
                // Either the first sighting or a replaced process.  On
                // first sighting the records are this process's own, so
                // nothing is fenced (cutoff 0); on replacement, fence
                // everything written so far.
                let cutoff = if self.model_cutoff.contains_key(path) {
                    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
                } else {
                    0
                };
                self.model_cutoff.insert(path.to_path_buf(), (claude_pid, cutoff));
                cutoff
            }
        }
    }
}

/// Worker thread entry.  Lives until the request channel is dropped
/// (which `stop()` triggers by clearing `scan_req_tx`).
fn worker_main(
    mut ctx: WorkerCtx,
    req_rx: Receiver<()>,
    res_tx: Sender<ScanResult>,
) {
    while req_rx.recv().is_ok() {
        let result = ctx.scan_once();
        if res_tx.send(result).is_err() {
            // Main side hung up; nothing left to do.
            break;
        }
    }
}

impl WorkerCtx {
    /// One full scan pass: walk `~/.claude/projects/*/*.jsonl`,
    /// update `self.seen`, list shelld sessions, BFS each for a
    /// `claude` descendant, and compute the new badge mapping.
    /// Slow (disk + sysctl) but runs off the L1 main loop so its
    /// runtime is invisible to the plugin host's 100ms tick budget.
    fn scan_once(&mut self) -> ScanResult {
        let mut log_lines: Vec<(LogLevel, &'static str, String)> = Vec::new();

        // -- per-session mapping: BFS each shelld session ------------
        let mut new_mapping: HashMap<u64, String> = HashMap::new();
        let mut new_meta: HashMap<u64, BindMeta> = HashMap::new();
        let mut new_activity: HashMap<u64, CcActivity> = HashMap::new();
        let sessions = match self.shelld.list_sessions() {
            Ok(v) => v,
            Err(e) => {
                log_lines.push((
                    LogLevel::Info,
                    "tick.shelld_list_failed",
                    format!("{e}"),
                ));
                return ScanResult { new_mapping, new_meta, new_activity, log_lines };
            }
        };
        let procs = pidtree::list_all_procs();
        // Pass 1 — the per-pane facts, gathered before anything is
        // bound.  Binding needs to be a decision over the whole set:
        // one session uuid belongs to exactly one pane, so a pane that
        // can *prove* its uuid (argv) has to be served before a pane
        // that is only guessing from mtimes.
        struct PaneFacts {
            shelld_sid: u64,
            claude_pid: i32,
            claude_start: SystemTime,
            cwd: PathBuf,
            encoded: String,
            argv_uuid: Option<String>,
        }
        let mut facts: Vec<PaneFacts> = Vec::new();
        for s in &sessions {
            if !s.alive {
                continue;
            }
            let descendants = pidtree::descendants_of(s.child_pid, &procs);
            let Some(claude) =
                descendants.iter().find(|d| looks_like_claudecode(d))
            else {
                continue;
            };
            let Some(cwd) = pidtree::proc_cwd(claude.pid) else {
                continue;
            };
            facts.push(PaneFacts {
                shelld_sid: s.session_id,
                claude_pid: claude.pid,
                claude_start: SystemTime::UNIX_EPOCH
                    + Duration::from_secs(claude.start_unix),
                encoded: encode_project_dir(&cwd),
                cwd,
                argv_uuid: argv_session_uuid(claude.pid, &descendants),
            });
        }
        // Stable order so an ambiguous project resolves the same way on
        // every tick — badges that swap panes every 2 s would be worse
        // than a badge that is merely a guess.
        facts.sort_by_key(|f| f.shelld_sid);

        // -- jsonl pass, scoped to the projects that have panes -------
        self.refresh_seen(
            facts.iter().map(|f| f.encoded.as_str()),
            &mut log_lines,
        );

        // Pass 2 — assign, proof first, guesses after, no uuid twice.
        let mut claimed: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut bound: Vec<(usize, String, Option<PathBuf>)> = Vec::new();
        for (i, f) in facts.iter().enumerate() {
            if let Some(uuid) = &f.argv_uuid {
                if claimed.insert(uuid.clone()) {
                    let path = self.session_by_uuid(uuid).map(|s| s.jsonl_path.clone());
                    bound.push((i, uuid.clone(), path));
                }
            }
        }
        for (i, f) in facts.iter().enumerate() {
            if bound.iter().any(|(j, _, _)| *j == i) {
                continue;
            }
            if let Some((uuid, path)) =
                self.session_for_project(&f.encoded, &claimed, f.claude_start)
            {
                claimed.insert(uuid.clone());
                bound.push((i, uuid, Some(path)));
            }
        }

        for (i, sid_uuid, jsonl_path) in bound {
            let f = &facts[i];
            let (tag, profile_num) = match profile_tag_for(f.claude_pid) {
                Some(t) => {
                    let n = t
                        .strip_prefix('P')
                        .and_then(|d| d.parse::<u8>().ok())
                        .unwrap_or(u8::MAX);
                    (Some(t), n)
                }
                None => (None, u8::MAX),
            };
            // Active model, tailed from the session jsonl: the
            // newest of (assistant record's authoritative
            // `"model"` field, `/model` local_command output) —
            // the latter makes an interactive switch show up on
            // the very next tick instead of after the next
            // assistant turn.  A session named by argv but not yet
            // scanned has no path — badge without the model half.
            let model = jsonl_path.as_ref().and_then(|p| {
                let cutoff = self.model_cutoff_for(p, f.claude_pid);
                tail_model_short(p, cutoff)
            });
            let badge = match (tag, model) {
                (Some(t), Some(m)) => format!("{}@{} {}", t, m, sid_uuid),
                (Some(t), None) => format!("{} {}", t, sid_uuid),
                (None, _) => sid_uuid.clone(),
            };
            let project_basename = f
                .cwd
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            // cc-layer status.  The generic layer already said a job
            // owns this tty (that is how we found claude at all); this
            // says what claude is doing inside it.  A tool that shells
            // out appears as a process under claude younger than the
            // record that asked for it, which is what separates "the
            // tool is running" from "claude is parked on the approval
            // prompt" — long-lived children (MCP servers, started with
            // the session) are older than the record and don't count.
            if let Some(path) = jsonl_path.as_ref() {
                let record_at = fs::metadata(path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let has_young_child = pidtree::descendants_of(f.claude_pid, &procs)
                    .iter()
                    .any(|d| d.start_unix >= record_at);
                new_activity.insert(
                    f.shelld_sid,
                    tail_activity(path, has_young_child),
                );
            }
            new_mapping.insert(f.shelld_sid, badge);
            new_meta.insert(
                f.shelld_sid,
                BindMeta {
                    profile_num,
                    uuid: sid_uuid,
                    claude_pid: f.claude_pid,
                    project_basename,
                },
            );
            // No log line here on purpose — `session.bound` is
            // transition-only.  Main side diffs `result.new_mapping`
            // against `self.last_mapping` and only logs the deltas;
            // otherwise we'd write 12 lines per 2 s tick in steady
            // state and drown the file.
        }

        ScanResult { new_mapping, new_meta, new_activity, log_lines }
    }

    /// Reverse-lookup: encoded project dir → newest known session
    /// (id + its jsonl path, so callers can tail per-session state
    /// like the active model).  Cheap scan over `seen`; a dozen
    /// projects active in practice.
    ///
    /// Two constraints make this a *guess with guardrails* rather than
    /// a free-for-all:
    ///
    /// * `claimed` — a uuid already bound to another pane is skipped.
    ///   Two panes cwd'd into one project used to receive the identical
    ///   badge, which is provably wrong for at least one of them
    ///   (2026-07-30: sessions 383 + 394 both in `qualcomm/insight`,
    ///   both badged `9e304c9a`, which argv shows belongs to 383).
    /// * `claude_start` — a session whose file has not been written
    ///   since this claude process started cannot be the session it is
    ///   writing.  Without this, a pane freshly `claude`d in a project
    ///   whose newest session is days old wears that dead session's
    ///   uuid.  No eligible candidate ⇒ no badge, which is the honest
    ///   answer until the pane's own session file appears.
    fn session_for_project(
        &self,
        encoded_dir: &str,
        claimed: &std::collections::HashSet<String>,
        claude_start: SystemTime,
    ) -> Option<(String, PathBuf)> {
        let mut newest: Option<(SystemTime, &SessionInfo)> = None;
        for s in self.seen.values() {
            if s.project_dir != encoded_dir {
                continue;
            }
            if claimed.contains(&s.session_id) {
                continue;
            }
            if s.last_mtime < claude_start {
                continue;
            }
            match newest {
                Some((t, _)) if t >= s.last_mtime => {}
                _ => newest = Some((s.last_mtime, s)),
            }
        }
        newest.map(|(_, s)| (s.session_id.clone(), s.jsonl_path.clone()))
    }

    /// Look a session up by uuid — the argv-authoritative path knows
    /// *which* session a pane owns but still needs its jsonl to tail
    /// the active model.  `None` for a session too new to have been
    /// scanned yet; the badge then carries no model until it is.
    fn session_by_uuid(&self, uuid: &str) -> Option<&SessionInfo> {
        self.seen.values().find(|s| s.session_id == uuid)
    }
}

/// The live session uuid as stated by the claude process tree's own
/// argv, if it states one.
///
/// This is the only *authoritative* binding available: everything else
/// is inference from file mtimes.  Two argv shapes carry it —
///
/// * `--session-id <uuid>` — the session this process writes.  Wins,
///   because a forked resume (`--resume old.jsonl --fork-session
///   --session-id new`) writes `new` while naming `old`.
/// * `--resume <uuid>` / `--resume <path>/<uuid>.jsonl` — a plain
///   resume continues writing the file it names.
///
/// Searched on the matched process and its own subtree: the daemon
/// shape (`claude daemon run` → pty host → version binary) puts the
/// flags several levels below the `claude` the pane sees.  A plain
/// interactive `claude` states nothing, which is why the inference path
/// still has to exist.
///
/// `pane_tree` is the pane's already-computed descendant list, so this
/// costs no new process-table walk.  Only claude's own subtree gets a
/// `proc_cmdline` (a sysctl each) — scanning the whole pane tree would
/// mean paying for rust-analyzer, node, and every build job as well.
fn argv_session_uuid(claude_pid: i32, pane_tree: &[pidtree::ProcRow]) -> Option<String> {
    let mut subtree: Vec<i32> = vec![claude_pid];
    // Transitive closure by repeated passes.  The claude subtree is a
    // handful of processes and at most a few levels deep, so this beats
    // building a map for it.
    loop {
        let before = subtree.len();
        for p in pane_tree {
            if subtree.contains(&p.ppid) && !subtree.contains(&p.pid) {
                subtree.push(p.pid);
            }
        }
        if subtree.len() == before {
            break;
        }
    }
    let mut resumed: Option<String> = None;
    for pid in subtree {
        let Some(line) = pidtree::proc_cmdline(pid) else { continue };
        if let Some(u) = flag_uuid(&line, "--session-id") {
            return Some(u);
        }
        if resumed.is_none() {
            resumed = flag_uuid(&line, "--resume");
        }
    }
    resumed
}

/// Value of `flag` in a space-joined argv, reduced to a bare uuid.
/// Accepts both `<uuid>` and `<path>/<uuid>.jsonl`; returns None when
/// the flag is absent or its value isn't uuid-shaped (`--resume` also
/// takes a session *name*).
fn flag_uuid(cmdline: &str, flag: &str) -> Option<String> {
    let mut it = cmdline.split(' ');
    let raw = loop {
        let tok = it.next()?;
        if tok == flag {
            break it.next()?;
        }
    };
    let base = raw.rsplit('/').next().unwrap_or(raw);
    let base = base.strip_suffix(".jsonl").unwrap_or(base);
    is_uuid(base).then(|| base.to_string())
}

/// 8-4-4-4-12 lowercase hex with dashes.  Deliberately strict: a
/// non-uuid `--resume` value must fall through to the inference path,
/// not become a bogus badge.
fn is_uuid(s: &str) -> bool {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let mut parts = s.split('-');
    for want in GROUPS {
        let Some(p) = parts.next() else { return false };
        if p.len() != want || !p.bytes().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
    }
    parts.next().is_none()
}

/// Read the first line of `path` and try to parse `"sessionId":"<uuid>"`
/// out of it.  Cheap regex-free string scan — JSON parser would be
/// overkill for one well-known field, and pulling serde_json into the
/// shell binary just for this would violate the self-build principle.
fn parse_session_id(path: &PathBuf) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let f = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(f);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    // claudecode jsonl always starts the file with a "mode" record
    // containing `"sessionId":"<uuid>"`.  We look for that key.
    let key = "\"sessionId\":\"";
    let i = line.find(key)? + key.len();
    let rest = &line[i..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpfile(content: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "claudecode-plugin-test-{}-{}.jsonl",
            std::process::id(),
            n
        ));
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    fn tmphome(entries: &[(&str, bool)]) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let home = std::env::temp_dir().join(format!(
            "claudecode-plugin-home-{}-{}",
            std::process::id(),
            n
        ));
        fs::create_dir_all(&home).unwrap();
        for (name, is_dir) in entries {
            let p = home.join(name);
            if *is_dir {
                fs::create_dir(&p).unwrap();
            } else {
                fs::File::create(&p).unwrap();
            }
        }
        home
    }

    #[test]
    fn discover_profiles_finds_sorted_dirs() {
        let home = tmphome(&[
            (".claude-profile-4", true),
            (".claude-profile-1", true),
            (".claude-profile-2", true),
            (".claude", true),
            (".claude-profile-x", true),   // non-numeric suffix
            (".claude-profile-3", false),  // a file, not a dir
            ("Documents", true),
        ]);
        assert_eq!(discover_profiles_in(&home), vec![1, 2, 4]);
    }

    #[test]
    fn discover_profiles_empty_when_none_exist() {
        let home = tmphome(&[(".claude", true)]);
        assert!(discover_profiles_in(&home).is_empty());
    }

    #[test]
    fn short_model_normalises_ids_and_display_names() {
        assert_eq!(short_model("claude-fable-5"), "fable-5");
        assert_eq!(short_model("claude-opus-4-8"), "opus-4-8");
        assert_eq!(short_model("claude-sonnet-5"), "sonnet-5");
        assert_eq!(short_model("claude-haiku-4-5-20251001"), "haiku-4-5");
        assert_eq!(short_model("Fable 5"), "fable-5");
        assert_eq!(short_model("Opus 4.8"), "opus-4-8");
        assert_eq!(short_model("Default (recommended)"), "default");
        // ANSI-bold display name straight out of /model's stdout.
        assert_eq!(short_model("\u{1b}[1mFable 5\u{1b}[22m"), "fable-5");
    }

    /// The profile-switch case.
    ///
    /// Switching profiles re-runs `claude<N> --resume <uuid>`, so the
    /// same jsonl keeps growing and its newest assistant record still
    /// names the *previous* profile's model.  Reading it back made the
    /// badge state the wrong model until the next turn.  Fencing the
    /// read at the switch offset makes it report nothing instead —
    /// the badge then omits `@model`, which is honest.
    #[test]
    fn model_read_is_fenced_at_a_profile_switch() {
        let dir = std::env::temp_dir().join("cc-model-fence-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");

        // Pre-switch history: P1 was answering with fable-5.
        let pre = "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"model\":\"claude-fable-5\"}}\n";
        fs::write(&path, pre).unwrap();
        let switch_at = fs::metadata(&path).unwrap().len();

        // Unfenced, the old model is what you get — this is the bug.
        assert_eq!(tail_model_short(&path, 0).as_deref(), Some("fable-5"));

        // Fenced at the switch: nothing to report yet.
        assert_eq!(
            tail_model_short(&path, switch_at),
            None,
            "a fenced read must not surface the previous profile's model"
        );

        // The new profile answers a turn; now it reports again.
        let post = "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"model\":\"claude-opus-4-8\"}}\n";
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        use std::io::Write;
        f.write_all(post.as_bytes()).unwrap();
        drop(f);
        assert_eq!(
            tail_model_short(&path, switch_at).as_deref(),
            Some("opus-4-8"),
            "post-switch records must still be read"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// The fence moves only when the owning process is replaced.
    /// argv is the only authoritative statement of which session a
    /// pane owns; these are the shapes claude actually emits (captured
    /// from a live 18-pane session on 2026-07-30).
    #[test]
    fn argv_uuid_reads_the_shapes_claude_emits() {
        // Plain resume: continues writing the file it names.
        assert_eq!(
            flag_uuid(
                "claude --resume 9e304c9a-93d3-428d-a6a6-fc537db59dc7",
                "--resume"
            )
            .as_deref(),
            Some("9e304c9a-93d3-428d-a6a6-fc537db59dc7")
        );
        // Forked resume: `--resume` names the *source* file by path,
        // `--session-id` names the session actually being written.
        let forked = "/x/ClaudeCode.app/Contents/MacOS/claude --bg-pty-host /tmp/s.sock 72 56 \
             -- /x/versions/2.1.220 --session-id 2c78740f-4a41-4e6e-8b99-5e0614326d1f \
             --fork-session --resume /Users/d/.claude-profile-1/projects/-Users-d-p/\
             429c7c04-81cc-43bf-8cbc-814dded894d0.jsonl --reply-on-resume";
        assert_eq!(
            flag_uuid(forked, "--session-id").as_deref(),
            Some("2c78740f-4a41-4e6e-8b99-5e0614326d1f")
        );
        assert_eq!(
            flag_uuid(forked, "--resume").as_deref(),
            Some("429c7c04-81cc-43bf-8cbc-814dded894d0"),
            "path form reduces to the bare uuid"
        );
        // A fresh interactive claude states nothing — that is what
        // keeps the inference path load-bearing.
        assert_eq!(flag_uuid("claude", "--resume"), None);
        assert_eq!(flag_uuid("claude --resume", "--resume"), None);
        // `--resume` also takes a session *name*; a non-uuid value must
        // not become a badge.
        assert_eq!(flag_uuid("claude --resume my-branch-work", "--resume"), None);
        assert!(!is_uuid("9e304c9a93d3428da6a6fc537db59dc7"), "dashes required");
        assert!(!is_uuid("9e304c9a-93d3-428d-a6a6-fc537db59dc7-extra"));
        assert!(!is_uuid("9e304c9z-93d3-428d-a6a6-fc537db59dc7"), "hex only");
    }

    /// Build a `WorkerCtx` whose `seen` holds one session per project.
    fn ctx_with_sessions(sessions: &[(&str, &str, SystemTime)]) -> WorkerCtx {
        let mut seen = HashMap::new();
        for (uuid, project, mtime) in sessions {
            let path = PathBuf::from(format!("/fake/{project}/{uuid}.jsonl"));
            seen.insert(
                path.clone(),
                SessionInfo {
                    session_id: (*uuid).to_string(),
                    project_dir: (*project).to_string(),
                    jsonl_path: path,
                    last_mtime: *mtime,
                    last_size: 1,
                    last_message_kind: None,
                },
            );
        }
        WorkerCtx {
            projects_root: PathBuf::from("/fake"),
            shelld: Arc::new(ShelldClient::new(None)),
            seen,
            model_cutoff: HashMap::new(),
        }
    }

    /// The 2026-07-30 report: two panes cwd'd into one project both
    /// wore badge `9e304c9a`.  One session belongs to one pane — the
    /// second pane must come away with nothing, not a copy.
    #[test]
    fn a_session_already_bound_is_not_handed_to_a_second_pane() {
        let now = SystemTime::now();
        let ctx = ctx_with_sessions(&[("9e304c9a", "-Users-d-insight", now)]);
        let started = now - Duration::from_secs(600);
        let mut claimed = std::collections::HashSet::new();

        let first = ctx.session_for_project("-Users-d-insight", &claimed, started);
        assert_eq!(first.as_ref().map(|(u, _)| u.as_str()), Some("9e304c9a"));
        claimed.insert("9e304c9a".to_string());

        assert!(
            ctx.session_for_project("-Users-d-insight", &claimed, started).is_none(),
            "the only session is taken; a second pane gets no badge"
        );
    }

    /// A pane freshly `claude`d in a project whose newest session is
    /// days old must not wear that dead session's uuid: the file has
    /// not been written since this process started, so it cannot be
    /// what this process is writing.
    #[test]
    fn a_session_older_than_the_process_is_not_bound() {
        let now = SystemTime::now();
        let stale = now - Duration::from_secs(2 * 24 * 3600);
        let ctx = ctx_with_sessions(&[("df58a571", "-Users-d-insight", stale)]);
        let claimed = std::collections::HashSet::new();

        let started_after = now - Duration::from_secs(300);
        assert!(
            ctx.session_for_project("-Users-d-insight", &claimed, started_after).is_none(),
            "nothing written since launch ⇒ no session of ours yet"
        );

        // Same session, a claude that predates it: now it is plausibly
        // ours and binds as before.
        let started_before = stale - Duration::from_secs(60);
        assert_eq!(
            ctx.session_for_project("-Users-d-insight", &claimed, started_before)
                .map(|(u, _)| u),
            Some("df58a571".to_string())
        );
    }

    /// `seen` is bounded by the projects that currently have panes ×
    /// `SESSIONS_KEPT_PER_PROJECT`, and it *shrinks* — it used to be
    /// insert-only, which meant a shell running for weeks accumulated an
    /// entry per session file it had ever noticed.
    #[test]
    fn seen_keeps_the_newest_few_per_live_project_and_prunes_the_rest() {
        let root = std::env::temp_dir().join(format!(
            "cc-refresh-seen-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mk = |project: &str, n: usize| {
            let dir = root.join(project);
            fs::create_dir_all(&dir).unwrap();
            for i in 0..n {
                let uuid = format!("{:08x}-1111-2222-3333-444444444444", i);
                let p = dir.join(format!("{uuid}.jsonl"));
                fs::write(&p, format!("{{\"sessionId\":\"{uuid}\"}}\n")).unwrap();
                // Stagger mtimes so "newest N" is well defined; index 0
                // is oldest.
                let t = filetime_plus(i as u64);
                set_mtime(&p, t);
            }
        };
        mk("-p-alpha", 6);
        mk("-p-beta", 2);

        let mut ctx = WorkerCtx {
            projects_root: root.clone(),
            shelld: Arc::new(ShelldClient::new(None)),
            seen: HashMap::new(),
            model_cutoff: HashMap::new(),
        };
        let mut logs = Vec::new();
        // Two panes in alpha: the project is walked once, not twice.
        ctx.refresh_seen(["-p-alpha", "-p-beta", "-p-alpha"].into_iter(), &mut logs);

        let alpha: Vec<_> = ctx
            .seen
            .values()
            .filter(|s| s.project_dir == "-p-alpha")
            .map(|s| s.session_id.clone())
            .collect();
        assert_eq!(
            alpha.len(),
            SESSIONS_KEPT_PER_PROJECT,
            "kept the newest few, not all six: {alpha:?}"
        );
        assert!(
            !alpha.iter().any(|id| id.starts_with("00000000")
                || id.starts_with("00000001")),
            "the two oldest must be the ones dropped: {alpha:?}"
        );
        assert_eq!(
            ctx.seen.values().filter(|s| s.project_dir == "-p-beta").count(),
            2,
            "a project with fewer files keeps all of them"
        );

        // Beta's pane goes away: its entries must leave the map.
        let mut logs2 = Vec::new();
        ctx.refresh_seen(["-p-alpha"].into_iter(), &mut logs2);
        assert_eq!(ctx.seen.len(), SESSIONS_KEPT_PER_PROJECT);
        assert!(ctx.seen.values().all(|s| s.project_dir == "-p-alpha"));

        let _ = fs::remove_dir_all(&root);
    }

    fn filetime_plus(secs: u64) -> SystemTime {
        // A fixed base well in the past keeps the ordering deterministic
        // regardless of when the test runs.
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs)
    }

    fn set_mtime(path: &PathBuf, t: SystemTime) {
        let secs = t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        let times = [
            libc::timeval { tv_sec: secs as i64, tv_usec: 0 },
            libc::timeval { tv_sec: secs as i64, tv_usec: 0 },
        ];
        let c = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let r = unsafe { libc::utimes(c.as_ptr(), times.as_ptr()) };
        assert_eq!(r, 0, "utimes failed for {}", path.display());
    }

    #[test]
    fn model_cutoff_tracks_the_owning_pid() {
        let dir = std::env::temp_dir().join("cc-model-cutoff-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        fs::write(&path, "x".repeat(100)).unwrap();

        let mut ctx = WorkerCtx {
            projects_root: dir.clone(),
            shelld: Arc::new(ShelldClient::new(None)),
            seen: HashMap::new(),
            model_cutoff: HashMap::new(),
        };

        // First sighting: nothing is fenced — those records belong to
        // the process we are looking at.
        assert_eq!(ctx.model_cutoff_for(&path, 111), 0);
        // Same process, file grew: still unfenced.
        fs::write(&path, "x".repeat(200)).unwrap();
        assert_eq!(ctx.model_cutoff_for(&path, 111), 0);
        // Process replaced: everything written so far is fenced off.
        assert_eq!(ctx.model_cutoff_for(&path, 222), 200);
        // And it stays put while that process lives.
        fs::write(&path, "x".repeat(500)).unwrap();
        assert_eq!(ctx.model_cutoff_for(&path, 222), 200);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_model_prefers_newest_record_in_file() {
        // assistant(fable) then a later /model switch (the modern
        // "type":"user" record shape, JSON-escaped ANSI, trailing
        // "and saved..." remark) -> the switch wins.
        let path = tmpfile(concat!(
            r#"{"type":"message","role":"assistant","model":"claude-fable-5","content":[]}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":"<local-command-stdout>Set model to \u001b[1mOpus 4.8\u001b[22m and saved as your default for new sessions</local-command-stdout>"}}"#,
            "\n",
        ));
        assert_eq!(tail_model_short(&path, 0).as_deref(), Some("opus-4-8"));

        // ...and vice versa: an assistant turn after the switch wins.
        // (older "system"/"local_command" record shape)
        let path = tmpfile(concat!(
            r#"{"type":"system","subtype":"local_command","content":"<local-command-stdout>Kept model as \u001b[1mOpus 4.8\u001b[22m</local-command-stdout>"}"#,
            "\n",
            r#"{"type":"message","role":"assistant","model":"claude-fable-5","content":[]}"#,
            "\n",
        ));
        assert_eq!(tail_model_short(&path, 0).as_deref(), Some("fable-5"));

        // No model anywhere -> None.
        let path = tmpfile(r#"{"type":"user","text":"hi"}"#);
        assert_eq!(tail_model_short(&path, 0), None);

        // Self-reference guard: a conversation that DISCUSSES the
        // marker (e.g. this feature being developed in a marspot
        // session) stores it inside a text field with the
        // surrounding quotes JSON-escaped — must NOT be picked up.
        let path = tmpfile(concat!(
            r#"{"type":"message","role":"assistant","model":"claude-fable-5","content":[]}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"marker 改锚 <local-command-stdout>Set model to 之类的-块),而 我 5"}]}}"#,
            "\n",
        ));
        assert_eq!(tail_model_short(&path, 0).as_deref(), Some("fable-5"));
    }

    #[test]
    fn badge_menu_excludes_current_profile() {
        let items = badge_menu_for(4, &[1, 2, 3, 4]);
        assert_eq!(
            items
                .iter()
                .map(|i| (i.tag, i.label.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "switch to P1"), (2, "switch to P2"), (3, "switch to P3")],
        );
    }

    #[test]
    fn badge_menu_from_p0_offers_every_profile() {
        // P0 (default .claude) isn't in the discovered set, so every
        // numbered profile is a target.
        let items = badge_menu_for(0, &[1, 2]);
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn badge_menu_single_profile_current_is_empty() {
        assert!(badge_menu_for(1, &[1]).is_empty());
        assert!(badge_menu_for(1, &[]).is_empty());
    }

    #[test]
    fn discover_profiles_missing_home_is_empty() {
        let home = std::env::temp_dir().join("claudecode-plugin-home-nonexistent");
        assert!(discover_profiles_in(&home).is_empty());
    }

    #[test]
    fn parse_session_id_finds_uuid_in_first_record() {
        let path = tmpfile(
            r#"{"type":"mode","mode":"normal","sessionId":"3ad170c8-7d00-4764-9bb8-fc31eba2154d"}
{"type":"user","text":"hello"}
"#,
        );
        let id = parse_session_id(&path).unwrap();
        assert_eq!(id, "3ad170c8-7d00-4764-9bb8-fc31eba2154d");
    }

    #[test]
    fn parse_session_id_returns_none_when_missing() {
        let path = tmpfile(r#"{"type":"user","text":"hello"}"#);
        assert!(parse_session_id(&path).is_none());
    }

    // ── cc activity classification ────────────────────────────────
    // The literals below are trimmed copies of real records from a
    // live transcript (`~/.claude/projects/…/<uuid>.jsonl`), not
    // invented shapes — the whole classification rests on where
    // `"type"` appears, so a guessed shape would test nothing.

    const REC_TOOL_USE: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}]}}"#;
    const REC_TEXT: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}"#;
    const REC_THINKING: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"…"}]}}"#;
    const REC_TOOL_RESULT: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]}}"#;
    const REC_USER_TEXT: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"go on"}]}}"#;
    const REC_SYSTEM: &str = r#"{"type":"system","subtype":"local_command","content":"<local-command-stdout>"}"#;

    /// An assistant turn that ended with a message is the one state
    /// where nothing is in flight.
    #[test]
    fn activity_assistant_message_is_awaiting_user() {
        assert_eq!(
            activity_from_last_record(REC_TEXT, false),
            CcActivity::AwaitingUser
        );
        assert_eq!(
            activity_from_last_record(REC_THINKING, false),
            CcActivity::AwaitingUser
        );
    }

    /// A tool call with nothing after it: running when claude has a
    /// child younger than the record, parked on the approval prompt
    /// when it has none.  Both are "in flight" — the split is for the
    /// log line, and neither may be read as idle.
    #[test]
    fn activity_tool_use_splits_on_whether_a_child_is_running() {
        assert_eq!(
            activity_from_last_record(REC_TOOL_USE, true),
            CcActivity::ToolPending { executing: true }
        );
        assert_eq!(
            activity_from_last_record(REC_TOOL_USE, false),
            CcActivity::ToolPending { executing: false }
        );
        assert_ne!(
            activity_from_last_record(REC_TOOL_USE, false),
            CcActivity::AwaitingUser,
            "an unanswered tool call is never idle — this is the case \
             that makes killing claude lose a pending call"
        );
    }

    /// Both user-record flavours (a real prompt, and the transcript's
    /// record of a tool result) leave the assistant owing the next
    /// record.
    #[test]
    fn activity_user_records_mean_the_assistant_owes_a_turn() {
        assert_eq!(
            activity_from_last_record(REC_USER_TEXT, false),
            CcActivity::Working
        );
        assert_eq!(
            activity_from_last_record(REC_TOOL_RESULT, false),
            CcActivity::Working
        );
    }

    /// Anything not modelled reads as Unknown, which callers treat as
    /// "in flight" — never as idle.
    #[test]
    fn activity_unmodelled_shapes_are_unknown_not_idle() {
        for line in [REC_SYSTEM, "", "not json at all", r#"{"no_type":1}"#] {
            let got = activity_from_last_record(line, false);
            assert_eq!(got, CcActivity::Unknown, "line: {line}");
        }
    }

    /// `describe` is what lands in the log, so pin the strings — they
    /// are what a future reader greps for.
    #[test]
    fn activity_describe_strings_are_stable() {
        assert_eq!(CcActivity::AwaitingUser.describe(), "awaiting_user");
        assert_eq!(
            CcActivity::ToolPending { executing: true }.describe(),
            "tool_executing"
        );
        assert_eq!(
            CcActivity::ToolPending { executing: false }.describe(),
            "tool_awaiting_approval"
        );
        assert_eq!(CcActivity::Working.describe(), "working");
        assert_eq!(CcActivity::Unknown.describe(), "unknown");
    }

    /// End-to-end through the file read, including the "last line
    /// wins" rule.
    #[test]
    fn tail_activity_classifies_the_final_record_of_a_file() {
        let path = tmpfile(&format!(
            "{REC_USER_TEXT}\n{REC_TOOL_USE}\n{REC_TOOL_RESULT}\n{REC_TEXT}\n"
        ));
        assert_eq!(tail_activity(&path, false), CcActivity::AwaitingUser);
        let path = tmpfile(&format!("{REC_TEXT}\n{REC_TOOL_USE}\n"));
        assert_eq!(
            tail_activity(&path, false),
            CcActivity::ToolPending { executing: false }
        );
        let missing = PathBuf::from("/nonexistent/marspot-cc-activity.jsonl");
        assert_eq!(tail_activity(&missing, false), CcActivity::Unknown);
    }

    // ── real transcript lines ─────────────────────────────────────
    // Verbatim prefixes of live records (`~/.claude/projects/…`),
    // truncated only in the payload.  The records above were written
    // with `"type"` first, which is NOT the on-disk field order, and
    // the classifier reading only the literal last line was wrong on 7
    // of 9 live panes — both facts are only visible against the real
    // thing.

    const LIVE_TOOL_RESULT: &str = r#"{"parentUuid":"22efebbb-9e50-42b8-a263-0870f70dde24","isSidechain":false,"promptId":"df17b9a2-3099-4e1c-a7ca-2405556f6a85","type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01SRfj2obe7VZK5ykPYFbcGz","type":"tool_result","content":"…"}]}}"#;
    const LIVE_TURN_DURATION: &str = r#"{"parentUuid":"d387726e-c410-4213-84b6-c73a3e604ddd","isSidechain":false,"type":"system","subtype":"turn_duration","durationMs":29074,"messageCount":1433,"timestamp":"2026-07-30T18:31:52.149Z","isMeta":false}"#;
    /// NOTE the field order: `message` (carrying its own
    /// `"type":"message"`) comes BEFORE the record's own `"type"`.
    /// That is how claudecode actually writes assistant records, and
    /// reading the first `"type"` in the line therefore yields
    /// `message` — the bug 0.7.38 shipped.
    const LIVE_ASSISTANT_TEXT: &str = r#"{"parentUuid":"4809f00f-b471-4330-a9e3-870374a08f67","isSidechain":false,"message":{"model":"claude-fable-5","id":"msg_011CdTzjoRr7Nb2u3TumMno6","type":"message","role":"assistant","content":[{"type":"text","text":"这条唤醒滞后"}]},"type":"assistant","uuid":"a261e9a7","timestamp":"2026-07-28T06:39:12.718Z"}"#;
    const LIVE_ASSISTANT_TOOL_USE: &str = r#"{"parentUuid":"4809f00f","isSidechain":false,"message":{"model":"claude-fable-5","id":"msg_02","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_9","name":"Bash","input":{"command":"ls"}}]},"type":"assistant","uuid":"b1"}"#;
    const LIVE_LAST_PROMPT: &str = r#"{"type":"last-prompt","lastPrompt":"文档、网站都要更新","leafUuid":"a261e9a7","sessionId":"f59436fa"}"#;

    /// The record's own type is the TOP-LEVEL one — not the first
    /// `"type"` in the line, which for assistant records belongs to
    /// the nested `message` object.
    #[test]
    fn record_type_reads_the_records_own_type_from_a_live_line() {
        assert_eq!(record_type(LIVE_TOOL_RESULT), Some("user"));
        assert_eq!(record_type(LIVE_TURN_DURATION), Some("system"));
        assert_eq!(record_type(LIVE_LAST_PROMPT), Some("last-prompt"));
        assert_eq!(
            record_type(LIVE_ASSISTANT_TEXT),
            Some("assistant"),
            "nested message.type must not win — this is the 0.7.38 bug"
        );
        assert_eq!(record_type(LIVE_ASSISTANT_TOOL_USE), Some("assistant"));
    }

    /// Depth is what makes it right, so pin the pieces directly.
    #[test]
    fn top_level_str_ignores_nested_keys_and_handles_escapes() {
        assert_eq!(
            top_level_str(r#"{"a":{"type":"inner"},"type":"outer"}"#, "type"),
            Some("outer")
        );
        assert_eq!(
            top_level_str(r#"{"list":[{"type":"x"}],"type":"outer"}"#, "type"),
            Some("outer")
        );
        // A quoted brace / escaped quote inside a value must not move
        // the depth counter or end the string early.
        assert_eq!(
            top_level_str(r#"{"text":"a \" } { b","type":"outer"}"#, "type"),
            Some("outer")
        );
        // Present but not a string, and simply absent.
        assert_eq!(top_level_str(r#"{"type":7}"#, "type"), None);
        assert_eq!(top_level_str(r#"{"other":"x"}"#, "type"), None);
    }

    /// A finished assistant turn, written the way claudecode writes
    /// it, is `awaiting_user` — the state 0.7.38 could never produce.
    #[test]
    fn live_assistant_records_classify_without_being_mistaken_for_bookkeeping() {
        assert!(!is_bookkeeping_record(LIVE_ASSISTANT_TEXT));
        assert_eq!(
            activity_from_last_record(LIVE_ASSISTANT_TEXT, false),
            CcActivity::AwaitingUser
        );
        assert_eq!(
            activity_from_last_record(LIVE_ASSISTANT_TOOL_USE, false),
            CcActivity::ToolPending { executing: false }
        );
        assert!(is_bookkeeping_record(LIVE_LAST_PROMPT));
        assert!(is_bookkeeping_record(LIVE_TURN_DURATION));
    }

    /// Transcript text that merely QUOTES the marker (a session where
    /// marspot itself is being developed does exactly this) is escaped
    /// in JSON, so the unescaped pattern can't match it.
    #[test]
    fn a_quoted_tool_use_marker_in_message_text_is_not_a_tool_call() {
        let line = r#"{"parentUuid":"x","message":{"type":"message","role":"assistant","content":[{"type":"text","text":"we match on \"type\":\"tool_use\" here"}]},"type":"assistant"}"#;
        assert_eq!(
            activity_from_last_record(line, false),
            CcActivity::AwaitingUser
        );
    }

    /// The regression this whole helper exists for: claudecode appends
    /// `system/turn_duration` after the assistant's closing message, so
    /// the resting state hides one line up.
    #[test]
    fn tail_activity_skips_the_turn_duration_record_after_a_finished_turn() {
        let path = tmpfile(&format!(
            "{LIVE_TOOL_RESULT}\n{LIVE_ASSISTANT_TEXT}\n{LIVE_TURN_DURATION}\n"
        ));
        assert_eq!(
            tail_activity(&path, false),
            CcActivity::AwaitingUser,
            "a finished turn must not read as unknown just because a \
             bookkeeping record trails it"
        );
    }

    /// Skipping bookkeeping must not skip past a real record: a tool
    /// call still pending is still pending.
    #[test]
    fn tail_activity_stops_at_the_first_conversation_record() {
        let path = tmpfile(&format!(
            "{LIVE_ASSISTANT_TEXT}\n{REC_TOOL_USE}\n{LIVE_TURN_DURATION}\n"
        ));
        assert_eq!(
            tail_activity(&path, true),
            CcActivity::ToolPending { executing: true }
        );
    }

    /// Nothing but bookkeeping in the window = no information.
    #[test]
    fn tail_activity_is_unknown_when_only_bookkeeping_is_visible() {
        let path = tmpfile(&format!(
            "{LIVE_TURN_DURATION}\n{LIVE_TURN_DURATION}\n"
        ));
        assert_eq!(tail_activity(&path, false), CcActivity::Unknown);
    }

    #[test]
    fn tail_last_message_type_reads_final_record() {
        let path = tmpfile(concat!(
            r#"{"type":"mode","sessionId":"abc"}"#,
            "\n",
            r#"{"type":"user","text":"hi"}"#,
            "\n",
            r#"{"type":"assistant","text":"there"}"#,
            "\n",
        ));
        let kind = tail_last_message_type(&path).unwrap();
        assert_eq!(kind, "assistant");
    }

    #[test]
    fn tail_last_message_type_handles_trailing_whitespace() {
        let path = tmpfile(concat!(
            r#"{"type":"user","text":"hi"}"#,
            "\n   \n",
        ));
        let kind = tail_last_message_type(&path).unwrap();
        assert_eq!(kind, "user");
    }

    #[test]
    fn strip_ansi_drops_csi_and_osc() {
        // Bold colour around the dot + a plain message.
        let raw = b"\x1b[1;33m\xe2\x8f\xba\x1b[0m API Error";
        let stripped = strip_ansi(raw);
        assert_eq!(&stripped, b"\xe2\x8f\xba API Error");
    }

    #[test]
    fn retryable_kind_matches_rate_limit_marker_wrapped() {
        // Real claude output — error wraps after "Rate" onto the
        // next line at a 2-space indent.  Our scanner has to
        // whitespace-normalise to catch it.
        let buf = b"\xe2\x8f\xba API Error: Server is temporarily limiting requests (not your usage limit) \xc2\xb7 Rate\n  limited\n";
        assert_eq!(retryable_error_kind(buf), Some("rate_limited"));
    }

    #[test]
    fn retryable_kind_matches_rate_limit_marker_inline() {
        let buf = b"API Error: ... Rate limited\n";
        assert_eq!(retryable_error_kind(buf), Some("rate_limited"));
    }

    #[test]
    fn retryable_kind_matches_overloaded() {
        let line = b"API Error: overloaded_error";
        assert_eq!(retryable_error_kind(line), Some("overloaded"));
    }

    #[test]
    fn retryable_kind_matches_network() {
        let line = b"API Error: Network error contacting Anthropic";
        assert_eq!(retryable_error_kind(line), Some("network"));
    }

    #[test]
    fn retryable_kind_ignores_unrelated_lines() {
        let line = b"  ok, processing your request ...";
        assert_eq!(retryable_error_kind(line), None);
    }

    #[test]
    fn retryable_kind_survives_ansi_around_marker() {
        let line = b"\x1b[31mAPI Error: Rate limited\x1b[0m\r";
        assert_eq!(retryable_error_kind(line), Some("rate_limited"));
    }
}

/// Cheap "what's the type of the last record" check.  Reads at most
/// the last 32 KiB of the file and looks at the last `\n`-separated
/// record's `"type":"…"` field.  Returns None if the file is malformed
/// or has no recognisable type.
/// Read a session's last record and classify it.  Same 32 KB tail
/// window as `tail_last_message_type`; the record we need is the last
/// line, and a single record over 32 KB (a huge tool result) reads as
/// `Unknown`, which the caller must already treat as "in flight".
fn tail_activity(path: &PathBuf, has_young_child: bool) -> CcActivity {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 32 * 1024;
    let Ok(mut f) = fs::File::open(path) else {
        return CcActivity::Unknown;
    };
    let Ok(md) = f.metadata() else {
        return CcActivity::Unknown;
    };
    let len = md.len();
    let start = len.saturating_sub(TAIL);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return CcActivity::Unknown;
    }
    let mut buf = Vec::with_capacity(TAIL as usize);
    if f.read_to_end(&mut buf).is_err() {
        return CcActivity::Unknown;
    }
    let s = String::from_utf8_lossy(&buf);
    // Walk back to the last record that is part of the conversation,
    // stepping over the bookkeeping ones claudecode appends after a
    // turn.  Bounded so a long run of them can't turn one tick into a
    // scan of the whole window.
    const MAX_SKIP: usize = 64;
    match s
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(MAX_SKIP)
        .find(|l| !is_bookkeeping_record(l))
    {
        Some(last) => activity_from_last_record(last, has_young_child),
        None => CcActivity::Unknown,
    }
}

fn tail_last_message_type(path: &PathBuf) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 32 * 1024;
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = if len > TAIL { len - TAIL } else { 0 };
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity(TAIL as usize);
    f.read_to_end(&mut buf).ok()?;
    // Find the last newline-anchored record.
    let s = String::from_utf8_lossy(&buf);
    let last_line = s.lines().rev().find(|l| !l.trim().is_empty())?;
    let key = "\"type\":\"";
    let i = last_line.find(key)? + key.len();
    let rest = &last_line[i..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

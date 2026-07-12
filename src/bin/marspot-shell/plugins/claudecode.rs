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
use std::time::SystemTime;
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
                // Settle window so the user doesn't see the zsh
                // prompt + spawn echo before claude paints its first
                // frame.  2 s covers a healthy machine; the watchdog
                // catches a hung claude.
                if elapsed >= std::time::Duration::from_secs(2) {
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
fn tail_model_short(path: &std::path::Path) -> Option<String> {
    use std::cell::RefCell;
    // (mtime, size)-keyed memo so the 2 s scan tick only re-reads a
    // session's tail when the jsonl actually grew — idle panes cost
    // one `stat` per tick, not a 256 KB read.  Worker-thread-local;
    // capped so dead sessions can't accumulate entries forever.
    thread_local! {
        static CACHE: RefCell<
            HashMap<PathBuf, (SystemTime, u64, Option<String>)>,
        > = RefCell::new(HashMap::new());
    }
    const CACHE_CAP: usize = 64;
    let md = fs::metadata(path).ok()?;
    let mtime = md.modified().ok()?;
    let size = md.len();
    let hit = CACHE.with(|c| {
        c.borrow().get(path).and_then(|(t, s, v)| {
            (*t == mtime && *s == size).then(|| v.clone())
        })
    });
    if let Some(v) = hit {
        return v;
    }
    let result = tail_model_short_uncached(path);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= CACHE_CAP {
            c.clear();
        }
        c.insert(path.to_path_buf(), (mtime, size, result.clone()));
    });
    result
}

fn tail_model_short_uncached(path: &std::path::Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(MODEL_TAIL_BYTES);
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
struct ScanResult {
    /// `shelld_session_id → badge string ("P<n> <uuid>")`.  Replaces
    /// `last_mapping` on the main side every time it arrives.
    new_mapping: HashMap<u64, String>,
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

        // -- jsonl pass: refresh self.seen ---------------------------
        let projects = match fs::read_dir(&self.projects_root) {
            Ok(d) => d,
            Err(_) => {
                // No projects dir yet — silent.  Still return so
                // tick clears any stale mapping.
                return ScanResult {
                    new_mapping: HashMap::new(),
                    new_meta: HashMap::new(),
                    log_lines,
                };
            }
        };
        let mut newly_seen = 0usize;
        let mut updates = 0usize;
        for project_entry in projects.flatten() {
            let project_path = project_entry.path();
            if !project_path.is_dir() {
                continue;
            }
            let project_dir = match project_path.file_name() {
                Some(n) => n.to_string_lossy().to_string(),
                None => continue,
            };
            let mut newest: Option<(PathBuf, SystemTime, u64)> = None;
            let dir = match fs::read_dir(&project_path) {
                Ok(d) => d,
                Err(_) => continue,
            };
            for f in dir.flatten() {
                let p = f.path();
                if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }
                let meta = match f.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                let size = meta.len();
                match &newest {
                    Some((_, prev_mtime, _)) if *prev_mtime >= mtime => {}
                    _ => newest = Some((p, mtime, size)),
                }
            }
            let Some((jsonl_path, mtime, size)) = newest else {
                continue;
            };
            if let Some(prev) = self.seen.get(&jsonl_path) {
                if prev.last_mtime == mtime && prev.last_size == size {
                    continue;
                }
            }
            let session_id = match parse_session_id(&jsonl_path) {
                Some(id) => id,
                None => continue,
            };
            let last_message_kind = tail_last_message_type(&jsonl_path);
            let is_new = !self.seen.contains_key(&jsonl_path);
            self.seen.insert(
                jsonl_path.clone(),
                SessionInfo {
                    session_id: session_id.clone(),
                    project_dir: project_dir.clone(),
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
        if newly_seen > 0 || updates > 0 {
            log_lines.push((
                LogLevel::Debug,
                "tick.summary",
                format!("new={} updated={}", newly_seen, updates),
            ));
        }

        // -- per-session mapping: BFS each shelld session ------------
        let mut new_mapping: HashMap<u64, String> = HashMap::new();
        let mut new_meta: HashMap<u64, BindMeta> = HashMap::new();
        let sessions = match self.shelld.list_sessions() {
            Ok(v) => v,
            Err(e) => {
                log_lines.push((
                    LogLevel::Info,
                    "tick.shelld_list_failed",
                    format!("{e}"),
                ));
                return ScanResult { new_mapping, new_meta, log_lines };
            }
        };
        let procs = pidtree::list_all_procs();
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
            let encoded = encode_project_dir(&cwd);
            if let Some((sid_uuid, jsonl_path)) = self.session_for_project(&encoded) {
                let (tag, profile_num) = match profile_tag_for(claude.pid) {
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
                // assistant turn.
                let model = tail_model_short(&jsonl_path);
                let badge = match (tag, model) {
                    (Some(t), Some(m)) => format!("{}@{} {}", t, m, sid_uuid),
                    (Some(t), None) => format!("{} {}", t, sid_uuid),
                    (None, _) => sid_uuid.clone(),
                };
                let project_basename = cwd
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                new_mapping.insert(s.session_id, badge);
                new_meta.insert(
                    s.session_id,
                    BindMeta {
                        profile_num,
                        uuid: sid_uuid,
                        claude_pid: claude.pid,
                        project_basename,
                    },
                );
                // No log line here on purpose — `session.bound` is
                // transition-only.  Main side diffs `result.new_mapping`
                // against `self.last_mapping` and only logs the deltas;
                // otherwise we'd write 12 lines per 2 s tick in steady
                // state and drown the file.
            }
        }

        ScanResult { new_mapping, new_meta, log_lines }
    }

    /// Reverse-lookup: encoded project dir → newest known session
    /// (id + its jsonl path, so callers can tail per-session state
    /// like the active model).  Cheap scan over `seen`; a dozen
    /// projects active in practice.
    fn session_for_project(&self, encoded_dir: &str) -> Option<(String, PathBuf)> {
        let mut newest: Option<(SystemTime, &SessionInfo)> = None;
        for s in self.seen.values() {
            if s.project_dir == encoded_dir {
                match newest {
                    Some((t, _)) if t >= s.last_mtime => {}
                    _ => newest = Some((s.last_mtime, s)),
                }
            }
        }
        newest.map(|(_, s)| (s.session_id.clone(), s.jsonl_path.clone()))
    }
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
        assert_eq!(tail_model_short(&path).as_deref(), Some("opus-4-8"));

        // ...and vice versa: an assistant turn after the switch wins.
        // (older "system"/"local_command" record shape)
        let path = tmpfile(concat!(
            r#"{"type":"system","subtype":"local_command","content":"<local-command-stdout>Kept model as \u001b[1mOpus 4.8\u001b[22m</local-command-stdout>"}"#,
            "\n",
            r#"{"type":"message","role":"assistant","model":"claude-fable-5","content":[]}"#,
            "\n",
        ));
        assert_eq!(tail_model_short(&path).as_deref(), Some("fable-5"));

        // No model anywhere -> None.
        let path = tmpfile(r#"{"type":"user","text":"hi"}"#);
        assert_eq!(tail_model_short(&path), None);

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
        assert_eq!(tail_model_short(&path).as_deref(), Some("fable-5"));
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

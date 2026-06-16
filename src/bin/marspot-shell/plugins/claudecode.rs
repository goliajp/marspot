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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use marspot::paths;
use marspot::shelld_client::ShelldClient;

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
    /// Keyed by jsonl path (uniquely identifies a session file).
    seen: HashMap<PathBuf, SessionInfo>,
    /// Where ~/.claude/projects lives.  Cached at init.
    projects_root: Option<PathBuf>,
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
    /// 0 = default `.claude`, 1/2/3 = `.claude-profile-N`, 255 = unknown.
    profile_num: u8,
    uuid: String,
    claude_pid: i32,
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
            seen: HashMap::new(),
            projects_root: None,
            shelld: None,
            last_mapping: HashMap::new(),
            last_meta: HashMap::new(),
            monitors: HashMap::new(),
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
            client.detach_raw(sid);
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
        let next_profile = match meta.profile_num {
            1 => 2,
            2 => 3,
            3 => 1,
            _ => 1,
        };
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

    /// Reverse-lookup: given an encoded project dir (e.g.
    /// `-Users-doracawl-workspace-foo`), return the latest sessionId
    /// we've seen for it.  Walks the `seen` map; cheap when only a
    /// dozen projects are active.
    fn session_id_for_project(&self, encoded_dir: &str) -> Option<String> {
        let mut newest: Option<(SystemTime, &SessionInfo)> = None;
        for s in self.seen.values() {
            if s.project_dir == encoded_dir {
                match newest {
                    Some((t, _)) if t >= s.last_mtime => {}
                    _ => newest = Some((s.last_mtime, s)),
                }
            }
        }
        newest.map(|(_, s)| s.session_id.clone())
    }
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
        self.projects_root = Some(PathBuf::from(home).join(".claude").join("projects"));
        // Connect to shelld so tick can map shelld_session_id →
        // child_pid → claude descendant → sessionId.  Best-effort:
        // if shelld is down, plugin still runs in global-scan-only
        // mode, no per-session mapping.
        let socket = paths::shelld_socket();
        let wake = Arc::new(AtomicBool::new(false));
        let wk = wake.clone();
        match ShelldClient::connect(&socket, move || {
            wk.store(true, Ordering::Release);
        }) {
            Ok(c) => {
                self.shelld = Some(Arc::new(c));
                host.log(
                    LogLevel::Info,
                    "init.shelld_connected",
                    &format!("shelld client up ({})", socket.display()),
                );
            }
            Err(e) => {
                host.log(
                    LogLevel::Warn,
                    "init.shelld_unavailable",
                    &format!(
                        "shelld at {} unavailable ({e}); per-session mapping disabled",
                        socket.display()
                    ),
                );
            }
        }
        host.log(
            LogLevel::Info,
            "init",
            &format!(
                "claudecode plugin initialised (projects_root={})",
                self.projects_root.as_ref().unwrap().display()
            ),
        );
        self.initialised = true;
        Ok(())
    }

    fn tick(&mut self, host: &dyn PluginHost) {
        if !self.initialised {
            return;
        }
        let root = match &self.projects_root {
            Some(r) => r.clone(),
            None => return,
        };
        // Walk one level down.  Each subdir = one project (encoded cwd).
        let projects = match fs::read_dir(&root) {
            Ok(d) => d,
            Err(_) => return, // No projects dir yet — silent
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
            // Find newest .jsonl in this project.
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
            // Update tracking.  Skip if path + mtime + size unchanged.
            if let Some(prev) = self.seen.get(&jsonl_path) {
                if prev.last_mtime == mtime && prev.last_size == size {
                    continue;
                }
            }
            // Parse sessionId from first line of the file.
            let session_id = match parse_session_id(&jsonl_path) {
                Some(id) => id,
                None => continue, // malformed; try again next tick
            };
            // Inspect last message type (tail-read).  Cheap heuristic:
            // read last 32 KB, find newline-anchored last record.
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
                host.log(
                    LogLevel::Info,
                    "session",
                    &format!(
                        "session detected sid={} project={} kind={} size={}",
                        session_id,
                        project_dir,
                        last_message_kind.as_deref().unwrap_or("?"),
                        size
                    ),
                );
            } else {
                updates += 1;
                host.log(
                    LogLevel::Debug,
                    "session.update",
                    &format!(
                        "sid={} kind={} size={}",
                        session_id,
                        last_message_kind.as_deref().unwrap_or("?"),
                        size
                    ),
                );
            }
        }
        if newly_seen > 0 || updates > 0 {
            host.log(
                LogLevel::Debug,
                "tick.summary",
                &format!("new={} updated={}", newly_seen, updates),
            );
        }

        // M3.2 — per-shelld-session mapping.  For each live shelld
        // session, BFS its zsh.pid for a `claude` descendant; if
        // found, encode its cwd and reverse-lookup the sessionId
        // from `self.seen`.  Log only on transitions to keep the
        // log file quiet for a steady-state session.
        let Some(client) = self.shelld.as_ref() else {
            return; // shelld unavailable; skip mapping silently
        };
        let sessions = match client.list_sessions() {
            Ok(v) => v,
            Err(e) => {
                host.log(
                    LogLevel::Info,
                    "tick.shelld_list_failed",
                    &format!("{e}"),
                );
                return;
            }
        };
        let procs = pidtree::list_all_procs();
        let mut new_mapping: HashMap<u64, String> = HashMap::new();
        let mut new_meta: HashMap<u64, BindMeta> = HashMap::new();
        for s in &sessions {
            if !s.alive {
                continue;
            }
            let descendants =
                pidtree::descendants_of(s.child_pid, &procs);
            let Some(claude) = descendants.iter().find(|d| looks_like_claudecode(d))
            else {
                continue; // pane isn't running claudecode right now
            };
            let Some(cwd) = pidtree::proc_cwd(claude.pid) else {
                continue;
            };
            let encoded = encode_project_dir(&cwd);
            if let Some(sid_uuid) = self.session_id_for_project(&encoded) {
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
                // Badge format: "<prefix> <uuid>".  The renderer
                // treats the text before the first space as the
                // clickable prefix and underlines it; everything
                // after is plain.
                let badge = match tag {
                    Some(t) => format!("{} {}", t, sid_uuid),
                    None => sid_uuid.clone(),
                };
                new_mapping.insert(s.session_id, badge);
                new_meta.insert(
                    s.session_id,
                    BindMeta {
                        profile_num,
                        uuid: sid_uuid,
                        claude_pid: claude.pid,
                    },
                );
            }
        }
        // Log transitions (bound / unbound) vs. last tick.
        for (sh_sid, cc_sid) in &new_mapping {
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
            if !new_mapping.contains_key(sh_sid) {
                host.log(
                    LogLevel::Info,
                    "session.unbound",
                    &format!(
                        "shelld_session={} (was sid={})",
                        sh_sid, cc_sid
                    ),
                );
                // Empty badge text = clear on L2 side.
                let _ = host.set_pane_badge(*sh_sid, "");
            }
        }
        // Re-push every active badge every tick (idempotent).  Why
        // not transition-only: L2 core can spawn/crash/respawn between
        // ticks (CORE_BOOT_LOOP, silent update, etc.); a transition-
        // only push leaves the freshly-spawned core with no badges
        // until something changes.  Per tick ≤ 9 small frames = a few
        // hundred bytes; cheap compared to staying out-of-sync.
        //
        // Badge text is the full UUID — same string the claudecode
        // `/resume` picker shows, so the user can map a pane to a
        // resume entry by eye.  Badge is its own variable (NOT a
        // suffix of `title`): L2 stores them separately, render
        // strip draws them as independent items on the same row.
        for (sh_sid, cc_sid) in &new_mapping {
            if let Err(e) = host.set_pane_badge(*sh_sid, cc_sid) {
                host.log(
                    LogLevel::Warn,
                    "pane_badge.set_failed",
                    &format!("{e}"),
                );
            }
        }
        self.last_mapping = new_mapping;
        self.last_meta = new_meta;
        // RFC-003: profile-cycle state machine lives in
        // ProfileCyclePaneSession::on_tick, driven by the L1 plugin
        // dispatcher.
        //
        // C7: auto-retry monitor.  Sync the monitor set with the
        // freshly-rebound `last_meta` (start one per new claude pid,
        // drop those whose claude exited), then drain whatever raw
        // PTY bytes accumulated since last tick and scan for
        // retryable error patterns.
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

    fn stop(&mut self, host: &dyn PluginHost) {
        host.log(LogLevel::Info, "stop", "plugin stopped");
        self.initialised = false;
        self.seen.clear();
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

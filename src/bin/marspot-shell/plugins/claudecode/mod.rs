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

mod idle;
mod quota;
mod model;
mod statusline;
mod worker;
pub use statusline::statusline_ingest;
use idle::*;
use model::*;
use statusline::*;
use worker::*;

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
    /// Ask the pane's L3 to hold its picture where it is (or release
    /// it).  Separate from `PANE_SESSION_CAP_FREEZE_GRID`, which only
    /// stops L2 drawing: that one dies with the core, and every silent
    /// update restarts the core.
    fn hold_grid(&self, session_id: u64, on: bool) -> std::io::Result<()>;
    /// Deliver text the way a paste would arrive.
    fn paste(&self, session_id: u64, text: &str) -> std::io::Result<()>;
    /// Tell the pane's L3 that its foreground program was taken down
    /// by us, so mouse reporting should stop.
    fn reset_mouse_reporting(&self, session_id: u64) -> std::io::Result<()>;
}

#[allow(dead_code)]
struct CcSessionInfo {
    pub session_id: u64,
    pub alive: bool,
    pub child_pid: i32,
    pub title: String,
}

/// The plugin's route to a pane is also the op runner's, so a script
/// can be handed the same client the rest of this plugin already uses.
impl pty_op::PtyIo for ShelldClient {
    fn send(&self, sid: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.send_input_to(sid, bytes)
    }
    fn hold(&self, sid: u64, on: bool) -> std::io::Result<()> {
        self.hold_grid_of(sid, on)
    }
    fn paste(&self, sid: u64, text: &str) -> std::io::Result<()> {
        match self.host_inject.as_ref() {
            Some(p) => p.paste(sid, text),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "no InjectInputProxy on this host (test build?)",
            )),
        }
    }
    fn reset_mouse_reporting(&self, sid: u64) -> std::io::Result<()> {
        match self.host_inject.as_ref() {
            Some(p) => p.reset_mouse_reporting(sid),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "no InjectInputProxy on this host (test build?)",
            )),
        }
    }
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
    fn hold_grid_of(&self, sid: u64, on: bool) -> std::io::Result<()> {
        match self.host_inject.as_ref() {
            Some(p) => p.hold_grid(sid, on),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "no InjectInputProxy on this host (test build?)",
            )),
        }
    }

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
use crate::plugins::pty_op;
use crate::plugins::{
    LogLevel, Plugin, PluginError, PluginHost, PluginMetadata,
    PLUGIN_API_VERSION,
};

/// Per-session state cached so we only re-log on real change.
#[derive(Clone, Debug)]
struct SessionInfo {
    session_id: String,
    project_dir: String, // encoded form, e.g. -Users-ada-workspace-...
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
    /// Per pane: the model it is working in, as the scan last read it.
    ///
    /// Only the quota chooser reads this. Fable carries a cap of its
    /// own on top of the account's 5h/7d windows, so for a pane
    /// pinned to it a profile with a fresh week is not necessarily a
    /// profile with room.
    last_model: HashMap<u64, String>,
    /// Panes stranded on an account that is out: how many moves have
    /// been spent on each, and where the last one was aimed.  An entry
    /// disappears when the account answers again.
    stranded: HashMap<u64, Stranding>,
    /// When each pane was last moved off a refused account.
    moved_at: HashMap<u64, Instant>,

    /// Previous tick's `shelld_session_id → ("<fg|bg>,<activity>",
    /// since)`, so `cc_status.changed` is transition-only and can say
    /// how long the state it replaces had held — the number any future
    /// idle threshold has to be calibrated against.  Pruned every tick
    /// to the sessions still reporting.
    last_activity: HashMap<u64, (String, Instant)>,
    /// Panes whose claude has been reclaimed and that are waiting for a
    /// keypress.  Persisted (`dormant.tsv`) because an L1 self-execv
    /// drops the PaneSessions that carry the wake path.
    dormant: Vec<DormantRecord>,
    /// Dormant panes that currently have a live `HibernatePaneSession`
    /// attached.  Split from `dormant` so a re-arm after an execv
    /// doesn't stack a second session onto a pane that still has one.
    armed: std::collections::HashSet<u64>,
    /// Panes this plugin has asked L3 to hold, and the look each one
    /// is frozen at.  Tracked so a hold can be undone even if the
    /// session that asked for it is gone (see the sweep in
    /// `rearm_dormant`) — and so the pane's chrome does not move while
    /// its picture is standing still.
    ///
    /// The badge belongs to the frozen frame as much as the cells do:
    /// claude going away drops the pane out of the scan's mapping, and
    /// the mapping is what clears badges.  A parked pane would blank
    /// its corner, which is the reclamation announcing itself.
    held: HashMap<u64, FrozenLook>,
    /// `shelld_session_id → (claude subtree CPU ns, sampled at)` from
    /// the previous scan, so the idle policy can look at a delta
    /// rather than an absolute.
    cpu_samples: HashMap<u64, (u64, SystemTime)>,
    /// Last logged "why not yet" per candidate pane, so the reason is
    /// logged on change rather than every scan.
    blocked_reason: HashMap<u64, String>,
    /// The pane the user's cursor is in, from `on_pane_focused`.
    ///
    /// The seat they left.  Reclamation is otherwise blind to where
    /// the user is: it reads the *session's* clock (transcript age),
    /// and a pane can be thirty minutes idle by that clock while the
    /// user is sitting in it reading.  Parking it there means the next
    /// keystroke costs a wake — which, now that a reclamation is
    /// invisible, reads as marspot going unresponsive for no reason.
    focused_sid: Option<u64>,
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
pub(crate) fn retryable_error_kind(buf: &[u8]) -> Option<&'static str> {
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


/// What has been spent on one stranded pane.
#[derive(Clone, Copy)]
struct Stranding {
    /// Moves made while this pane has been continuously refused.
    moves: u8,
    /// Where the last one was aimed, so a move that has not taken
    /// effect yet is not made twice.
    last_target: Option<u8>,
    /// When the pane was first seen stuck on an account that is out.
    since: Instant,
    /// Whether the reason for holding has already been said once.  A
    /// hold is a steady state, and the sweep runs every two seconds:
    /// without this, one held pane wrote 170 identical lines
    /// (2026-09-29, sid 430).
    hold_logged: bool,
}

impl Default for Stranding {
    fn default() -> Self {
        Self { moves: 0, last_target: None, since: Instant::now(), hold_logged: false }
    }
}

#[derive(Clone, Debug)]
struct BindMeta {
    /// 0 = default `.claude`, N = `.claude-profile-N`, 255 = unknown.
    profile_num: u8,
    /// `CLAUDE_CONFIG_DIR` as read off the live process — the ground
    /// truth of which profile this session belongs to.
    ///
    /// The resume line uses THIS rather than reconstructing
    /// `claude<N>`: those are interactive shell aliases
    /// (`alias claude1='CLAUDE_CONFIG_DIR=~/.claude-profile-1 claude'`),
    /// so reproducing them depends on the user's rc file still
    /// defining them, in that shell, at that moment.  Setting the
    /// variable we observed is the alias's own expansion, made
    /// explicit and independent of all of that.
    config_dir: Option<String>,
    uuid: String,
    claude_pid: i32,
    /// Basename of the claude process's cwd — used as the pane title
    /// so user sees "marspot" instead of "session-3" once cc binds.
    /// Empty when basename couldn't be resolved (e.g. process exited
    /// between scan and tick).
    project_basename: String,
    /// The whole of that cwd, which is where the session lives.
    ///
    /// `claude --resume` resolves a session id against the project of
    /// the directory it is run from, and binds what it reopens to that
    /// directory -- rules, hooks and the pane's title follow. The
    /// pane's shell is not reliably standing there: a profile switch
    /// that types the resume wherever the shell happens to be moved a
    /// torajs session into devops, and the only visible sign was the
    /// title changing.
    project_dir: Option<String>,
    /// Unix seconds this session was last refused by the API for want
    /// of quota, from its own transcript.
    ///
    /// The feed is collected behind what a session spends, and can be
    /// behind by a lot -- it carries a row's model caps forward when it
    /// cannot re-measure them, and one stood at 0.62 for 94 minutes
    /// while this message was written sixteen times. So the two are
    /// compared by when each was true, and the later one is believed.
    refused_at: Option<quota::Refusal>,
    /// When this session's transcript was last written.
    ///
    /// The honest answer to "how long has this session been idle".
    /// The pane's own quiet clock is not: claude prints `Checking for
    /// updates` in the corner every thirty minutes, which makes the
    /// terminal busy for half a minute and resets that clock — on this
    /// machine it fired 23 times in one pane's log, so a session idle
    /// for eleven hours never once accumulated thirty quiet minutes.
    /// Chrome does not touch the transcript.
    transcript_at: SystemTime,
    /// Whether claude has written this session's transcript yet.
    ///
    /// False is not a gap — it is the defining fact about a session
    /// that has not taken a turn: there is nothing to `--resume`, so
    /// the profile cycle starts claude plain instead.  See
    /// [`profile_cycle_op`].
    has_transcript: bool,
}








impl ClaudecodePlugin {
    pub fn new() -> Self {
        Self {
            initialised: false,
            shelld: None,
            last_mapping: HashMap::new(),
            last_meta: HashMap::new(),
            last_model: HashMap::new(),
            stranded: HashMap::new(),
            moved_at: HashMap::new(),

            last_activity: HashMap::new(),
            dormant: Vec::new(),
            armed: std::collections::HashSet::new(),
            held: HashMap::new(),
            cpu_samples: HashMap::new(),
            blocked_reason: HashMap::new(),
            focused_sid: None,
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

    /// Reclaim the claude in any pane the state machine says has been
    /// quiet long enough, and that shows no CPU of its own.
    ///
    /// Every gate here is a veto; nothing "votes for" hibernating.
    /// The state machine already refuses to call a pane quiet without
    /// three agreeing observations, so this adds only what the machine
    /// cannot see: whether the subtree is burning CPU behind a silent
    /// transcript, and how long ago we last looked.
    fn run_idle_policy(&mut self, host: &dyn PluginHost, result: &ScanResult) {
        let Some(threshold) = hibernate_after() else {
            return;
        };
        self.run_idle_policy_at(host, result, threshold);
    }

    /// [`run_idle_policy`] with the threshold handed in.
    ///
    /// The threshold is the user's setting, and the policy used to read
    /// it for itself — so every test of the policy ran against whatever
    /// the machine running it had configured.  On a machine with
    /// reclamation switched off, three tests failed and two more passed
    /// for the wrong reason ("nothing was reclaimed" holds trivially when
    /// nothing is allowed to be), while the same suite was green on the
    /// bench host, which has no settings file.  The decision takes the
    /// number; where the number comes from is the caller's business.
    fn run_idle_policy_at(
        &mut self,
        host: &dyn PluginHost,
        result: &ScanResult,
        threshold: Duration,
    ) {
        // Not wired to a shelld yet: nothing to reclaim into.
        if self.shelld.is_none() {
            return;
        }
        for (sid, (cpu_now, sampled_at)) in &result.new_cpu {
            // No "have I already done this one" check: a pane we
            // reclaimed reports `Dormant`, which composes to a state
            // that is not `AwaitingUser`, so the gate below excludes it
            // on the same evidence everyone else uses.  The private
            // dormant set is for the wake path, not for decisions.
            let Ok(Some(view)) = host.pane_status(*sid) else {
                continue; // no machine for this pane yet
            };
            let Some((cpu_prev, prev_at)) = self.cpu_samples.get(sid).copied() else {
                // First sample for this pane: record it and decide on
                // the next pass, when there is a delta to look at.
                self.cpu_samples.insert(*sid, (*cpu_now, *sampled_at));
                continue;
            };
            let (work_in_flight, own_timer) =
                result.new_vetoes.get(sid).copied().unwrap_or((true, true));
            let evidence = IdleEvidence {
                work_in_flight,
                own_timer,
                user_here: self.focused_sid == Some(*sid),
                quiescent: view.quiescent,
                awaiting_user: matches!(
                    view.status,
                    marspot::pane_state::PaneStatus::AwaitingUser
                ),
                held: view.held,
                idle_for: result
                    .new_meta
                    .get(sid)
                    .map(|m| {
                        SystemTime::now()
                            .duration_since(m.transcript_at)
                            .unwrap_or_default()
                    })
                    // No transcript for this pane means no evidence of
                    // idleness, not permission to act on none.
                    .unwrap_or_default(),
                cpu_delta_ns: cpu_now.saturating_sub(cpu_prev),
                since_sample: sampled_at.duration_since(prev_at).unwrap_or_default(),
            };
            if !should_hibernate(evidence, threshold) {
                // Say why, once per change of reason.  Without this the
                // log shows nothing at all until the moment a session
                // is reclaimed, and "nothing happened" reads the same
                // whether the policy is waiting or broken.
                if let Some((category, line)) = blocking_reason(evidence, threshold) {
                    // De-duplicate on the CATEGORY: the line carries a
                    // live number, so comparing lines would log every
                    // scan (it did — see `blocking_reason`).
                    if self.blocked_reason.get(sid).map(String::as_str) != Some(category) {
                        host.log(
                            LogLevel::Info,
                            "hibernate.waiting",
                            &format!("shelld_session={} — {}", sid, line),
                        );
                        self.blocked_reason.insert(*sid, category.to_string());
                    }
                } else {
                    self.blocked_reason.remove(sid);
                }
                // Replace the baseline only once it is older than the
                // window: rolling it forward every scan is what made
                // the delta span 2 s and the gate unreachable.
                if evidence.since_sample >= CPU_BASELINE_WINDOW {
                    self.cpu_samples.insert(*sid, (*cpu_now, *sampled_at));
                }
                continue;
            }
            self.blocked_reason.remove(sid);
            // This scan's binding, not the previous tick's: the pid
            // is about to be signalled, so it should be the freshest
            // one we have.  (`last_meta` is only updated after this
            // runs, which would hand the policy a pid one scan old.)
            let Some(meta) = result.new_meta.get(sid).cloned() else {
                continue;
            };
            // A session we cannot name the profile of cannot be
            // brought back correctly: the resume line decides which
            // config dir — which account — claude comes back under.
            // Not reclaiming costs memory; reclaiming costs the user
            // their session in the wrong place.
            if meta.profile_num == PROFILE_UNKNOWN {
                if self.blocked_reason.get(sid).map(String::as_str)
                    != Some("unknown_profile")
                {
                    host.log(
                        LogLevel::Warn,
                        "hibernate.unknown_profile",
                        &format!(
                            "shelld_session={} uuid={} — profile unreadable; not reclaiming",
                            sid, meta.uuid
                        ),
                    );
                    self.blocked_reason.insert(*sid, "unknown_profile".to_string());
                }
                continue;
            }
            // Badged from its profile alone — claude is running but has
            // not written a session file yet, so there is no uuid to
            // resume.  Reclaiming would take the pane down with no way
            // back; leave it running.
            if meta.uuid.is_empty() {
                if self.blocked_reason.get(sid).map(String::as_str)
                    != Some("unknown_session")
                {
                    host.log(
                        LogLevel::Warn,
                        "hibernate.unknown_session",
                        &format!(
                            "shelld_session={sid} — no session file yet; not reclaiming"
                        ),
                    );
                    self.blocked_reason.insert(*sid, "unknown_session".to_string());
                }
                continue;
            }
            // Re-verify the pid immediately before signalling it.
            // `last_meta` is up to one scan old, pids are recycled by
            // the kernel, and the consequence of acting on a stale one
            // is sending SIGTERM to an unrelated process.  Cheap check,
            // unbounded downside without it.
            let still_claude = pidtree::proc_cmdline(meta.claude_pid)
                .map(|line| {
                    let first = line.split_whitespace().next().unwrap_or("");
                    std::path::Path::new(first)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with("claude") || n == "node")
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            if !still_claude {
                host.log(
                    LogLevel::Warn,
                    "hibernate.pid_no_longer_claude",
                    &format!(
                        "shelld_session={} pid={} is not claude any more; not signalling",
                        sid, meta.claude_pid
                    ),
                );
                self.cpu_samples.remove(sid);
                continue;
            }
            host.log(
                LogLevel::Info,
                "hibernate.start",
                &format!(
                    // `idle` is the clock that DECIDES (the session's
                    // transcript age).  It used to print `held` — the
                    // pane's own state clock — which reads 0s or 1s on
                    // a session that has been idle for hours, so the
                    // log said the policy had fired on nothing.
                    "shelld_session={} uuid={} idle={}s (pane state held {}s) \
                     cpu_delta={}ms over {}s — reclaiming pid {}",
                    sid,
                    meta.uuid,
                    evidence.idle_for.as_secs(),
                    evidence.held.as_secs(),
                    evidence.cpu_delta_ns / 1_000_000,
                    evidence.since_sample.as_secs(),
                    meta.claude_pid,
                ),
            );
            // Hold the pane's picture BEFORE anything else happens on
            // it.  Everything from here to the far side of the wake —
            // claude's exit, the shell prompt, the resume line,
            // claude's startup — has to happen behind the frame the
            // user last saw.
            //
            // The hold lives in L3, not in the core: a silent update
            // restarts the core, and a core-side freeze evaporates
            // with it.  That is what put a bare zsh prompt in a parked
            // pane after an update.
            //
            // The signal itself is sent from the `Holding` stage, once
            // this request has had time to land — see that variant.
            let Some(op) = reclaim_op(
                &meta.uuid,
                meta.config_dir.as_deref(),
                meta.claude_pid,
                shell_pid_for(*sid),
                meta.project_dir.as_deref(),
            ) else {
                host.log(
                    LogLevel::Warn,
                    "hibernate.no_resume_line",
                    &format!(
                        "refusing to reclaim {}: its profile cannot be quoted",
                        meta.uuid
                    ),
                );
                continue;
            };
            // Freeze the chrome at the same instant as the picture:
            // this scan still sees the claude we are about to take
            // down, so it is the last one that knows what the corner
            // of this pane says.
            self.held.insert(
                *sid,
                FrozenLook {
                    badge: result
                        .new_mapping
                        .get(sid)
                        .cloned()
                        .unwrap_or_else(|| format!("P{}", meta.profile_num)),
                    title: meta.project_basename.clone(),
                },
            );
            if let Err(e) = host.submit_pty_op(*sid, op) {
                host.log(
                    LogLevel::Warn,
                    "hibernate.submit_failed",
                    &format!("pane {sid}: {e}"),
                );
                continue;
            }
            // This run *is* the pane's wake path, so mark it armed.
            // Without this the re-arm sweep sees an un-armed dormant
            // record two seconds later and starts a second run on the
            // same pane — which replaces the first mid-park, and leaves
            // two paths racing to type the same line.  Observed live.
            self.armed.insert(*sid);
            self.dormant.push(DormantRecord {
                shelld_sid: *sid,
                uuid: meta.uuid,
                profile_num: meta.profile_num,
                config_dir: meta.config_dir,
                project_dir: meta.project_dir,
                created_at: SystemTime::now(),
            });
            self.persist_dormant(host);
        }
        // Forget samples for panes that are gone.
        self.cpu_samples
            .retain(|sid, _| result.sessions_seen.contains(sid));
        self.blocked_reason
            .retain(|sid, _| result.sessions_seen.contains(sid));
    }

    /// Push every pane's badge + title for this scan.
    ///
    /// Three populations, and the middle one is why this is its own
    /// method: panes that just lost their claude (clear), panes we are
    /// deliberately holding (keep exactly what they were frozen with),
    /// and panes running claude now (the live badge).
    ///
    /// Re-pushed every tick rather than on change: L2 can spawn /
    /// crash / respawn between ticks (CORE_BOOT_LOOP, silent update),
    /// and transition-only would leave a fresh core with no badges
    /// until something moved.  Per tick ≤ 9 small frames.
    fn publish_looks(&self, host: &dyn PluginHost, result: &ScanResult) {
        for (sh_sid, cc_sid) in &self.last_mapping {
            if result.new_mapping.contains_key(sh_sid) {
                continue;
            }
            // A held pane lost its claude because WE took it down.
            // Clearing its corner would be the reclamation announcing
            // itself on a pane whose whole point is standing still —
            // the frozen look is re-asserted below instead.
            if self.held.contains_key(sh_sid) {
                continue;
            }
            host.log(
                LogLevel::Info,
                "session.unbound",
                &format!("shelld_session={} (was sid={})", sh_sid, cc_sid),
            );
            let _ = host.set_pane_badge(*sh_sid, "");
            let _ = host.set_pane_agent_tui(*sh_sid, false);
            let _ = host.set_pane_title(*sh_sid, "");
        }
        // Held panes wear the badge and title they were frozen with.
        for (sh_sid, look) in &self.held {
            if result.new_mapping.contains_key(sh_sid) {
                continue;
            }
            let _ = host.set_pane_badge(*sh_sid, &look.badge);
            let _ = host.set_pane_agent_tui(*sh_sid, true);
            if !look.title.is_empty() {
                let _ = host.set_pane_title(*sh_sid, &look.title);
            }
        }
        for (sh_sid, cc_sid) in &result.new_mapping {
            // Dev-cycle diagnostic.  The badge is assembled from four
            // separate lookups (profile tag, bound uuid, transcript
            // path, model) and any one going quiet leaves a half-badge
            // that says nothing about which — `P3` with no `@model`
            // took a screenshot and a manual dig to explain
            // (2026-08-10).  Logged only when it *changes*, so a
            // steady screen costs nothing.
            if self.last_mapping.get(sh_sid) != Some(cc_sid) {
                let meta = result.new_meta.get(sh_sid);
                host.log(
                    LogLevel::Info,
                    "badge.changed",
                    &format!(
                        "sid={sh_sid} badge={cc_sid:?} was={:?} cfg={:?} uuid={:?}",
                        self.last_mapping.get(sh_sid).map(|s| s.as_str()).unwrap_or("-"),
                        meta.and_then(|m| m.config_dir.as_deref()).unwrap_or("-"),
                        meta.map(|m| m.uuid.as_str()).unwrap_or("-"),
                    ),
                );
            }
            // Re-issued every tick: a core swap starts L2 with an
            // empty map, and this is what tells it to merge the hard
            // wraps a fixed-width TUI puts in the middle of a path.
            let _ = host.set_pane_agent_tui(*sh_sid, true);
            if let Err(e) = host.set_pane_badge(*sh_sid, cc_sid) {
                host.log(LogLevel::Warn, "pane_badge.set_failed", &format!("{e}"));
            }
            // Title 只用 project basename — profile 已经在 badge 里
            // ("P3 …"),title 再重复就冗余.
            if let Some(meta) = result.new_meta.get(sh_sid)
                && !meta.project_basename.is_empty()
                    && let Err(e) = host.set_pane_title(*sh_sid, &meta.project_basename) {
                        host.log(
                            LogLevel::Warn,
                            "pane_title.set_failed",
                            &format!("{e}"),
                        );
                    }
        }
    }

    /// Put the wake path back after it was lost.
    ///
    /// An L1 self-execv replaces the process and every PaneSession
    /// with it, while the pane itself survives with claude already
    /// reclaimed.  Without this, the next keystroke in a dormant pane
    /// runs as a shell command and the session is only recoverable by
    /// typing the resume line by hand.
    ///
    /// Also drops records for panes where claude is back (woken, or
    /// started by the user), which is what keeps the set bounded.
    fn rearm_dormant(&mut self, host: &dyn PluginHost, result: &ScanResult) {
        let Some(client) = self.shelld.as_ref().cloned() else {
            return;
        };
        let before = self.dormant.len();
        // A pane that reports a binding again has a live claude: it is
        // no longer dormant, whoever woke it.
        self.dormant.retain(|d| {
            // A scan may not judge a record until the kill it was made
            // for has had time to land.  The scan that triggers the
            // reclamation predates it, which is obvious; the ones for
            // the next few seconds are the subtle case — claude takes
            // longer than one 2 s tick to flush and exit, so those
            // scans still find it alive and still hold its binding.
            // Reading that as "claude is back" drops the record, and
            // with it the wake path.  Observed on the real machine at
            // 21:15: pane 384 was reclaimed and `dormant.tsv` was
            // rewritten empty in the same minute, leaving the pane at
            // a bare shell instead of a resumable session.
            if result.scanned_at <= d.created_at + KILL_GRACE {
                return true;
            }
            !result.new_mapping.contains_key(&d.shelld_sid)
                && result.sessions_seen.contains(&d.shelld_sid)
        });
        // Ask the kernel, not the scan, whether the pane is really
        // empty.  A binding can lag a restart by a scan or two, and
        // arming a wake on a pane that already has a live claude means
        // the next focus types `claude --resume …` **into that running
        // claude's prompt** — where it sits waiting for the user to
        // press Enter.  That is precisely the shape of the "I have to
        // press Enter" report, and it costs one proc-table walk to
        // make impossible.
        //
        // Only when there is a record waiting to be armed.  The list
        // is empty almost always — a pane is reclaimed once, armed
        // once, and stays armed — while the walk is one
        // `proc_pidinfo` per pid on the host, measured at 0.46-1.33 ms
        // for 1200 processes here.  Small, but it is the thread that
        // draws the window and it was paying it every two seconds for
        // nothing.
        let procs = if self.dormant.iter().any(|d| !self.armed.contains(&d.shelld_sid)) {
            pidtree::list_all_procs()
        } else {
            Vec::new()
        };
        let rearm: Vec<DormantRecord> = self
            .dormant
            .iter()
            .filter(|d| !self.armed.contains(&d.shelld_sid))
            .filter(|d| {
                let shell = shell_pid_for(d.shelld_sid);
                shell > 0
                    && !pidtree::descendants_of(shell, &procs)
                        .iter()
                        .any(looks_like_claudecode)
            })
            .cloned()
            .collect();
        for d in rearm {
            // The same script, started at the park: there is nothing
            // left to kill, and the pid it would have signalled died
            // with the process that armed it.
            let Some(op) = reclaim_op(
                &d.uuid,
                d.config_dir.as_deref(),
                0,
                shell_pid_for(d.shelld_sid),
                d.project_dir.as_deref(),
            ) else {
                host.log(
                    LogLevel::Warn,
                    "hibernate.no_resume_line",
                    &format!("cannot re-arm {}: its profile cannot be quoted", d.uuid),
                );
                continue;
            };

            // Idempotent by design: if this pane is already held (the
            // usual case — L3 outlived the L1 that asked), L3 sees no
            // change.  If a *new* L3 came up meanwhile, this is what
            // puts the hold back.
            // The look this pane is frozen at, rebuilt from the
            // record: an L1 self-execv dropped whatever the badge said
            // in full, and the profile is the half that survives.  It
            // is what the badge would read anyway until the model is
            // read back off the resumed session.
            self.held.entry(d.shelld_sid).or_insert_with(|| FrozenLook {
                badge: if d.profile_num == PROFILE_UNKNOWN {
                    "cc".to_string()
                } else {
                    format!("P{}", d.profile_num)
                },
                title: String::new(),
            });
            let _ = client.hold_grid_of(d.shelld_sid, true);
            let park = op.index_of(RECLAIM_PARK_LABEL).unwrap_or(0);
            match host.submit_pty_op_at(d.shelld_sid, op, park) {
                Ok(()) => {
                    self.armed.insert(d.shelld_sid);
                    host.log(
                        LogLevel::Info,
                        "hibernate.rearmed",
                        &format!("dormant session {} can be woken again", d.uuid),
                    );
                }
                Err(e) => host.log(
                    LogLevel::Warn,
                    "hibernate.rearm_failed",
                    &format!("pane {}: {e}", d.shelld_sid),
                ),
            }
        }
        self.armed.retain(|sid| {
            self.dormant.iter().any(|d| d.shelld_sid == *sid)
        });
        // A held pane whose claude is back must be drawing again.
        //
        // The session releases its own hold when the wake finishes,
        // but that release travels L1 → L2 → L3 and a core that is
        // restarting at that instant drops it — leaving a live pane
        // showing a picture from before the reclamation.  This is the
        // sweep that notices.  Costs a frame only when it is the one
        // fixing something: `held` is empty in the ordinary case.
        let back: Vec<u64> = self
            .held
            .keys()
            .copied()
            // A pane with a wake armed is one we are deliberately
            // holding — leave it alone.  The scan's mapping is up to
            // two seconds old, so the pass right after a reclamation
            // still shows the claude we just killed; acting on that
            // released the hold six milliseconds into the park, and
            // the pane spent its whole parked life showing a shell
            // prompt instead of the frame the user left.
            .filter(|sid| !self.armed.contains(sid))
            .filter(|sid| result.new_mapping.contains_key(sid))
            .collect();
        for sid in back {
            self.held.remove(&sid);
            if client.hold_grid_of(sid, false).is_ok() {
                host.log(
                    LogLevel::Info,
                    "hibernate.hold_released",
                    &format!("pane {sid} has a live claude again"),
                );
            }
        }
        if self.dormant.len() != before {
            self.persist_dormant(host);
        }
    }

    /// Write the dormant set to the plugin's state dir.  Best-effort:
    /// losing it costs a wake path, not a session.
    fn persist_dormant(&self, host: &dyn PluginHost) {
        let Ok(dir) = host.state_dir() else { return };
        let path = dir.join("dormant.tsv");
        if let Err(e) = fs::write(&path, encode_dormant(&self.dormant)) {
            host.log(
                LogLevel::Warn,
                "hibernate.persist_failed",
                &format!("{e}"),
            );
        }
    }

    /// Read back what an earlier process left dormant.  Called once at
    /// init; `rearm_dormant` does the rest on the next scan.
    fn load_dormant(&mut self, host: &dyn PluginHost) {
        let Ok(dir) = host.state_dir() else { return };
        let Ok(text) = fs::read_to_string(dir.join("dormant.tsv")) else {
            return;
        };
        self.dormant = decode_dormant(&text);
        if !self.dormant.is_empty() {
            host.log(
                LogLevel::Info,
                "hibernate.loaded",
                &format!("{} dormant session(s) carried over", self.dormant.len()),
            );
        }
    }

    /// Kick off the profile cycle for `shelld_session_id`.  Called
    /// from `on_pane_badge_click`.  Builds a ProfileCyclePaneSession
    /// and hands it to the host; the host freezes the grid + locks
    /// Which model this pane needs room in, when the feed caps one
    /// separately.
    ///
    /// `None` for everything the feed does not meter on its own --
    /// which is every pane except the Fable ones, and for those
    /// nothing about the choice changes.
    fn needs_model_for(&self, sid: u64) -> Option<String> {
        self.last_model.get(&sid).cloned()
    }

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
        // The best account for the model this pane is on, from what the
        // accounts say about themselves. Without that there is no
        // choice to make: walking to the next number was a guess, and
        // with most accounts spent it was usually a refused one.
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let usage = marspot::cc_usage::read();
        let needs = self.needs_model_for(shelld_sid);
        let picked = match (&usage, &home) {
            (Some(u), Some(home)) => {
                let rooms = quota::rooms_for(&profiles, &u.accounts, |p| {
                    quota::profile_email(home, p)
                });
                host.log(
                    LogLevel::Info,
                    "cycle.rooms",
                    &format!("current=P{} model={needs:?} rooms={rooms:?}", meta.profile_num),
                );
                quota::best_profile(&rooms, meta.profile_num, needs.as_deref()).filter(|p| {
                    rooms
                        .iter()
                        .find(|(n, _)| n == p)
                        .is_some_and(|(_, r)| !r.refused_for(needs.as_deref()))
                })
            }
            _ => None,
        };
        let Some(next_profile) = picked else {
            let why = if usage.is_some() { "no account has room for this model" } else { "no usage feed" };
            host.log(
                LogLevel::Info,
                "cycle.no_target",
                &format!("pane {shelld_sid} stays on P{}: {why}", meta.profile_num),
            );
            let badge = if usage.is_some() { "⚠ no account free" } else { "⚠ no usage data" };
            let _ = host.submit_pty_op(shelld_sid, cycle_blocked_op(badge));
            return;
        };
        host.log(
            LogLevel::Info,
            "cycle.profiles",
            &format!("discovered={profiles:?} current=P{} next=P{next_profile}", meta.profile_num),
        );
        // A hand click is someone who is right here: they can say
        // whatever they meant to say.
        self.switch_by_hand(host, shelld_sid, meta, next_profile);
    }


    /// Every pane sitting on an account that cannot answer moves to
    /// one that can.
    ///
    /// Driven by what the accounts say about themselves, not by
    /// reading the screen or the transcript for the words a refusal
    /// happens to print.  Those words move, they arrive as an agent's
    /// failure as readily as a turn's, and the record carrying them is
    /// pushed out of any window worth reading — one session's sat
    /// 125 KiB deep in a 755 MB file while the CLI wrote menus after
    /// it.  The account's own status has none of those problems: it is
    /// one field, it is true for every pane on that account at once,
    /// and it is already on this machine.
    fn sweep_unusable_profiles(&mut self, host: &dyn PluginHost, result: &ScanResult) {
        if self.last_meta.is_empty() {
            return;
        }
        let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
            return;
        };
        let Some(usage) = marspot::cc_usage::read() else {
            // No feed is not "all clear" — it is "no answer", and a
            // pane is not moved on no answer.
            return;
        };
        let profiles = discover_profiles();
        let rooms = quota::rooms_for(&profiles, &usage.accounts, |p| {
            quota::profile_email(&home, p)
        });
        // Whether a profile is out is a question about a pane, not
        // about a profile: a pane pinned to a model can be shut out of
        // an account whose week is fine, and a pane that has just been
        // refused knows something the feed has not collected yet.
        let is_out = |profile: u8, model: Option<&str>, refusal: Option<&quota::Refusal>| -> bool {
            rooms
                .iter()
                .find(|(p, _)| *p == profile)
                .is_some_and(|(_, r)| r.refused_for_now(model, refusal))
        };
        // However the pane got where it is -- by hand, by a move, or
        // started there -- one rule: what it is running against what its
        // profile has left for that model.
        let out_for = |sid: &u64, meta: &BindMeta| -> bool {
            let model = result.new_models.get(sid).map(String::as_str);
            is_out(meta.profile_num, model, meta.refused_at.as_ref())
        };
        // Forget what was tried for a pane that is out of trouble —
        // but not for one that is merely between bindings.  A cycle
        // kills the CLI and the scan cannot bind the pane again until
        // the new process exists, so for a tick or two `last_meta` has
        // nothing to say about it.  Dropping the tally there loses the
        // fact that a move is already on its way, and the next pass
        // starts a second one on top of the first (seen on pane 442,
        // 2026-09-29: three cycles in 40 seconds).
        self.stranded.retain(|sid, _| {
            if !result.sessions_seen.contains(sid) {
                return false;
            }
            match self.last_meta.get(sid) {
                Some(m) => out_for(sid, m),
                None => true,
            }
        });
        // Bounded with the panes, like everything else here.
        self.moved_at.retain(|sid, _| result.sessions_seen.contains(sid));
        let sids: Vec<u64> = self.last_meta.keys().copied().collect();
        for sid in sids {
            let Some(meta) = self.last_meta.get(&sid).cloned() else { continue };
            if !out_for(&sid, &meta) {
                continue;
            }
            let first_time = !self.stranded.contains_key(&sid);
            self.stranded.entry(sid).or_default();
            if first_time {
                host.log(
                    LogLevel::Info,
                    "quota.stranded",
                    &format!("sid={sid} is on P{}, which is out", meta.profile_num),
                );
            }
            self.move_off_refused(host, sid, &rooms, meta, result);
        }
    }

    /// Move one pane off an account that has stopped answering.
    fn move_off_refused(
        &mut self,
        host: &dyn PluginHost,
        sid: u64,
        rooms: &[(u8, quota::Room)],
        meta: BindMeta,
        result: &ScanResult,
    ) {
        // One question, answered from structure: is a command of the
        // user's running on this machine right now — then leave the
        // pane alone until it finishes, because moving kills the
        // process and takes the command with it.
        //
        // There used to be a second question — was this pane working,
        // so should it be told to carry on.  It is gone, and the
        // sentence now goes to every pane that is moved.  The reading
        // was wrong half the time in the direction that costs the
        // most: of six panes moved in silence on 2026-09-30, three had
        // a turn killed mid-flight by the very refusal that triggered
        // the move.  They look idle because they are: the CLI printed
        // its error, closed the turn, and the transcript's last record
        // is a finished one.  Nothing in the structure separates
        // "finished" from "cut off" after the fact.
        //
        // Saying it to a pane that really was idle costs one line,
        // which is what the sentence is worded for — it asks the
        // session to continue *if* something was interrupted.
        let activity = result.new_activity.get(&sid);
        let tool_executing = matches!(activity, Some(CcActivity::ToolPending { executing: true }));
        // Read before the Situation so the borrow ends here: the
        // model is a fact about this pane, and the decision is about
        // where it can go next.
        let needs_model = result.new_models.get(&sid).cloned();
        let decision = quota::next_move(&quota::Situation {
            rooms,
            current: meta.profile_num,
            needs_model: needs_model.as_deref(),
            tool_executing,
            moves_this_episode: self.stranded.get(&sid).map(|s| s.moves).unwrap_or(0),
            last_target: self.stranded.get(&sid).and_then(|s| s.last_target),
            stranded_for: self.stranded.get(&sid).map(|s| s.since.elapsed()),
            since_last_move: self.moved_at.get(&sid).map(|at| at.elapsed()),
        });
        match decision {
            quota::Move::Hold(why) => {
                let tally = self.stranded.entry(sid).or_default();
                if !tally.hold_logged {
                    tally.hold_logged = true;
                    host.log(
                        LogLevel::Info,
                        "quota.hold",
                        &format!("sid={sid} stays on P{}: {why}", meta.profile_num),
                    );
                }
            }
            quota::Move::To(target) => {
                // Nothing moves on a guess about the pane itself: the
                // model it is on has to be known, and the hook's record
                // that the new claude is started from has to name the
                // same one. Otherwise the new claude comes up on the new
                // profile's default, which is how a pane switched to
                // Opus came back on Fable (2026-10-03, pane 442).
                let choice = pushed_choice(&meta.uuid);
                if let Err(why) = confirm_choice(needs_model.as_deref(), &choice) {
                    let tally = self.stranded.entry(sid).or_default();
                    if !tally.hold_logged {
                        tally.hold_logged = true;
                        host.log(
                            LogLevel::Info,
                            "quota.hold",
                            &format!(
                                "sid={sid} stays on P{}: {why} (scan={:?} hook={:?})",
                                meta.profile_num, needs_model, choice.model
                            ),
                        );
                    }
                    return;
                }
                host.log(
                    LogLevel::Info,
                    "quota.moving",
                    &format!("sid={sid} P{} → P{target}", meta.profile_num),
                );
                if self.start_profile_cycle_to(host, sid, meta, target, true, choice) {
                    self.moved_at.insert(sid, Instant::now());
                    let tally = self.stranded.entry(sid).or_default();
                    tally.moves += 1;
                    tally.last_target = Some(target);
                }
            }
        }
    }

    /// A switch someone asked for, remembered so the sweep leaves the
    /// pane where they put it.
    ///
    /// The pane still reads as being on the profile it is leaving
    /// until the new process is bound, and the sweep must not start a
    /// move of its own over the top of this one: the same "not landed"
    /// mark an automatic move leaves covers that gap.
    fn switch_by_hand(&mut self, host: &dyn PluginHost, sid: u64, meta: BindMeta, target: u8) {
        // Asked for or not, a switch starts a new claude, and it starts
        // on the model the pane is on or not at all.
        let choice = pushed_choice(&meta.uuid);
        let model = self.needs_model_for(sid);
        if let Err(why) = confirm_choice(model.as_deref(), &choice) {
            host.log(
                LogLevel::Warn,
                "cycle.model_unconfirmed",
                &format!(
                    "pane {sid}: not switching to P{target}: {why} (scan={model:?} hook={:?})",
                    choice.model
                ),
            );
            let _ = host.submit_pty_op(sid, cycle_blocked_op("⚠ model unknown"));
            return;
        }
        if self.start_profile_cycle_to(host, sid, meta, target, false, choice) {
            self.stranded.entry(sid).or_default().last_target = Some(target);
        }
    }

    /// Start the exit → resume cycle towards an explicit target
    /// profile.  Shared tail of the badge left-click (auto-next) and
    /// the badge context-menu "switch to Px" pick (explicit target).
    ///
    /// Returns whether a cycle was actually handed to the host.  Every
    /// caller that keeps a tally of moves needs that answer: a refusal
    /// to build one (no session name yet, a busy background holder)
    /// leaves the pane exactly where it was, and counting it as a move
    /// spends the budget that was supposed to pay for a real one.
    fn start_profile_cycle_to(
        &mut self,
        host: &dyn PluginHost,
        shelld_sid: u64,
        meta: BindMeta,
        next_profile: u8,
        say_continue: bool,
        // the model and effort the new claude is started with
        choice: CliChoice,
    ) -> bool {
        if self.shelld.is_none() {
            host.log(
                LogLevel::Warn,
                "cycle.no_shelld",
                "no shelld client; cannot start cycle",
            );
            return false;
        }
        // Before the signal, not after it.  This cycle kills first and
        // resumes second, so a resume the CLI was always going to
        // refuse costs the pane its session rather than just failing.
        let next_dir =
            PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join(format!(".claude-profile-{next_profile}"));
        let holder = background_session_holder(&next_dir, &meta.uuid);
        // An idle background job is stopped as part of the cycle — the
        // CLI's own remedy ("claude stop <job> first to resume it here"),
        // and what the user asked for: the switch completes and the
        // conversation comes back in the pane (2026-09-17).  Idle means
        // nothing is in flight, so what stops is a process, not work.
        // A job that is busy is left alone: stopping it would cut off
        // whatever it is doing, and that is not a badge click's to take.
        if let Some(h) = holder.filter(|h| !h.idle) {
            host.log(
                LogLevel::Warn,
                "cycle.background_session",
                &format!(
                    "pane {shelld_sid}: uuid={} is held by a background session (pid {}) that is \
                     busy; not cycling — stopping it would cut off its work, and \
                     `claude --resume` refuses while it runs",
                    meta.uuid, h.pid
                ),
            );
            if let Err(e) = host.submit_pty_op(shelld_sid, cycle_blocked_op("⚠ held by bg job")) {
                host.log(
                    LogLevel::Warn,
                    "cycle.blocked_badge_failed",
                    &format!("pane {shelld_sid}: {e}"),
                );
            }
            return false;
        }
        if let Some(h) = holder {
            host.log(
                LogLevel::Info,
                "cycle.stopping_background",
                &format!(
                    "pane {shelld_sid}: uuid={} is held by an idle background session (pid {}); \
                     stopping it so the conversation can resume here under P{next_profile}",
                    meta.uuid, h.pid
                ),
            );
        }
        let Some(op) = profile_cycle_op(
            say_continue,
            &meta.uuid,
            meta.has_transcript,
            next_profile,
            meta.claude_pid,
            holder.map(|h| h.pid),
            shell_pid_for(shelld_sid),
            meta.project_dir.as_deref(),
            &choice,
        ) else {
            host.log(
                LogLevel::Warn,
                "cycle.no_command",
                &format!("cannot build a P{next_profile} resume line for {}", meta.uuid),
            );
            return false;
        };
        // The one fact this path never recorded: WHICH session it is
        // about to resume.  Everything else was logged — the pane, the
        // profiles, all five steps — so "did switching profile lose my
        // context?" could not be answered from the log at all, only
        // guessed at (2026-08-07).  A pane whose project holds six real
        // conversations, which torajs does, makes that the only
        // question worth asking.
        host.log(
            LogLevel::Info,
            "cycle.resuming",
            &format!(
                "pane {shelld_sid} → P{next_profile} {} uuid={} model={} effort={}",
                if meta.has_transcript { "resuming" } else { "starting fresh," },
                meta.uuid,
                choice.model.as_deref().unwrap_or("(profile default)"),
                choice.effort.as_deref().unwrap_or("(profile default)"),
            ),
        );
        if let Err(e) = host.submit_pty_op(shelld_sid, op) {
            host.log(
                LogLevel::Warn,
                "cycle.submit_failed",
                &format!("pane {shelld_sid}: {e}"),
            );
            return false;
        }
        true
    }

}


impl ClaudecodePlugin {
    /// Move this pane's conversation to codex profile `n` (RFC-009).
    fn hand_off(&mut self, host: &dyn PluginHost, sid: u64, n: u8) {
        use crate::plugins::handoff;
        let Some(meta) = self.last_meta.get(&sid).cloned() else {
            host.log(LogLevel::Warn, "handoff.no_bind", &format!("pane {sid}: no claude to hand off from"));
            return;
        };
        let Some((_, dir)) = handoff::profiles(handoff::Agent::Codex).into_iter().find(|(p, _)| *p == n) else {
            host.log(LogLevel::Warn, "handoff.profile_gone", &format!("codex profile {n} no longer exists"));
            return;
        };
        let leaving = handoff::Leaving {
            agent: handoff::Agent::Claude,
            pid: meta.claude_pid,
            shell_pid: shell_pid_for(sid),
            home: config_dir_or_default(meta.config_dir.as_deref()),
            session_id: Some(meta.uuid.clone()),
            cwd: None,
        };
        host.log(LogLevel::Info, "handoff.start", &format!("pane {sid} claude → codex P{n}: {leaving:?}"));
        let Some(op) = handoff::switch_op(sid, leaving, handoff::Agent::Codex, n, dir) else {
            host.log(LogLevel::Warn, "handoff.no_command", &format!("pane {sid}: cannot build the codex line"));
            return;
        };
        if let Err(e) = host.submit_pty_op(sid, op) {
            host.log(LogLevel::Warn, "handoff.submit_failed", &format!("pane {sid}: {e}"));
        }
    }
}


/// The claude session that would make a `--resume` of `uuid` fail, if
/// there is one.
///
/// Worth a stat before every cycle because of the order the cycle runs
/// in: the running claude is taken down *first*, and only then is the
/// resume line typed.  A resume that was never going to be accepted
/// therefore does not merely fail — the pane is left at a shell prompt
/// with the conversation no longer in it, and the way back is `claude
/// attach <id>` typed by hand.  The user saw a red paragraph and a
/// screenful of mouse reports instead (2026-09-01, uuid=1bcedee1).
///
/// The test mirrors the CLI's own, read out of `claude` 2.1.252: among
/// the live session records, a holder is one whose `sessionId` matches
/// and whose `kind` is anything other than `"interactive"` — which is
/// the case it reports as `Session <id> is running as a background
/// session`.
///
/// Records are one JSON file per pid under `<config-dir>/sessions/`,
/// named for the pid.  `config_dir` is the profile the *resuming*
/// claude will run under, because that is the directory it will read;
/// whether that sees another profile's sessions is the user's business
/// (mine symlinks them all to one shared dir, which is exactly why the
/// refusal crossed profiles at all).
///
/// Anything unreadable answers "no holder".  The gate is here to stop a
/// cycle already known to be futile, not to demand proof that one is
/// safe.
fn background_session_holder(config_dir: &std::path::Path, uuid: &str) -> Option<BackgroundHolder> {
    if uuid.is_empty() {
        return None;
    }
    let needle = format!("\"sessionId\":\"{uuid}\"");
    for e in fs::read_dir(config_dir.join("sessions")).ok()?.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Some(pid) = path
            .file_stem()
            .and_then(|x| x.to_str())
            .and_then(|x| x.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(body) = fs::read_to_string(&path) else {
            continue;
        };
        if !body.contains(&needle) {
            continue;
        }
        // A record with no `kind` is not a holder — the CLI skips
        // those too rather than guessing what they are.
        let Some(kind) = json_string_field(&body, "\"kind\":\"") else {
            continue;
        };
        if kind == "interactive" {
            continue;
        }
        // The directory keeps a file per pid that ever ran, so most of
        // what is in it is dead.  Only a live process holds anything.
        if unsafe { libc::kill(pid, 0) } != 0 {
            continue;
        }
        let idle = json_string_field(&body, "\"status\":\"").as_deref() == Some("idle");
        return Some(BackgroundHolder { pid, idle });
    }
    None
}

/// A live background claude holding a session a cycle wants to resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BackgroundHolder {
    pid: i32,
    /// The CLI's own `status` for it.  Idle means nothing is in flight:
    /// stopping it loses no work, only the process — the conversation is
    /// the transcript, and the cycle resumes that in the pane.
    idle: bool,
}

/// What the CLI says one live claude process is doing, from
/// `<config-dir>/sessions/<pid>.json`.
struct RecordedSession {
    session_id: String,
    kind: String,
}

/// The session a live claude is on, in its own words — `None` when the
/// process has no record (older CLI builds, or the record not written
/// yet) or the record does not parse.  A dead pid's record is stale by
/// definition and not consulted.
fn recorded_session(config_dir: &std::path::Path, pid: i32) -> Option<RecordedSession> {
    if unsafe { libc::kill(pid, 0) } != 0 {
        return None;
    }
    let body = fs::read_to_string(config_dir.join("sessions").join(format!("{pid}.json"))).ok()?;
    let session_id = json_string_field(&body, "\"sessionId\":\"")?;
    let kind = json_string_field(&body, "\"kind\":\"")?;
    (!session_id.is_empty()).then_some(RecordedSession { session_id, kind })
}

/// Every session a live non-interactive claude holds, per the records in
/// `config_dir` — the uuids no pane can be bound to.
fn background_held_sessions(config_dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(config_dir.join("sessions")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        let Some(pid) = path
            .file_stem()
            .and_then(|x| x.to_str())
            .and_then(|x| x.parse::<i32>().ok())
        else {
            continue;
        };
        if let Some(rec) = recorded_session(config_dir, pid)
            && rec.kind != "interactive" {
                out.push(rec.session_id);
            }
    }
    out
}

/// `CLAUDE_CONFIG_DIR` as a path, or the CLI's default when unset.
pub(crate) fn config_dir_or_default(config_dir: Option<&str>) -> PathBuf {
    match config_dir {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude"),
    }
}

/// Answer a badge click that cannot be honoured, without touching the
/// pane.
///
/// Nothing is typed and nothing is held: a click the cycle refuses must
/// cost the user no more than a click that did nothing.  The badge is
/// the only channel a pane has for saying so, and riding a (very short)
/// op is what makes it appear and then put itself away — the scan loop
/// owns the badge the rest of the time and would otherwise overwrite
/// this within the second.
fn cycle_blocked_op(badge: &'static str) -> pty_op::PtyOp {
    pty_op::PtyOp::new("cc.cycle_blocked")
        .lock_keys(false)
        .hold_screen(false)
        .escape_hatch(false)
        .badge(badge)
        .step(pty_op::Step::settle(Duration::from_secs(4)).named("show"))
}

/// Is the model a switch would carry the one the pane is on?
///
/// The scan's reading and the hook's record are two reads of the same
/// thing; a switch goes ahead only when both exist and agree. The id
/// carries a context suffix (`claude-fable-5-1[1m]`) that the short
/// name leaves out.
fn confirm_choice(pane_model: Option<&str>, choice: &CliChoice) -> Result<(), &'static str> {
    let Some(pane) = pane_model else {
        return Err("the pane's model is not known");
    };
    let Some(id) = choice.model.as_deref() else {
        return Err("the hook has not recorded the model to carry");
    };
    if short_model(id.split('[').next().unwrap_or(id)) != pane {
        return Err("the hook's record names a different model from the pane's");
    }
    Ok(())
}

/// The profile-cycle script: take the current claude down and bring the
/// same session back under the next profile.
///
/// SIGTERM straight to the pid rather than `/exit` through the PTY:
/// bare `exit` is ambiguous (claude treats it as a message and replies
/// politely without quitting), `/exit` works but prints `Bye!`, and both
/// echo into the grid.  A signal echoes nothing.
///
/// `claudeN` is an interactive alias in the user's rc file, so the
/// script sets the variable the alias would have set — same effect,
/// without depending on that file still defining it.
fn profile_cycle_op(
    say_continue: bool,
    uuid: &str,
    has_transcript: bool,
    next_profile: u8,
    claude_pid: i32,
    background_holder: Option<i32>,
    shell_pid: i32,
    // where the session lives, so resuming it does not move it
    project_dir: Option<&str>,
    // the model and effort it was running with
    choice: &CliChoice,
) -> Option<pty_op::PtyOp> {
    // No uuid, no cycle: without a name for the session there is no
    // way to tell "nothing to resume" from "a conversation we failed
    // to find", and resuming *nothing* in the second case would drop
    // what the user is looking at.
    if uuid.is_empty() {
        return None;
    }
    let home = std::env::var("HOME").ok()?;
    let mut cmd = pty_op::PtyCommand::new("claude")
        .env("CLAUDE_CONFIG_DIR", format!("{home}/.claude-profile-{next_profile}"))
        .clear_screen_first(true);
    // The claude this starts reports its own submits. This is the path
    // the badge takes, so it is the one that matters most and the one
    // that was missed when the other two were wired.
    // No submit receipt hook: nothing reads a receipt since the
    // sentence moved to the command line, and the hook forked this
    // binary on every prompt the person sent by hand.
    // A session with no transcript has taken no turn — there is
    // nothing to carry across, and `--resume` on a uuid claude never
    // wrote fails.  Start claude plain instead.
    //
    // This is the whole of the reported bug's second half: entering
    // cc and immediately clicking the badge did nothing at all, twice,
    // and went on doing nothing until the session had answered
    // something — 8.8 hours, on the pane this was found on.  The
    // window where a user is most likely to notice they are on the
    // wrong account is exactly the window where switching was refused.
    if has_transcript {
        cmd = cmd.arg("--resume").arg(uuid);
    }
    // Resumed where it lives, not where the pane's shell happens to
    // stand. `--resume` looks the id up in the project of the current
    // directory and binds what it reopens to that directory, so a
    // shell that has wandered takes the session with it: a torajs
    // session came back as `devops`, loading devops's rules, and the
    // only sign was the pane's title.
    cmd = cmd.in_dir(project_dir);
    // The model the session was on, not the new profile's default.
    // Quoted: an id like `claude-fable-5-1[1m]` is a glob to a shell.
    if let Some(model) = &choice.model {
        cmd = cmd.arg("--model").quoted_arg(model.clone());
    }
    if let Some(effort) = &choice.effort {
        cmd = cmd.arg("--effort").arg(effort.clone());
    }
    // The sentence goes in as claude's own argument, not as typing.
    //
    // It used to be pasted into the composer and followed by a
    // return, and that is the wrong path however well it works: the
    // composer is the person's buffer. Every failure leaves our text
    // sitting in it -- a machine at load 513 produced exactly that,
    // three times -- and even the successful path spends a moment
    // with marspot's words in the person's input box.
    //
    // `claude [options] [prompt]` takes it as a positional argument
    // and submits it itself. Measured 2026-10-01 through a real pty:
    // an interactive claude started this way answers the prompt
    // without the composer ever holding it, and it combines with
    // `--resume`.
    if say_continue {
        cmd = cmd.quoted_arg(resume_word());
    }
    let line = cmd.to_bytes()?;
    // What the person had typed and not sent. Read before claude is
    // taken down, handed back once the new one is reading input.
    // Switching accounts is bookkeeping; it is not a reason to throw
    // away someone's half-written sentence.
    let held = pty_op::Job::new();
    let mut op = pty_op::PtyOp::new("cc.profile_cycle")
        .hold_screen(true)
        .badge(format!("→ P{next_profile}"))
        // Whatever the person had typed travels; the sentence this
        // script says does not. It is on the command line now and
        // never reaches the composer -- but a copy left there by an
        // older build would otherwise be carried forward for ever.
        .step(pty_op::Step::capture_composer(Arc::clone(&held)).disowning(resume_word()))
        .step(pty_op::Step::settle(HOLD_SETTLE).named("hold_settle"))
        .step(
            pty_op::Step::terminate(claude_pid, libc::SIGTERM)
                .escalate_after(Duration::from_secs(3), libc::SIGKILL),
        );
    // The same way the pane's own claude is taken down — a signal, and
    // gone before the resume is typed, because `claude --resume` checks
    // for a live holder the moment it starts.  Its transcript is the
    // conversation; the resume below picks it up.
    if let Some(pid) = background_holder.filter(|&p| p != claude_pid) {
        op = op.step(
            pty_op::Step::terminate(pid, libc::SIGTERM)
                .escalate_after(Duration::from_secs(3), libc::SIGKILL)
                .named("stop_background"),
        );
    }
    Some(
        op.step(pty_op::Step::send(line).named("resume"))
            // Wait for the new claude to draw, exactly as the
            // reclamation does.  The hand-written version settled for a
            // flat 600 ms and unfroze onto whatever was there — fine for
            // a small session, a visible flash of shell for a big one (a
            // 317 MB transcript took 10 s+ to paint, measured
            // 2026-07-13).
            .step(
                pty_op::Step::await_process(shell_pid, looks_like_claudecode)
                    .timeout(Duration::from_secs(20)),
            )
            // Ask the terminal instead of counting its bytes.  Two
            // guesses came before this one: "quiet for a while" fired
            // on the startup trickle and the message went into a CLI
            // that was not reading input yet (2026-09-29), and "quiet
            // after 4096 bytes" was never reached by half the panes
            // (2026-09-30).  Alt screen plus bracketed paste is the
            // program itself saying it has the terminal.
            .step(pty_op::Step::await_tui_ready().or_late())
            // Then wait for it to actually paint, because the hold
            // lifts when this run ends and what is underneath is a
            // cleared screen.
            //
            // This used to be here by accident: the sentence was typed
            // in, and the submit step waits for the composer to redraw
            // with the text in it -- which is a first frame by another
            // name. Moving the sentence onto the command line took the
            // wait away with it, and the pane went black for as long as
            // the new claude took to draw (reported the same evening,
            // and the log shows `Done` nine milliseconds after
            // `await_tui_ready` gave up at 13 bytes).
            //
            // `await_tui_ready` cannot serve as the wait: it proceeds
            // on timeout by design, because a pane that will not answer
            // must not cost the resume that already happened.
            .step(
                pty_op::Step::await_quiet(idle::WAKE_QUIET_FOR)
                    .after_bytes(idle::FIRST_FRAME_BYTES)
                    .timeout(idle::WAKE_WATCHDOG)
                    .named("first_frame"),
            )
            // Their sentence last. If there was one, it goes back and
            // the run ends there -- a pane with someone mid-thought in
            // it does not also need to be told the quota came back.
            .step(pty_op::Step::restore_composer(held)),
    )
}

/// What to say to a session that has just been resumed on another
/// account.
///
/// Said to every pane the sweep moves, because no reading of the
/// transcript separates "finished" from "cut off" after the fact.
///
/// It carries both branches so that it can be said in either state:
/// carry on if something was interrupted, stay put if nothing was.
/// Tried by hand across every agent pane on 2026-09-30 — nothing was
/// woken that should have stayed asleep, and nothing that had been
/// cut off was missed.  Naming the idle case is what buys that;
/// without it the earlier wording still nudged an idle session into
/// finding something to continue.
fn resume_word() -> String {
    std::env::var("MARSPOT_CC_RESUME_WORD").unwrap_or_else(|_| {
        "额度恢复了，如果有刚被打断的工作请继续，如果是 idle 状态请保持".to_string()
    })
}

/// Scan `$HOME` for `.claude-profile-N` directories and return the
/// profile numbers, sorted ascending.  This is the source of truth
/// for the badge-click cycle — add a `~/.claude-profile-5` dir (plus
/// its `claude5` shell alias) and the cycle picks it up on the next
/// click, no code change.  The default `.claude` (P0) is deliberately
/// not part of the cycle, matching the previous hardcoded 1→2→3 loop.
pub(crate) fn discover_profiles() -> Vec<u8> {
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


/// Read `CLAUDE_CONFIG_DIR` off the running `claude` pid and parse a
/// short profile tag.  Returns:
///   * `Some("P1")` for `/Users/.../.claude-profile-1`
///   * `Some("P0")` for the default `/Users/.../.claude` (no -profile-N)
///   * `None`       when the env var isn't set / the read failed
///
/// The profile tag for a pane, from the config dir it was started
/// with.
///
/// Takes the directory rather than the pid because everything a pane
/// says about its profile — the tag, where its transcripts live, the
/// `--config-dir` a resume line has to quote — has to come from **one
/// read**.  Three separate reads is how the tag came to say `P3`
/// while the transcript walk looked under the default profile and
/// found nothing (2026-08-10).
fn profile_tag_from(config_dir: Option<&str>) -> Option<String> {
    let dir = config_dir?;
    // Trailing slash tolerant; basename only.
    let base = std::path::Path::new(dir).file_name()?.to_string_lossy().into_owned();
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

/// Where this pane keeps its transcripts, from the same one read as
/// [`profile_tag_from`].
fn projects_root_from(config_dir: Option<&str>) -> Option<PathBuf> {
    let dir = config_dir?.trim();
    if dir.is_empty() {
        return None;
    }
    Some(PathBuf::from(dir).join("projects"))
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
pub(crate) fn looks_like_claudecode(d: &pidtree::ProcRow) -> bool {
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
        // Anything an earlier process left dormant still has its
        // scrollback and its uuid; the next scan re-arms the wake path.
        self.load_dormant(host);
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
            statusline_state: None,
            seen: HashMap::new(),
            model_cutoff: HashMap::new(),
            banner_tried: HashMap::new(),
            last_refusal: HashMap::new(),
            last_model: HashMap::new(),
            last_model_by_pane: HashMap::new(),
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
        // Where a tick's time went, so an overshoot names its own
        // cause.  33 of them in one day, up to 1.6 s, with nothing
        // logged around them (2026-09-27) — this hook runs on the
        // thread that draws the window, and the budget is 100 ms.
        let t_start = std::time::Instant::now();
        let mut t_apply = 0u128;
        let mut t_looks = 0u128;
        let mut t_activity = 0u128;
        let mut t_idle = 0u128;
        let mut t_rearm = 0u128;
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
        let t_drain = t_start.elapsed().as_micros();
        if let Some(result) = latest {
            let t0 = std::time::Instant::now();
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
            t_apply = t0.elapsed().as_micros();
            let t0 = std::time::Instant::now();
            self.publish_looks(host, &result);
            t_looks = t0.elapsed().as_micros();
            let t0 = std::time::Instant::now();
            // Report this layer to the shell, which owns the state
            // machine.  The plugin does NOT compose its view with the
            // kernel's and does not decide what is actionable — it
            // only says what its own transcript shows.  Every live
            // session gets a report, including `Absent` for panes with
            // no claude in them: silence and "not here" are different
            // facts, and the machine distinguishes them.
            for (sid, activity) in &result.new_activity {
                let _ = host.report_pane_activity(*sid, *activity);
            }
            for sid in result.sessions_seen.iter() {
                if !result.new_activity.contains_key(sid) {
                    let _ = host.report_pane_activity(
                        *sid,
                        activity_for_unbound(*sid, &self.dormant),
                    );
                }
            }
            t_activity = t0.elapsed().as_micros();
            let t0 = std::time::Instant::now();
            // Idle reclamation runs off the same scan: the CPU samples
            // it compares were taken by the worker in the same pass
            // that produced these bindings.
            self.sweep_unusable_profiles(host, &result);
            self.run_idle_policy(host, &result);
            t_idle = t0.elapsed().as_micros();
            let t0 = std::time::Instant::now();
            // A pane that lost its claude while we had it marked
            // dormant, with no session of ours attached (an L1
            // self-execv drops every PaneSession), needs its wake path
            // put back — otherwise the next keystroke lands in the
            // shell and the session is only recoverable by hand.
            self.rearm_dormant(host, &result);
            t_rearm = t0.elapsed().as_micros();
            self.last_mapping = result.new_mapping;
            self.last_meta = result.new_meta;
            self.last_model = result.new_models;
        }

        // (2) Kick off the next scan if the worker is idle.  At most
        // one in-flight request: if the worker is still chewing on
        // the previous (slow disk), we skip — preserves the worker
        // queue from growing unbounded.
        if !self.scan_inflight
            && let Some(tx) = &self.scan_req_tx {
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

        // (3) RFC-003 profile-cycle state machine lives in
        // ProfileCyclePaneSession::on_tick, driven by the L1 plugin
        // dispatcher (not here).
        //
        // C7 auto-retry monitors: cheap host-side bookkeeping, no
        // disk IO.  `monitor_unsupported` latches true on first
        // tick (PTY raw broadcast not wired yet) so the body is a
        // no-op in steady state.
        let t0 = std::time::Instant::now();
        self.refresh_monitors(host);
        self.pump_monitors(host);
        let t_monitors = t0.elapsed().as_micros();

        let total = t_start.elapsed().as_micros();
        if total > 20_000 {
            host.log(
                LogLevel::Warn,
                "tick.slow",
                &format!(
                    "total={total} drain={t_drain} apply={t_apply} looks={t_looks} \
                     activity={t_activity} idle={t_idle} rearm={t_rearm} \
                     monitors={t_monitors} (µs)"
                ),
            );
        }
    }

    /// Where the user is.
    ///
    /// The only input this plugin has about the *person* rather than
    /// the session.  Everything else it weighs — transcript age, CPU,
    /// child processes — describes what claude is doing, and none of it
    /// can tell "half an hour since the last turn" apart from "half an
    /// hour since the last turn, and they are sitting right here
    /// reading it".
    fn on_pane_focused(&mut self, _host: &dyn PluginHost, shelld_session_id: u64) {
        self.focused_sid = Some(shelld_session_id);
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
        host: &dyn PluginHost,
        shelld_session_id: u64,
    ) -> Vec<marspot::shell_proto::PaneBadgeMenuItem> {
        // Both ways out of here used to be silent, and an empty menu
        // opens nothing — so a right-click on the badge that did
        // nothing left no trace anywhere to say which of the two it
        // was.  The badge is drawn from a look this plugin published
        // and the core holds until told otherwise; the menu is
        // computed live from `last_meta`.  The two can disagree, and
        // when they do the badge is on screen with nothing behind it.
        let Some(meta) = self.last_meta.get(&shelld_session_id) else {
            host.log(
                LogLevel::Warn,
                "badge_menu.no_bind",
                &format!(
                    "shelld_session={} has a badge but no binding; menu empty",
                    shelld_session_id
                ),
            );
            return Vec::new();
        };
        let profiles = discover_profiles();
        let mut items = badge_menu_for(meta.profile_num, &profiles);
        items.extend(crate::plugins::handoff::menu_rows(crate::plugins::handoff::Agent::Claude));
        if items.is_empty() {
            host.log(
                LogLevel::Warn,
                "badge_menu.empty",
                &format!(
                    "shelld_session={} current=P{} discovered={:?}; nothing to offer",
                    shelld_session_id, meta.profile_num, profiles
                ),
            );
        }
        items
    }

    fn on_pane_badge_menu_action(
        &mut self,
        host: &dyn PluginHost,
        shelld_session_id: u64,
        tag: u32,
    ) {
        if let Some((to, n)) = crate::plugins::handoff::parse_tag(tag) {
            if to == crate::plugins::handoff::Agent::Codex {
                self.hand_off(host, shelld_session_id, n);
            }
            return;
        }
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
                "shelld_session={} P{} → P{} uuid={}",
                shelld_session_id, meta.profile_num, target, meta.uuid
            ),
        );
        // A menu pick is someone who is right here.
        self.switch_by_hand(host, shelld_session_id, meta, target);
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
/// Alias for the shared alphabet: what claudecode reports about a
/// pane.  The variants live in `marspot::pane_state` because the shell
/// composes them with the kernel's view — the plugin's job ends at
/// "here is what my transcript says".
///
/// The classification below reads the session jsonl's last
/// conversation record.  Read off the session jsonl's last record, whose shapes
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
type CcActivity = marspot::pane_state::Activity;

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

/// Cheap "what's the type of the last record" check.  Reads at most
/// the last 32 KiB of the file and looks at the last `\n`-separated
/// record's `"type":"…"` field.  Returns None if the file is malformed
/// or has no recognisable type.
/// The tail window of a transcript as text, for the cheap
/// "what has this session been doing" checks.  Same 32 KB window the
/// classifier uses; a read failure yields an empty string, which the
/// callers read as "no evidence" — and every caller's no-evidence
/// answer is the conservative one.
fn tail_window(path: &PathBuf) -> String {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 32 * 1024;
    let Ok(mut f) = fs::File::open(path) else {
        return String::new();
    };
    let Ok(md) = f.metadata() else {
        return String::new();
    };
    let start = md.len().saturating_sub(TAIL);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::with_capacity(TAIL as usize);
    if f.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buf).into_owned()
}

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

/// When this session was last told by the API that it had reached a
/// limit, as unix seconds.
///
/// Reads the tail window the scan already has, so it costs nothing
/// beyond the scan of those lines.
///
/// Claude Code records the refusal itself rather than leaving it on
/// the screen: `"isApiErrorMessage":true` on an assistant record whose
/// text is the limit message, with the timestamp it happened. That is
/// what makes this evidence and not a screen-scrape -- and it is the
/// only timely evidence there is, because the usage feed is collected
/// behind what a session is actually spending.
///
/// Two texts are matched, both read out of the CLI's own templates
/// rather than guessed: the model-limit branch and the credits branch.
/// Deliberately not `rate_limit` or `429`, which a loaded server
/// produces too -- being overloaded is not being out of quota, and
/// moving a pane for it costs a restart for nothing.
pub(super) fn last_quota_refusal(tail: &str) -> Option<quota::Refusal> {
    let line = tail
        .lines()
        .rev()
        .find(|l| l.contains("\"isApiErrorMessage\":true") && says_out_of_quota(l))?;
    Some(quota::Refusal { at: record_timestamp(line)?, model: refused_model(line) })
}

/// The model a refusal names, or empty when it cannot be read.
///
/// The CLI's two templates put it in fixed places: "You've reached
/// your Fable limit." and "Opus 5.5 requires usage credits.".
fn refused_model(line: &str) -> String {
    const LIMIT: &str = "You've reached your ";
    if let Some(i) = line.find(LIMIT) {
        let rest = &line[i + LIMIT.len()..];
        return rest.find(" limit").map(|j| short_model(&rest[..j])).unwrap_or_default();
    }
    match line.find(" requires usage credits") {
        // the text field opens with the model's display name
        Some(j) => short_model(&line[line[..j].rfind('"').map_or(0, |q| q + 1)..j]),
        None => String::new(),
    }
}

/// A refusal, if it reached the claude that started at `started`.
fn refused_since(refusal: Option<quota::Refusal>, started: SystemTime) -> Option<quota::Refusal> {
    let start = started.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs() as i64;
    refusal.filter(|r| r.at >= start)
}

/// The two ways the CLI says "this account cannot serve you".
fn says_out_of_quota(line: &str) -> bool {
    (line.contains("You've reached your") && line.contains("limit"))
        || line.contains("requires usage credits")
}

/// The `timestamp` field of one transcript record.
fn record_timestamp(line: &str) -> Option<i64> {
    let key = "\"timestamp\":\"";
    let i = line.find(key)? + key.len();
    let rest = &line[i..];
    let end = rest.find('"')?;
    marspot::cc_usage::parse_iso_utc(rest[..end].to_string())
}

fn tail_last_message_type(path: &PathBuf) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 32 * 1024;
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The policy threshold every idle test runs at: the shipped
    /// default, stated here rather than read off the machine running
    /// the suite — see `run_idle_policy_at`.
    const TEST_THRESHOLD: Duration = Duration::from_secs(30 * 60);
    use std::io::Write;
    use std::path::Path;

    /// A `WorkerCtx` with nothing cached, for the model-half tests.
    fn ctx_for_test(projects_root: PathBuf) -> WorkerCtx {
        WorkerCtx {
            projects_root,
            shelld: Arc::new(ShelldClient::new(None)),
            statusline_state: None,
            seen: HashMap::new(),
            model_cutoff: HashMap::new(),
            banner_tried: HashMap::new(),
            last_refusal: HashMap::new(),
            last_model: HashMap::new(),
            last_model_by_pane: HashMap::new(),
        }
    }

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

    /// Wire a badge click against a session one background job holds,
    /// with that job's CLI `status`, and return the badge the click put
    /// up after ONE tick.
    ///
    /// One tick only, on purpose: the holder pid is this test process,
    /// and a cycle that got as far as `stop_background` would signal it.
    /// The first step of every cycle is a timed settle, so a single tick
    /// cannot reach a signal — what it can do is show which op the click
    /// queued, and that is the whole question.
    fn badge_after_click_on_bg_held_session(tag: &str, status: &str) -> String {
        let home = std::env::temp_dir()
            .join(format!("cc-cycle-gate-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        // Two profiles so the cycle has somewhere to go: P1 → P2.
        for n in [1u8, 2] {
            fs::create_dir_all(home.join(format!(".claude-profile-{n}/sessions")))
                .unwrap();
        }
        let uuid = "e1e62fd9-d0bc-493a-865a-56f2de8d7ac3";
        let me = std::process::id() as i32;
        // The shape claude actually writes, taken off a real `claude
        // --bg` run: `kind` is **"bg"**, not "background".  The test
        // being `!= "interactive"` rather than a list of known kinds
        // is what makes that not matter — and this record is here so
        // it keeps not mattering.
        fs::write(
            home.join(format!(".claude-profile-2/sessions/{me}.json")),
            format!(
                r#"{{"pid":{me},"sessionId":"{uuid}","cwd":"/tmp","kind":"bg","status":"{status}"}}"#
            ),
        )
        .unwrap();
        let _home = HomeOverride::set(&home);

        let host = FakeHost::new(home.join("state"));
        let mut plugin = ClaudecodePlugin::new();
        plugin.shelld = Some(Arc::new(ShelldClient::new(None)));
        let meta = BindMeta {
            profile_num: 1,
            config_dir: Some(
                home.join(".claude-profile-1").to_string_lossy().into_owned(),
            ),
            uuid: uuid.to_string(),
            // Never signalled within one tick; a pid that cannot exist
            // keeps a regression from killing anything real regardless.
            claude_pid: i32::MAX,
            project_basename: String::new(),
            project_dir: None,
            refused_at: None,
            transcript_at: SystemTime::now(),
            has_transcript: true,
        };

        plugin.start_profile_cycle_to(&host, 7, meta, 2, false, CliChoice::default());
        host.pump_ops();

        // The badge rides the PaneSession, so it takes a tick to
        // appear — the real host drives these from its own loop.
        struct Recording(std::sync::Mutex<Vec<String>>);
        impl crate::plugins::PaneSessionHost for Recording {
            fn shelld_session_id(&self) -> u64 {
                7
            }
            fn end(&self) {}
            fn set_badge(&self, text: &str) {
                self.0.lock().unwrap().push(text.to_string());
            }
            fn set_pane_title(&self, _t: &str) {}
            fn log(&self, _l: LogLevel, _t: &str, _m: &str) {}
        }
        let rec = Recording(std::sync::Mutex::new(Vec::new()));
        for sess in host.sessions.lock().unwrap().iter_mut() {
            sess.on_tick(&rec);
        }
        let badges = rec.0.lock().unwrap().clone();
        fs::remove_dir_all(&home).ok();
        assert_eq!(badges.len(), 1, "one badge, for the one click: {badges:?}");
        badges[0].clone()
    }

    /// A busy background job keeps its session: the click is answered
    /// by the refusal, because stopping the job would cut off its work.
    #[test]
    fn a_click_on_a_busy_bg_held_session_refuses_instead_of_cycling() {
        let badge = badge_after_click_on_bg_held_session("busy", "busy");
        // A running step gets a spinner frame appended — kept, because
        // it is what says the click was received at all.
        assert!(
            badge.starts_with("⚠ held by bg job"),
            "a busy job must not be stopped by a badge click; `→ P2` here would \
             mean the cycle ran.  got {badge:?}"
        );
    }

    /// An idle background job no longer blocks the switch: the cycle runs
    /// (and stops the job before resuming — pinned in
    /// `a_cycle_through_a_background_hold_stops_it_before_resuming`).
    /// Asked for on 2026-09-17: "尽可能 switch 完成以后要能恢复".
    #[test]
    fn a_click_on_an_idle_bg_held_session_cycles() {
        let badge = badge_after_click_on_bg_held_session("idle", "idle");
        assert!(
            badge.starts_with("→ P2"),
            "an idle job's session should switch and come back in the pane; got {badge:?}"
        );
    }

    /// `HOME` is process-global.  nextest is process-per-test and
    /// immune, but `cargo test` runs these as threads alongside a test
    /// that reads the real `HOME`, so serialise and put it back.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A temp `HOME` with two profiles: P1 on an account that is out,
    /// P2 on one with room.  One right answer, so a test never has to
    /// argue about which way the pick went.
    fn quota_home(tag: &str) -> std::path::PathBuf {
        let home = std::env::temp_dir().join(format!("marspot-quota-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        for n in [1u8, 2] {
            fs::create_dir_all(home.join(format!(".claude-profile-{n}/sessions"))).unwrap();
            fs::write(
                home.join(format!(".claude-profile-{n}/.claude.json")),
                format!("{{\n  \"oauthAccount\": {{\n    \"emailAddress\": \"{tag}{n}@example\"\n  }}\n}}"),
            )
            .unwrap();
        }
        fs::create_dir_all(home.join(".local/state/devops")).unwrap();
        fs::write(
            home.join(".local/state/devops/claude-usage.json"),
            format!(
                r#"{{"generated_at": "2026-09-29T00:00:00+00:00", "accounts": [
                   {{"name":"A","email":"{tag}1@example","status":"rejected","utilization_5h":0.0,
                    "utilization_7d":1.0,"reset_5h":1,"reset_7d":2}},
                   {{"name":"B","email":"{tag}2@example","status":"allowed","utilization_5h":0.0,
                    "utilization_7d":0.2,"reset_5h":1,"reset_7d":3}}]}}"#
            ),
        )
        .unwrap();
        // The status-line hook's records, which a switch starts the new
        // claude from: every pane in these tests is on Opus 5.5.
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", home.join("state")) };
        let push = model_push_dir();
        fs::create_dir_all(&push).unwrap();
        for n in 77..=81 {
            fs::write(
                push.join(format!("u-{n}")),
                "opus-5-5\n/t/x.jsonl\nmedium\npids=1\nid=claude-opus-5-5\n",
            )
            .unwrap();
        }
        home
    }

    /// A pane bound to P1 of that `HOME`, with the session name it
    /// would have to resume.
    fn pane_on_p1(home: &std::path::Path, uuid: &str) -> BindMeta {
        BindMeta {
            profile_num: 1,
            config_dir: Some(home.join(".claude-profile-1").to_string_lossy().into_owned()),
            uuid: uuid.into(),
            claude_pid: i32::MAX,
            has_transcript: true,
            transcript_at: SystemTime::UNIX_EPOCH,
            project_basename: String::new(),
            project_dir: None,
            refused_at: None,
        }
    }

    fn scan_of(sid: u64, activity: CcActivity) -> ScanResult {
        let mut result = ScanResult {
            new_mapping: HashMap::new(),
            new_models: HashMap::new(),
            new_meta: HashMap::new(),
            new_activity: HashMap::new(),
            new_cpu: HashMap::new(),
            new_vetoes: HashMap::new(),
            scanned_at: SystemTime::now(),
            sessions_seen: vec![sid],
            log_lines: Vec::new(),
        };
        result.new_activity.insert(sid, activity);
        result.new_vetoes.insert(sid, (false, false));
        result.new_models.insert(sid, "opus-5-5".into());
        result
    }

    /// An automatic switch does not go ahead on a guess about the pane:
    /// its model has to be known, and the hook's record of what to
    /// start the new claude on has to name the same one.
    #[test]
    fn a_pane_is_not_moved_until_its_model_is_confirmed() {
        let home = quota_home("confirm");
        let _home = HomeOverride::set(&home);
        let host = FakeHost::new(home.join("state"));
        let sid = 81u64;
        let mut plugin = stranded_plugin(pane_on_p1(&home, "u-81"), sid);

        let mut unknown = scan_of(sid, CcActivity::AwaitingUser);
        unknown.new_models.clear();
        plugin.sweep_unusable_profiles(&host, &unknown);
        assert!(host.submitted.lock().unwrap().is_empty(), "model not known");

        let mut other = scan_of(sid, CcActivity::AwaitingUser);
        other.new_models.insert(sid, "fable-5-1".into());
        plugin.sweep_unusable_profiles(&host, &other);
        assert!(host.submitted.lock().unwrap().is_empty(), "the record names another model");

        fs::remove_file(model_push_dir().join("u-81")).unwrap();
        plugin.sweep_unusable_profiles(&host, &scan_of(sid, CcActivity::AwaitingUser));
        assert!(host.submitted.lock().unwrap().is_empty(), "no record to start the new claude from");

        fs::write(
            model_push_dir().join("u-81"),
            "opus-5-5\n/t/x.jsonl\nmedium\npids=1\nid=claude-opus-5-5\n",
        )
        .unwrap();
        plugin.sweep_unusable_profiles(&host, &scan_of(sid, CcActivity::AwaitingUser));
        let handed = host.submitted.lock().unwrap().clone();
        assert_eq!(handed.len(), 1, "both reads agree: it moves");
        let sent = host.sent.lock().unwrap().clone();
        assert!(sent[0].1.contains("--model 'claude-opus-5-5'"), "{:?}", sent[0].1);
        let _ = fs::remove_dir_all(&home);
    }

    fn stranded_plugin(meta: BindMeta, sid: u64) -> ClaudecodePlugin {
        let mut plugin = ClaudecodePlugin::new();
        plugin.shelld = Some(Arc::new(ShelldClient::new(None)));
        plugin.last_meta.insert(sid, meta);
        plugin
    }

    /// The wiring, end to end inside the process: a pane sitting on an
    /// account that is out gets moved to one that is not.
    ///
    /// Nothing here reads a screen or greps a transcript for the words
    /// a refusal prints.  The account says it cannot answer, the pane
    /// is on that account, and that is the whole trigger.
    #[test]
    fn a_pane_on_a_spent_account_is_handed_to_the_host_as_a_switch() {
        let home = quota_home("wire");
        let _home = HomeOverride::set(&home);
        let host = FakeHost::new(home.join("state"));
        let sid = 77u64;
        let mut plugin = stranded_plugin(pane_on_p1(&home, "u-77"), sid);
        // Waiting for its person as far as the transcript can tell —
        // which is also what a turn the account just killed looks
        // like, so it is spoken to all the same.
        let result = scan_of(sid, CcActivity::AwaitingUser);
        plugin.sweep_unusable_profiles(&host, &result);

        let handed = host.submitted.lock().unwrap().clone();
        assert_eq!(handed.len(), 1, "one switch for one stranded pane: {handed:?}");
        assert_eq!(handed[0].0, sid);
        assert!(handed[0].1.contains(&"resume"), "{:?}", handed[0].1);
        let sent = host.sent.lock().unwrap().clone();
        assert!(
            sent[0].1.contains(&resume_word()),
            "every moved pane is asked to carry on — the reading of \"idle\" was \
             wrong for three of six panes on 2026-09-30 — and it is asked on \
             claude's own command line, never through the person's composer: \
             {:?}",
            sent[0].1
        );

        // The next pass comes round before the resume has finished, so
        // the pane still reads as being on P1.  It is not moved a
        // second time on top of the first.
        plugin.sweep_unusable_profiles(&host, &result);
        assert_eq!(host.submitted.lock().unwrap().len(), 1, "one move per stranding");

        // And not even when the pane is briefly unbound in between —
        // which is what the cycle itself causes, and where the tally
        // used to be thrown away (pane 442, 2026-09-29).
        plugin.last_meta.remove(&sid);
        plugin.sweep_unusable_profiles(&host, &result);
        plugin.last_meta.insert(sid, pane_on_p1(&home, "u-77"));
        plugin.sweep_unusable_profiles(&host, &result);
        assert_eq!(
            host.submitted.lock().unwrap().len(),
            1,
            "a pane between bindings is still a pane that was just moved"
        );
        let _ = fs::remove_dir_all(&home);
    }

    /// A turn visibly in flight gets the same treatment — the point of
    /// this one is that nothing about the wiring depends on which of
    /// the two readings the transcript gave.
    #[test]
    fn a_pane_that_was_working_is_told_to_carry_on() {
        let home = quota_home("say");
        let _home = HomeOverride::set(&home);
        let host = FakeHost::new(home.join("state"));
        let sid = 78u64;
        let mut plugin = stranded_plugin(pane_on_p1(&home, "u-78"), sid);
        // A turn was being produced when the account stopped answering.
        let result = scan_of(sid, CcActivity::Working);
        plugin.sweep_unusable_profiles(&host, &result);

        let handed = host.submitted.lock().unwrap().clone();
        assert_eq!(handed.len(), 1);
        let sent = host.sent.lock().unwrap().clone();
        assert!(
            sent[0].1.contains(&resume_word()),
            "a pane that was working is told to carry on: {:?}",
            sent[0].1
        );
        let _ = fs::remove_dir_all(&home);
    }

    /// A pane put somewhere by hand is judged like any other: by its
    /// model against what that account has left for it. And the switch
    /// itself waits until the pane's model is confirmed.
    #[test]
    fn a_switch_by_hand_follows_the_same_rule() {
        let home = quota_home("hand");
        let _home = HomeOverride::set(&home);
        let host = FakeHost::new(home.join("state"));
        let sid = 80u64;
        let on = |profile: u8, pid: i32| {
            let mut m = pane_on_p1(&home, "u-80");
            m.profile_num = profile;
            m.config_dir =
                Some(home.join(format!(".claude-profile-{profile}")).to_string_lossy().into_owned());
            m.claude_pid = pid;
            m
        };
        let mut plugin = stranded_plugin(on(2, 100), sid);
        let result = scan_of(sid, CcActivity::AwaitingUser);
        let switches = |host: &FakeHost| {
            host.submitted.lock().unwrap().iter().filter(|(_, steps)| steps.contains(&"resume")).count()
        };

        // Model not read yet: the pick is refused, nothing is started.
        plugin.on_pane_badge_menu_action(&host, sid, 1);
        assert_eq!(switches(&host), 0, "the model is not known yet");
        assert_eq!(host.submitted.lock().unwrap().len(), 1, "the badge says why");

        plugin.last_model.insert(sid, "opus-5-5".into());
        plugin.on_pane_badge_menu_action(&host, sid, 1);
        assert_eq!(switches(&host), 1, "the pick itself");
        assert!(
            host.sent.lock().unwrap().iter().any(|(_, l)| l.contains("--model 'claude-opus-5-5'")),
            "started on the model it was on"
        );

        // Landed on P1, which has no room for it: it moves on, to P2.
        plugin.last_meta.insert(sid, on(1, 200));
        plugin.sweep_unusable_profiles(&host, &result);
        assert_eq!(switches(&host), 2, "unusable is unusable, however it got there");

        // On P2, which has room, nothing happens however often it looks.
        plugin.last_meta.insert(sid, on(2, 300));
        plugin.sweep_unusable_profiles(&host, &result);
        plugin.sweep_unusable_profiles(&host, &result);
        assert_eq!(switches(&host), 2, "usable: left alone");
        let _ = fs::remove_dir_all(&home);
    }

    /// A pane whose session has no name yet cannot be moved — there is
    /// nothing to resume on the other side, and starting fresh would
    /// drop what is on the screen.  The attempt must not be recorded
    /// as a move: the pane never went anywhere, and spending the
    /// budget on it leaves the real move with nothing to spend.
    #[test]
    fn a_pane_with_no_session_to_resume_is_not_counted_as_moved() {
        let home = quota_home("nouuid");
        let _home = HomeOverride::set(&home);
        let host = FakeHost::new(home.join("state"));
        let sid = 79u64;
        let mut plugin = stranded_plugin(pane_on_p1(&home, ""), sid);
        let result = scan_of(sid, CcActivity::Working);
        plugin.sweep_unusable_profiles(&host, &result);
        assert!(host.submitted.lock().unwrap().is_empty(), "nothing to hand over");
        assert_eq!(plugin.stranded.get(&sid).map(|s| s.moves), Some(0), "no move was spent");

        // Once the scan learns the session's name, the move happens.
        plugin.last_meta.insert(sid, pane_on_p1(&home, "u-79"));
        plugin.sweep_unusable_profiles(&host, &result);
        assert_eq!(host.submitted.lock().unwrap().len(), 1);
        let _ = fs::remove_dir_all(&home);
    }

    struct HomeOverride {
        old: Option<std::ffi::OsString>,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl HomeOverride {
        fn set(dir: &std::path::Path) -> Self {
            let guard = HOME_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let old = std::env::var_os("HOME");
            // SAFETY: no other thread reads HOME while the lock is held.
            unsafe { std::env::set_var("HOME", dir) };
            Self { old, _guard: guard }
        }
    }

    impl Drop for HomeOverride {
        fn drop(&mut self) {
            // SAFETY: as above — still under the lock.
            unsafe {
                match self.old.take() {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }

    /// And the other half: with nothing holding the session, the same
    /// click still cycles.
    ///
    /// A gate that is only ever tested when it fires is a gate that can
    /// quietly refuse everything.  Same setup as above with the record
    /// changed to `interactive`, which is what a pane running claude in
    /// the foreground writes about itself — the ordinary case, and the
    /// one the feature exists for.
    #[test]
    fn a_click_on_an_unheld_session_still_cycles() {
        let home = std::env::temp_dir()
            .join(format!("cc-cycle-open-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        for n in [1u8, 2] {
            fs::create_dir_all(home.join(format!(".claude-profile-{n}/sessions")))
                .unwrap();
        }
        let uuid = "e1e62fd9-d0bc-493a-865a-56f2de8d7ac3";
        let me = std::process::id() as i32;
        fs::write(
            home.join(format!(".claude-profile-2/sessions/{me}.json")),
            format!(
                r#"{{"pid":{me},"sessionId":"{uuid}","cwd":"/tmp","kind":"interactive","status":"idle"}}"#
            ),
        )
        .unwrap();
        let _home = HomeOverride::set(&home);

        let host = FakeHost::new(home.join("state"));
        let mut plugin = ClaudecodePlugin::new();
        plugin.shelld = Some(Arc::new(ShelldClient::new(None)));
        let meta = BindMeta {
            profile_num: 1,
            config_dir: Some(
                home.join(".claude-profile-1").to_string_lossy().into_owned(),
            ),
            uuid: uuid.to_string(),
            // The cycle's first step is a settle, so nothing is
            // signalled during this test; a pid that cannot exist
            // keeps it that way even if that changes.
            claude_pid: i32::MAX,
            project_basename: String::new(),
            project_dir: None,
            refused_at: None,
            transcript_at: SystemTime::now(),
            has_transcript: true,
        };

        plugin.start_profile_cycle_to(&host, 7, meta, 2, false, CliChoice::default());
        host.pump_ops();

        struct Recording(std::sync::Mutex<Vec<String>>);
        impl crate::plugins::PaneSessionHost for Recording {
            fn shelld_session_id(&self) -> u64 {
                7
            }
            fn end(&self) {}
            fn set_badge(&self, text: &str) {
                self.0.lock().unwrap().push(text.to_string());
            }
            fn set_pane_title(&self, _t: &str) {}
            fn log(&self, _l: LogLevel, _t: &str, _m: &str) {}
        }
        let rec = Recording(std::sync::Mutex::new(Vec::new()));
        for sess in host.sessions.lock().unwrap().iter_mut() {
            sess.on_tick(&rec);
        }

        let badges = rec.0.lock().unwrap();
        assert_eq!(badges.len(), 1);
        assert!(
            badges[0].starts_with("→ P2"),
            "an unheld session must still cycle. got {:?}",
            badges[0]
        );
        drop(badges);
        fs::remove_dir_all(&home).ok();
    }

    /// A `<config-dir>/sessions/` holding one record per `(pid, kind)`,
    /// all naming `uuid`.  Records are written the way claude writes
    /// them: one JSON object per file, named for the pid.
    fn tmp_config_with_sessions(uuid: &str, records: &[(i32, &str)]) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "cc-bg-session-test-{}-{}",
            std::process::id(),
            n
        ));
        let sessions = dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        for (pid, kind) in records {
            let body = format!(
                r#"{{"pid":{pid},"sessionId":"{uuid}","cwd":"/tmp","kind":"{kind}","status":"idle"}}"#
            );
            fs::write(sessions.join(format!("{pid}.json")), body).unwrap();
        }
        dir
    }

    /// The case the gate exists for: a live record for this uuid whose
    /// kind is not `interactive`.  `claude --resume` refuses this one,
    /// so the cycle must not take the running claude down for it.
    #[test]
    fn background_holder_found_for_live_non_interactive_record() {
        let uuid = "1bcedee1-2228-4e82-a9c2-632aac03bd2d";
        let me = std::process::id() as i32;
        let dir = tmp_config_with_sessions(uuid, &[(me, "background")]);
        assert_eq!(
            background_session_holder(&dir, uuid),
            Some(BackgroundHolder { pid: me, idle: true })
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// A busy background job is a holder too, but a different one: the
    /// cycle stops an idle job and refuses a busy one, so the status has
    /// to come through.
    #[test]
    fn a_busy_background_holder_says_so() {
        let uuid = "fb55e303-d796-4e20-80cf-d4be31c91a7a";
        let me = std::process::id() as i32;
        let dir = tmp_config_with_sessions(uuid, &[(me, "bg")]);
        let f = dir.join("sessions").join(format!("{me}.json"));
        let body = fs::read_to_string(&f).unwrap().replace("\"idle\"", "\"busy\"");
        fs::write(&f, body).unwrap();
        assert_eq!(
            background_session_holder(&dir, uuid),
            Some(BackgroundHolder { pid: me, idle: false })
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// The CLI's record for a pid is the session that process is on now,
    /// and it is what a pane binds to.  Only a live pid's record counts.
    #[test]
    fn a_live_claudes_record_names_its_session() {
        let uuid = "bcdc11f1-d813-4337-b30b-30a637e43238";
        let me = std::process::id() as i32;
        let dir = tmp_config_with_sessions(uuid, &[(me, "interactive")]);
        let rec = recorded_session(&dir, me).expect("own record");
        assert_eq!(rec.session_id, uuid);
        assert_eq!(rec.kind, "interactive");
        // No file for this pid (and it is not alive either).
        assert!(recorded_session(&dir, i32::MAX).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    /// Sessions background jobs hold are excluded from the binding
    /// guesses; interactive ones are not.  2026-09-17: panes 390 and 391
    /// were bound to sessions two background jobs owned, and every
    /// profile switch was refused.
    #[test]
    fn only_background_jobs_sessions_are_held() {
        let me = std::process::id() as i32;
        let dir = tmp_config_with_sessions("52e33087-2410-4f10-91f1-6d3182120ed6", &[(me, "bg")]);
        assert_eq!(
            background_held_sessions(&dir),
            vec!["52e33087-2410-4f10-91f1-6d3182120ed6".to_string()]
        );
        fs::remove_dir_all(&dir).ok();
        let dir = tmp_config_with_sessions("bcdc11f1-d813-4337-b30b-30a637e43238", &[(me, "interactive")]);
        assert!(background_held_sessions(&dir).is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    /// With an idle background job holding the session, the cycle stops
    /// that job after the pane's claude and before the resume is typed —
    /// `claude --resume` checks for a live holder the moment it starts.
    #[test]
    fn a_cycle_through_a_background_hold_stops_it_before_resuming() {
        let op = profile_cycle_op(true, "u-1", true, 4, 4242, Some(5151), 4200, None, &CliChoice::default()).expect("cycle");
        let labels: Vec<&str> = op.steps.iter().map(|s| s.label).collect();
        let stop = labels.iter().position(|l| *l == "stop_background").expect("stop step present");
        let resume = labels.iter().position(|l| *l == "resume").expect("resume step present");
        assert!(stop < resume, "{labels:?}");
        assert!(matches!(
            op.steps[stop].kind,
            pty_op::StepKind::Terminate { pid: 5151, .. }
        ));
        // No holder: no extra step.
        let op = profile_cycle_op(true, "u-1", true, 4, 4242, None, 4200, None, &CliChoice::default()).unwrap();
        assert!(!op.steps.iter().any(|s| s.label == "stop_background"));
        // A holder that is the pane's own claude is not stopped twice.
        let op = profile_cycle_op(true, "u-1", true, 4, 4242, Some(4242), 4200, None, &CliChoice::default()).unwrap();
        assert!(!op.steps.iter().any(|s| s.label == "stop_background"));
    }

    /// The switch says the thing the person used to have to type, and
    /// says it without touching the composer.
    ///
    /// `--resume` reopens the session and waits — the refused turn is
    /// not retried by itself, which is why every switch so far has
    /// ended with someone typing "继续" by hand.  It used to be typed
    /// by us instead: pasted into the composer, then a return.  That
    /// path is wrong however well it works, because the composer is
    /// the person's buffer and every failure leaves our sentence
    /// sitting in it unsent — seen three times on 2026-10-01.
    ///
    /// `claude [options] [prompt]` takes the sentence as an argument
    /// and submits it itself, so it rides in on the one line that
    /// starts the new process.  What this test pins is that there is
    /// no second path: nothing in the run types the sentence.
    #[test]
    fn a_cycle_says_continue_on_the_command_line() {
        let op = profile_cycle_op(true, "u-1", true, 4, 4242, None, 4200, None, &CliChoice::default()).expect("cycle");
        let labels: Vec<&str> = op.steps.iter().map(|s| s.label).collect();
        let resume = labels.iter().position(|l| *l == "resume").expect("resume");
        let line = match &op.steps[resume].kind {
            pty_op::StepKind::Send(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            _ => panic!("the resume step writes a command line, not {}", op.steps[resume].label),
        };
        assert!(
            line.contains(&resume_word()),
            "the sentence is an argument of the command that starts claude: {line}"
        );
        // A flag and its value are adjacent or they are not a flag and
        // its value. This is what keeping quoted arguments in a list of
        // their own once broke -- the quoted ones were appended last,
        // which put the sentence between `--resume` and the id it
        // names. There is one bare flag and one quoted positional on
        // this line, so the line itself is the case.
        assert!(
            line.contains("--resume u-1"),
            "the id follows its flag: {line}"
        );
        assert!(
            line.find("--resume").unwrap() < line.find(&resume_word()).unwrap(),
            "and the sentence comes after the pair, not inside it: {line}"
        );
        assert!(
            !op.steps
                .iter()
                .any(|s| matches!(&s.kind, pty_op::StepKind::Submit { .. })),
            "nothing in the run submits anything into the pane: {labels:?}"
        );
        assert!(
            !labels.contains(&"enter"),
            "and nothing presses return in it either: {labels:?}"
        );
    }

    /// A switch brings the session back on the model it was on.
    ///
    /// Without the flag the new claude takes the new profile's default:
    /// on 2026-10-03 a pane switched to Opus came back on Fable, on an
    /// account whose Fable was spent.
    #[test]
    fn a_cycle_keeps_the_model_and_effort_it_was_on() {
        let choice = CliChoice {
            model: Some("claude-fable-5-1[1m]".into()),
            effort: Some("medium".into()),
        };
        let op = profile_cycle_op(true, "u-1", true, 4, 4242, None, 4200, None, &choice).expect("cycle");
        let line = op
            .steps
            .iter()
            .find_map(|s| match &s.kind {
                pty_op::StepKind::Send(bytes) if s.label == "resume" => {
                    Some(String::from_utf8_lossy(bytes).into_owned())
                }
                _ => None,
            })
            .expect("a resume line");
        assert!(line.contains("--model 'claude-fable-5-1[1m]'"), "quoted, or the shell globs it: {line}");
        assert!(line.contains("--effort medium"), "{line}");

        let plain = profile_cycle_op(true, "u-1", true, 4, 4242, None, 4200, None, &CliChoice::default())
            .expect("cycle");
        let sent: String = plain
            .steps
            .iter()
            .filter_map(|s| match &s.kind {
                pty_op::StepKind::Send(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
                _ => None,
            })
            .collect();
        assert!(sent.contains("--resume"), "the probe reads the resume line: {sent}");
        assert!(!sent.contains("--model"), "nothing known, nothing passed: {sent}");
    }

    /// What an automatic switch needs to know about the pane first.
    #[test]
    fn a_switch_needs_the_panes_model_confirmed_twice() {
        let carry = |id: &str| CliChoice { model: Some(id.into()), effort: None };
        assert_eq!(confirm_choice(Some("fable-5-1"), &carry("claude-fable-5-1[1m]")), Ok(()));
        assert_eq!(confirm_choice(Some("opus-5-5"), &carry("claude-opus-5-5")), Ok(()));
        assert!(confirm_choice(None, &carry("claude-opus-5-5")).is_err(), "model not known");
        assert!(
            confirm_choice(Some("opus-5-5"), &CliChoice::default()).is_err(),
            "nothing to start the new claude on"
        );
        assert!(
            confirm_choice(Some("opus-5-5"), &carry("claude-fable-5-1[1m]")).is_err(),
            "the two reads disagree"
        );
    }

    /// The hook's record is a file: what goes on a command line from it
    /// is checked first.
    #[test]
    fn the_hooks_record_names_the_model_as_claude_takes_it() {
        let root = std::env::temp_dir().join(format!("cc-choice-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", &root) };
        let push = model_push_dir();
        fs::create_dir_all(&push).unwrap();
        let write = |uuid: &str, body: &str| fs::write(push.join(uuid), body).unwrap();

        write("u-new", "opus-5-5\n/t/u-new.jsonl\nmedium\npids=1,2\nid=claude-opus-5-5\n");
        assert_eq!(
            pushed_choice("u-new"),
            CliChoice { model: Some("claude-opus-5-5".into()), effort: Some("medium".into()) }
        );
        write("u-old", "opus-5-5\n/t/u-old.jsonl\nhigh\npids=1,2\n");
        assert_eq!(
            pushed_choice("u-old"),
            CliChoice { model: None, effort: Some("high".into()) },
            "a record from before the id line passes no model"
        );
        write("u-bad", "opus-5-5\n/t/u-bad.jsonl\nloud\npids=1,2\nid=opus; rm -rf ~\n");
        assert_eq!(pushed_choice("u-bad"), CliChoice::default(), "neither value is one claude takes");
        assert_eq!(pushed_choice("../u-new"), CliChoice::default(), "a uuid is not a path");
        let _ = fs::remove_dir_all(&root);
    }

    /// Starting a claude does not install a hook that runs on the
    /// person's keystrokes.
    ///
    /// Every pane marspot started carried a `UserPromptSubmit` hook so
    /// that text typed into it could be confirmed rather than read off
    /// the screen. Nothing reads a receipt any more -- the sentence
    /// moved to the command line and no production path submits -- and
    /// the hook was a fork and exec of marspot-shell on every prompt
    /// the person sent by hand: median 7.6 ms, up to 25.9 ms, measured.
    #[test]
    fn a_resumed_pane_gets_no_hook_on_the_persons_keystrokes() {
        let op = profile_cycle_op(true, "u-1", true, 4, 4242, None, 4200, None, &CliChoice::default()).expect("cycle");
        let line = match &op.steps.iter().find(|s| s.label == "resume").unwrap().kind {
            pty_op::StepKind::Send(b) => String::from_utf8_lossy(b).into_owned(),
            _ => unreachable!(),
        };
        assert!(
            !line.contains("UserPromptSubmit"),
            "no hook rides in on the line that starts claude: {line}"
        );
        assert!(
            !line.contains("--settings"),
            "and nothing is added to the person's own settings: {line}"
        );
    }

    /// A switch must not move the session it resumes.
    ///
    /// `claude --resume` resolves the id against the project of the
    /// directory it runs in, and binds what it reopens to that
    /// directory. The pane's shell is not reliably standing where the
    /// session lives, and when it is not, the switch relocates the
    /// conversation: a torajs session came back as `devops` and loaded
    /// devops's rules, with nothing to show for it but a changed pane
    /// title.
    #[test]
    fn a_cycle_resumes_the_session_where_it_lives() {
        let op = profile_cycle_op(
            true,
            "u-1",
            true,
            4,
            4242,
            None,
            4200,
            Some("/Users/x/workspace/goliajp/torajs"),
            &CliChoice::default(),
        )
        .expect("cycle");
        let resume = op
            .steps
            .iter()
            .find(|s| s.label == "resume")
            .expect("resume step");
        let line = match &resume.kind {
            pty_op::StepKind::Send(b) => String::from_utf8_lossy(b).into_owned(),
            _ => panic!("the resume step writes a command line"),
        };
        assert!(
            line.contains("cd '/Users/x/workspace/goliajp/torajs' && "),
            "it goes to the session's own directory first: {line}"
        );
        // Order matters and is not cosmetic: `exec FOO=bar cmd` runs a
        // program named `FOO=bar`, which is how every switch came to
        // exit 127 and then wait twenty seconds for a claude that had
        // never been started.
        let exec_at = line.find("exec ").expect("the subshell execs the program");
        let cd_at = line.find("cd '").expect("after the cd");
        assert!(cd_at < exec_at, "{line}");
        if let Some(env_at) = line.find("CLAUDE_CONFIG_DIR=") {
            assert!(
                env_at < exec_at,
                "the environment is set before exec, not after it: {line}"
            );
        }
        assert!(
            line.trim_end().ends_with(')'),
            "and does it in a subshell, so the person's prompt is not moved: {line}"
        );

        // Without one, the line is what it has always been: run where
        // the shell stands. Not knowing where a session lives is not a
        // reason to refuse to move it off a spent account.
        let plain = profile_cycle_op(true, "u-1", true, 4, 4242, None, 4200, None, &CliChoice::default()).expect("cycle");
        let line = match &plain.steps.iter().find(|s| s.label == "resume").unwrap().kind {
            pty_op::StepKind::Send(b) => String::from_utf8_lossy(b).into_owned(),
            _ => unreachable!(),
        };
        assert!(!line.contains("cd "), "no directory, no cd: {line}");
    }

    /// A directory that could break out of its quoting is refused, and
    /// refusing it costs the whole line rather than producing one that
    /// runs somewhere else.
    #[test]
    fn a_directory_that_cannot_be_quoted_is_not_guessed_at() {
        assert!(
            !profile_cycle_op(true, "u-1", true, 4, 4242, None, 4200, Some("/tmp/it's here"), &CliChoice::default())
                .map(|op| op.steps.iter().any(|s| s.label == "resume"))
                .unwrap_or(false),
            "a path with a quote in it produces no resume line at all"
        );
    }

    /// The hold over the pane lifts when the run ends, and what is
    /// under it is a screen we cleared ourselves -- so the run has to
    /// outlast the new claude's first frame.
    ///
    /// This was here by accident until the sentence moved onto the
    /// command line: the submit step waited for the composer to redraw
    /// with the pasted text in it, which is a first frame by another
    /// name. Taking the typing away took the wait with it and the pane
    /// went black for as long as the new process took to draw.
    #[test]
    fn a_cycle_outlasts_the_first_frame_of_the_new_claude() {
        let op = profile_cycle_op(true, "u-1", true, 4, 4242, None, 4200, None, &CliChoice::default()).expect("cycle");
        let labels: Vec<&str> = op.steps.iter().map(|s| s.label).collect();
        let at = |n: &str| labels.iter().position(|l| *l == n);
        let ready = at("await_tui_ready").expect("the terminal is asked first");
        let frame = at("first_frame").expect("and then waited for");
        let restore = at("restore_composer").expect("the composer comes back last");
        assert!(ready < frame && frame < restore, "{labels:?}");
        assert!(
            matches!(
                &op.steps[frame].kind,
                pty_op::StepKind::AwaitQuiet { min_bytes, .. } if *min_bytes > 0
            ),
            "the wait is for bytes drawn, not for a length of time: {labels:?}"
        );
        assert!(
            !op.steps[frame].proceed_on_timeout,
            "a pane that never draws is not a pane to unfreeze onto: {labels:?}"
        );
        assert!(
            op.steps[ready].proceed_on_timeout,
            "asking the terminal stays a deadline, not a precondition: {labels:?}"
        );
    }

    /// The ordinary case — a pane running claude in the foreground has
    /// a record of its own, and cycling it is the whole feature.
    #[test]
    fn interactive_record_is_not_a_holder() {
        let uuid = "1bcedee1-2228-4e82-a9c2-632aac03bd2d";
        let me = std::process::id() as i32;
        let dir = tmp_config_with_sessions(uuid, &[(me, "interactive")]);
        assert_eq!(background_session_holder(&dir, uuid), None);
        fs::remove_dir_all(&dir).ok();
    }

    /// The directory keeps a file for every pid that ever ran, so the
    /// common shape is a background record whose process is long gone.
    /// Blocking on those would make the cycle unusable after the first
    /// background job the project ever ran.
    #[test]
    fn dead_background_record_is_not_a_holder() {
        let uuid = "1bcedee1-2228-4e82-a9c2-632aac03bd2d";
        // Reserved by POSIX for the swapper / kernel; a `kill(0, ...)`
        // would signal our own process group, which is why the code
        // never passes it — and 2^31-1 is never a live pid here.
        let dead = i32::MAX;
        let dir = tmp_config_with_sessions(uuid, &[(dead, "background")]);
        assert_eq!(background_session_holder(&dir, uuid), None);
        fs::remove_dir_all(&dir).ok();
    }

    /// Another conversation's background job says nothing about this
    /// one — the match is on sessionId, not on "any background job in
    /// this profile".
    #[test]
    fn other_uuid_background_record_is_not_a_holder() {
        let mine = "1bcedee1-2228-4e82-a9c2-632aac03bd2d";
        let theirs = "dc928502-4c1c-4cdc-a1b8-ab993d342c64";
        let me = std::process::id() as i32;
        let dir = tmp_config_with_sessions(theirs, &[(me, "background")]);
        assert_eq!(background_session_holder(&dir, mine), None);
        fs::remove_dir_all(&dir).ok();
    }

    /// A profile that has never run claude has no `sessions/` at all,
    /// and an unreadable directory must not stand in for "blocked" —
    /// the gate stops futile cycles, it does not gate on proof.
    #[test]
    fn missing_sessions_dir_is_not_a_holder() {
        let dir = std::env::temp_dir().join("cc-bg-session-test-nonexistent");
        fs::remove_dir_all(&dir).ok();
        assert_eq!(
            background_session_holder(&dir, "1bcedee1-2228-4e82-a9c2-632aac03bd2d"),
            None
        );
    }

    /// The op that answers a refused click must not take anything from
    /// the pane: a click the cycle declines has to cost no more than a
    /// click that did nothing at all.
    #[test]
    fn cycle_blocked_op_takes_nothing_from_the_pane() {
        let op = cycle_blocked_op("⚠ held by bg job");
        assert!(!op.lock_keys, "a refused click must not eat the keyboard");
        assert!(!op.hold_screen, "nothing to hide — the pane is untouched");
        assert_eq!(op.badge.as_deref(), Some("⚠ held by bg job"));
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

    /// Badges compare as what they would be drawn as.
    fn rendered(b: Option<ModelBadge>) -> Option<String> {
        b.map(|b| b.render())
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

    /// A real status-line payload, captured from claude 2.1.239 by
    /// pointing `--settings` at a command that dumps stdin.  Kept
    /// verbatim so a change in the payload's shape shows up here
    /// rather than as a badge that quietly stops updating.
    const REAL_STATUSLINE_PAYLOAD: &str = concat!(
        r#"{"session_id":"b0b0b0b0-1111-2222-3333-444444444444","#,
        r#""transcript_path":"/Users/x/.claude-profile-3/projects/-p/"#,
        r#"b0b0b0b0-1111-2222-3333-444444444444.jsonl","#,
        r#""cwd":"/Users/x/p","effort":{"level":"high"},"#,
        r#""model":{"id":"claude-opus-5[1m]","display_name":"Opus 5 (1M context)"},"#,
        r#""version":"2.1.239","exceeds_200k_tokens":false}"#,
    );

    #[test]
    fn a_status_line_payload_yields_the_session_and_its_model() {
        let (sid, transcript, badge) =
            statusline_fields(REAL_STATUSLINE_PAYLOAD).expect("payload parses");
        assert_eq!(sid, "b0b0b0b0-1111-2222-3333-444444444444");
        assert!(transcript.ends_with("b0b0b0b0-1111-2222-3333-444444444444.jsonl"));
        // `display_name`, not `id`: the id's `[1m]` suffix is not a
        // model name and `short_model` throws the whole thing out.
        assert_eq!(badge.model, "opus-5");
        assert_eq!(short_model("claude-opus-5[1m]"), "");
        // The same payload states the effort, and the badge draws
        // both halves.
        assert_eq!(badge.effort.as_deref(), Some("high"));
        assert_eq!(badge.render(), "opus-5\u{b7}high");
    }

    #[test]
    fn a_status_line_payload_that_names_a_path_is_refused() {
        let bad = REAL_STATUSLINE_PAYLOAD
            .replace("b0b0b0b0-1111-2222-3333-444444444444\",", "../../etc/passwd\",");
        assert_eq!(statusline_fields(&bad), None);
    }

    /// The reported bug, as a test.
    ///
    /// The transcript's newest word on the subject is `opus-5` — the
    /// model that served the last turn.  The user has since switched
    /// to Fable, which claude knows and the transcript will not
    /// record until the session answers again.  Before the hook the
    /// badge read `opus-5` and stayed there; now claude's own report
    /// A pane whose session file does not exist yet still shows all
    /// three halves — profile, model, effort.
    ///
    /// claude writes that file on its first turn, so for the whole
    /// window in which the user is looking at a pane they just opened
    /// there is nothing to tail.  The status-line hook has run by then
    /// (measured: ~2 s after start), and its record is the only source
    /// that carries the effort as well — the banner line prints
    /// `Opus 5.5 (1M context) · Claude Max`, model and no more.
    #[test]
    fn a_pane_with_no_session_file_yet_still_shows_model_and_effort() {
        let root = std::env::temp_dir().join("cc-fresh-pane-badge-test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", &root) };
        let uuid = "d794e66f-bc9b-48e8-a114-70c5fc996678";
        let push = model_push_dir();
        fs::create_dir_all(&push).unwrap();
        // The shape the hook writes: model, transcript, effort, pids.
        fs::write(
            push.join(uuid),
            format!("opus-5-5\n{}/projects/x/{uuid}.jsonl\nmedium\npids=20809,1492\n", root.display()),
        )
        .unwrap();

        let mut ctx = ctx_for_test(root.join("projects"));
        assert_eq!(
            rendered(ctx.model_for_pane(None, uuid, None, 20809, 436)).as_deref(),
            Some("opus-5-5\u{b7}medium"),
            "the hook's record carries the effort the banner cannot"
        );
    }

    /// The model half must not blink.
    ///
    /// Its sources answer None on some ticks — the banner read is
    /// rate-limited to once every 3 s and the scan runs every 2 — so a
    /// badge that takes None at face value alternates between `P7` and
    /// `P7@opus-5-5` for as long as the pane has no session file
    /// (2026-09-23 report, 20 badge changes in one log tail).
    #[test]
    fn the_model_half_survives_a_tick_that_could_not_read_it() {
        let root = std::env::temp_dir().join("cc-badge-blink-test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", &root) };
        let uuid = "d794e66f-bc9b-48e8-a114-70c5fc996678";
        let mut ctx = ctx_for_test(root.join("projects"));

        // Tick 1: the hook's record was found.
        let first = rendered(ctx.model_for_pane(
            None,
            uuid,
            Some(ModelBadge::new("opus-5-5".into(), Some("medium".into()))),
            20809,
            436,
        ));
        assert_eq!(first.as_deref(), Some("opus-5-5\u{b7}medium"));

        // Tick 2: nothing readable — no record on disk, no banner
        // (no pane registry in a test), and the pane is the same one.
        assert_eq!(
            rendered(ctx.model_for_pane(None, uuid, None, 20809, 436)).as_deref(),
            Some("opus-5-5\u{b7}medium"),
            "a tick that could not read it must not blank the badge"
        );
        // A different pane borrows nothing.
        assert_eq!(rendered(ctx.model_for_pane(None, "", None, 1, 999)), None);
    }

    /// is what the badge shows.
    #[test]
    fn claudes_own_report_beats_the_transcripts_last_word() {
        let root = std::env::temp_dir().join("cc-pushed-model-test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        // Process-global, and nextest gives every test its own
        // process — this is why the suite runs under nextest.
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", &root) };

        let dir = root.join("projects");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("11111111-2222-3333-4444-555555555555.jsonl");
        fs::write(
            &path,
            "{\"role\":\"assistant\",\"model\":\"claude-opus-5\",\"x\":1}\n",
        )
        .unwrap();

        let mut ctx = WorkerCtx {
            projects_root: dir.clone(),
            shelld: Arc::new(ShelldClient::new(None)),
            statusline_state: None,
            seen: HashMap::new(),
            model_cutoff: HashMap::new(),
            banner_tried: HashMap::new(),
            last_refusal: HashMap::new(),
            last_model: HashMap::new(),
            last_model_by_pane: HashMap::new(),
        };
        assert_eq!(rendered(ctx.model_for(&path, 111, 0)).as_deref(), Some("opus-5"));

        let push = model_push_dir();
        fs::create_dir_all(&push).unwrap();
        fs::write(
            push.join("11111111-2222-3333-4444-555555555555"),
            format!("fable-5\n{}\n", path.display()),
        )
        .unwrap();

        assert_eq!(
            rendered(ctx.model_for(&path, 111, 0)).as_deref(),
            Some("fable-5"),
            "the badge follows claude, not the last completed turn"
        );

        // And a session claude has said nothing about still reads its
        // transcript rather than borrowing someone else's record.
        let other = dir.join("99999999-2222-3333-4444-555555555555.jsonl");
        fs::write(
            &other,
            "{\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"x\":1}\n",
        )
        .unwrap();
        assert_eq!(rendered(ctx.model_for(&other, 222, 0)).as_deref(), Some("sonnet-5"));

        let _ = fs::remove_dir_all(&root);
    }

    /// Registering the hook with Claude Code, on and off again.
    ///
    /// The file is the user's, so what is asserted is not just that
    /// the key appears and disappears but that everything around it
    /// comes back byte for byte — across the shapes a settings.json
    /// actually turns up in.
    #[test]
    fn the_hook_registers_and_unregisters_leaving_the_file_as_it_was() {
        let dir = std::env::temp_dir().join("cc-statusline-reconcile-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let bin = std::path::Path::new("/Apps/Marspot.app/Contents/MacOS/marspot-shell");

        let shapes = [
            // One line, several members.
            "{\"a\":1,\"b\":{\"c\":2}}",
            // Indented, statusLine would not be the last member.
            "{\n  \"a\": 1,\n  \"b\": { \"c\": 2 }\n}\n",
            // Indented, one member — so the comma to remove is the one
            // in front, not the one behind.
            "{\n  \"a\": 1\n}\n",
        ];
        for (i, before) in shapes.iter().enumerate() {
            let path = dir.join(format!("settings{i}.json"));
            fs::write(&path, before).unwrap();
            let files = vec![path.clone()];

            // Off and nothing of ours present: not one byte touched.
            assert!(reconcile_statusline_in(false, Some(bin), &files).is_empty());
            assert_eq!(fs::read_to_string(&path).unwrap(), *before);

            let notes = reconcile_statusline_in(true, Some(bin), &files);
            assert!(notes[0].ends_with("installed"), "{notes:?}");
            let on = fs::read_to_string(&path).unwrap();
            assert!(on.contains("--cc-statusline"));
            assert!(json_parses(&on), "{on}");
            // Same again is a no-op — the switch does not rewrite the
            // file on every scan.
            assert!(reconcile_statusline_in(true, Some(bin), &files).is_empty());
            assert_eq!(fs::read_to_string(&path).unwrap(), on);

            reconcile_statusline_in(false, Some(bin), &files);
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                *before,
                "shape {i} did not come back as it was"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// A status line the user already wrote is borrowed, not taken.
    ///
    /// Claude Code allows exactly one, and the people most likely to
    /// want the model in the badge are the ones who already cared
    /// enough to write one.
    #[test]
    fn an_existing_status_line_is_chained_and_handed_back() {
        let dir = std::env::temp_dir().join("cc-statusline-chain-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let bin = std::path::Path::new("/Apps/M.app/marspot-shell");
        // Theirs has a space in it, and a quote, because both are
        // things a status line command really contains.
        let theirs = "~/bin/my line.sh --say \\\"hi\\\"";
        let before =
            format!("{{\n  \"statusLine\": {{ \"type\": \"command\", \"command\": \"{theirs}\" }},\n  \"a\": 1\n}}\n");
        fs::write(&path, &before).unwrap();
        let files = vec![path.clone()];

        let notes = reconcile_statusline_in(true, Some(bin), &files);
        assert!(notes[0].contains("chaining"), "{notes:?}");
        let on = fs::read_to_string(&path).unwrap();
        assert!(json_parses(&on), "{on}");
        let (_, cmd) = status_line_command(&on).unwrap();
        assert!(cmd.starts_with("'/Apps/M.app/marspot-shell' --cc-statusline --chain "));
        // What comes back out of `--chain` is what went in, quotes and
        // spaces intact — it is handed to `sh -c` exactly as claude
        // would have handed it.
        assert_eq!(chained_out_of(&cmd), "~/bin/my line.sh --say \"hi\"");

        reconcile_statusline_in(false, Some(bin), &files);
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        let _ = fs::remove_dir_all(&dir);
    }

    /// An installed hook follows the binary.
    ///
    /// `install-local` leaves the bundle's copy alone while the app
    /// holds it open, so a hook registered then names
    /// `binaries/current/`; the cold launch that refreshes the bundle
    /// is when it should move back.
    #[test]
    fn an_installed_hook_is_repointed_when_the_binary_moves() {
        let dir = std::env::temp_dir().join("cc-statusline-repoint-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let files = vec![path.clone()];
        fs::write(&path, "{\n  \"a\": 1\n}\n").unwrap();

        let old = std::path::Path::new("/state/binaries/current/marspot-shell");
        reconcile_statusline_in(true, Some(old), &files);
        let new = std::path::Path::new("/Apps/Marspot.app/Contents/MacOS/marspot-shell");
        let notes = reconcile_statusline_in(true, Some(new), &files);
        assert!(notes[0].ends_with("repointed"), "{notes:?}");
        let (_, cmd) = status_line_command(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cmd, "'/Apps/Marspot.app/Contents/MacOS/marspot-shell' --cc-statusline");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The guard that stands between an edit and the user's tool.
    #[test]
    fn a_settings_edit_that_would_not_parse_is_refused() {
        assert!(json_parses("{\"a\":1}"));
        assert!(json_parses("{\n  \"a\": [1, 2],\n  \"b\": {}\n}\n"));
        // The two shapes a mis-cut member leaves behind.
        assert!(!json_parses("{\"a\":1,}"));
        assert!(!json_parses("{\"a\":1,,\"b\":2}"));
        assert!(!json_parses("{\"a\":1"));
        // A brace or a comma inside a string is not structure.
        assert!(json_parses("{\"a\":\"},{\"}"));
        assert!(json_parses("{\"a\":\"\\\\\"}"));
    }

    /// The transcript names the effort on the same record as the
    /// model, so the badge gets both halves from one line.
    #[test]
    fn an_assistant_record_names_the_effort_it_ran_at() {
        let dir = std::env::temp_dir().join("cc-effort-tail-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");

        // Shape taken from a real record: `effort` is a sibling of the
        // record's own uuid, not a member of `message`.
        fs::write(
            &path,
            "{\"role\":\"assistant\",\"model\":\"claude-opus-5\",\
             \"uuid\":\"u\",\"effort\":\"high\"}\n",
        )
        .unwrap();
        assert_eq!(rendered(tail_model_short(&path, 0)).as_deref(), Some("opus-5\u{b7}high"));

        // A model with no effort setting: the record simply does not
        // carry one, and the badge says the model alone.
        fs::write(
            &path,
            "{\"role\":\"assistant\",\"model\":\"claude-haiku-4-5\",\"uuid\":\"u\"}\n",
        )
        .unwrap();
        assert_eq!(rendered(tail_model_short(&path, 0)).as_deref(), Some("haiku-4-5"));

        // `/model` output names the model and nothing else.  Saying
        // no effort is the honest reading — the model just changed,
        // and what it will run at is the next turn's news.
        fs::write(
            &path,
            "{\"type\":\"user\",\"message\":{\"content\":\
             \"<local-command-stdout>Set model to \u{1b}[1mFable 5\u{1b}[22m</local-command-stdout>\"}}\n",
        )
        .unwrap();
        assert_eq!(rendered(tail_model_short(&path, 0)).as_deref(), Some("fable-5"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A record written before the effort line existed still reads.
    ///
    /// The hook and the badge are separate binaries during an update
    /// — the running shell reads what a just-replaced one wrote, and
    /// the other way round — so the file's shape has to tolerate both
    /// generations.
    #[test]
    fn a_model_record_without_an_effort_line_still_reads() {
        let root = std::env::temp_dir().join("cc-effort-record-test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", &root) };
        let push = model_push_dir();
        fs::create_dir_all(&push).unwrap();
        let jsonl = root.join("11111111-1111-1111-1111-111111111111.jsonl");
        let name = "11111111-1111-1111-1111-111111111111";

        // Two lines: what 0.7.116 wrote.
        fs::write(push.join(name), "opus-5\n/some/path.jsonl\n").unwrap();
        assert_eq!(
            pushed_model(&jsonl),
            Some((ModelBadge::new("opus-5".into(), None), EffortSaid::No)),
            "two lines is a record that could not say, not one saying no"
        );

        // Three lines, third blank: claude reported no effort, and
        // that is an answer — nothing falls through to fill it in.
        fs::write(push.join(name), "opus-5\n/some/path.jsonl\n\n").unwrap();
        assert_eq!(
            pushed_model(&jsonl),
            Some((ModelBadge::new("opus-5".into(), None), EffortSaid::Yes))
        );

        // Three lines with one.
        fs::write(push.join(name), "opus-5\n/some/path.jsonl\nxhigh\n").unwrap();
        assert_eq!(
            pushed_model(&jsonl).map(|(b, _)| b.render()).as_deref(),
            Some("opus-5\u{b7}xhigh")
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A parked pane still gains the effort it never got pushed.
    ///
    /// The reported case: claude re-runs the status-line hook only
    /// when it redraws, so a pane nobody is in keeps whatever record
    /// it last wrote — here one from a build with no effort line at
    /// all.  Short-circuiting on it would hide the effort the pane's
    /// own transcript is stating plainly, for as long as the pane
    /// stays parked.
    #[test]
    fn a_record_too_old_to_carry_the_effort_takes_it_from_the_transcript() {
        let root = std::env::temp_dir().join("cc-effort-fill-test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", &root) };
        let dir = root.join("projects");
        fs::create_dir_all(&dir).unwrap();
        let name = "22222222-2222-2222-2222-222222222222";
        let path = dir.join(format!("{name}.jsonl"));
        fs::write(
            &path,
            "{\"role\":\"assistant\",\"model\":\"claude-opus-5\",\"effort\":\"high\"}\n",
        )
        .unwrap();
        let push = model_push_dir();
        fs::create_dir_all(&push).unwrap();

        let mut ctx = WorkerCtx {
            statusline_state: None,
            projects_root: dir.clone(),
            shelld: Arc::new(ShelldClient::new(None)),
            seen: HashMap::new(),
            model_cutoff: HashMap::new(),
            banner_tried: HashMap::new(),
            last_refusal: HashMap::new(),
            last_model: HashMap::new(),
            last_model_by_pane: HashMap::new(),
        };

        // The old two-line shape.
        fs::write(push.join(name), format!("opus-5\n{}\n", path.display())).unwrap();
        assert_eq!(
            rendered(ctx.model_for(&path, 111, 0)).as_deref(),
            Some("opus-5\u{b7}high"),
            "the pushed model stands, and the transcript fills the half it could not carry"
        );

        // Claude having actually reported no effort is different, and
        // is left alone — the transcript's older turn does not get to
        // overrule what claude said a moment ago.
        fs::write(push.join(name), format!("opus-5\n{}\n\n", path.display())).unwrap();
        ctx.last_model.clear();
        assert_eq!(rendered(ctx.model_for(&path, 111, 0)).as_deref(), Some("opus-5"));

        // Nor is an effort borrowed across a model change: the
        // transcript's turn was served by something else.
        fs::write(push.join(name), format!("fable-5\n{}\n", path.display())).unwrap();
        ctx.last_model.clear();
        assert_eq!(rendered(ctx.model_for(&path, 111, 0)).as_deref(), Some("fable-5"));

        let _ = fs::remove_dir_all(&root);
    }

    /// The profile-switch case, from the other side.
    ///
    /// The transcript still says `opus-5` — the model the profile that
    /// was just cycled away from served with — and the fence correctly
    /// refuses to read it.  The answer is on the pane's own screen:
    /// the resumed claude reprinted its banner and the banner names
    /// Fable.  Before the reorder `last_model` answered first and the
    /// badge kept saying `opus-5` for as long as the pane stayed
    /// parked.
    #[test]
    fn a_resumed_pane_reads_its_banner_before_falling_back() {
        use marspot_term::session_registry::{write_session_entry, SessionEntry};
        let root = std::env::temp_dir().join("cc-banner-order-test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", &root) };

        let dir = root.join("projects");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        fs::write(
            &path,
            "{\"role\":\"assistant\",\"model\":\"claude-opus-5\",\"x\":1}\n",
        )
        .unwrap();

        let sid = 4242u64;
        write_session_entry(&SessionEntry {
            id: sid,
            pid: 1,
            socket: root.join("s.sock"),
            cols: 100,
            rows: 30,
            title: "t".into(),
            cwd: "/tmp".into(),
            proto_version: 1,
            created_at_unix: 0,
            shm_name: String::new(),
            shell_child_pid: 0,
        })
        .unwrap();
        fs::write(
            marspot_term::paths::sessions_dir().join(sid.to_string()).join("bytelog"),
            "  \u{2599}\u{2584}\u{259f}  Claude Code v2.1.239\r\n\
             \u{2599}\u{2584}\u{259f}  Fable 5 with high effort \u{b7} Claude Max\r\n\
             ~/workspace/goliajp/marspot\r\n",
        )
        .unwrap();

        let mut ctx = WorkerCtx {
            projects_root: dir.clone(),
            shelld: Arc::new(ShelldClient::new(None)),
            statusline_state: None,
            seen: HashMap::new(),
            model_cutoff: HashMap::new(),
            banner_tried: HashMap::new(),
            last_refusal: HashMap::new(),
            last_model: HashMap::new(),
            last_model_by_pane: HashMap::new(),
        };
        // First sighting: the transcript is this process's own.
        assert_eq!(rendered(ctx.model_for(&path, 111, sid)).as_deref(), Some("opus-5"));

        // The switch: new pid, fence at end of file, and the records a
        // resumed process writes at startup name no model.
        use std::io::Write;
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{{\"type\":\"mode\",\"mode\":\"default\"}}").unwrap();
        drop(f);

        assert_eq!(
            rendered(ctx.model_for(&path, 222, sid)).as_deref(),
            Some("fable-5·high"),
            "the banner on the pane's own screen outranks the model \
             the previous profile happened to leave behind, and it \
             names the effort while it is there"
        );

        let _ = fs::remove_dir_all(&root);
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
        assert_eq!(rendered(tail_model_short(&path, 0)).as_deref(), Some("fable-5"));

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
            rendered(tail_model_short(&path, switch_at)).as_deref(),
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
            statusline_state: None,
            projects_root: PathBuf::from("/fake"),
            shelld: Arc::new(ShelldClient::new(None)),
            seen,
            model_cutoff: HashMap::new(),
            banner_tried: HashMap::new(),
            last_refusal: HashMap::new(),
            last_model: HashMap::new(),
            last_model_by_pane: HashMap::new(),
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
            statusline_state: None,
            seen: HashMap::new(),
            model_cutoff: HashMap::new(),
            banner_tried: HashMap::new(),
            last_refusal: HashMap::new(),
            last_model: HashMap::new(),
            last_model_by_pane: HashMap::new(),
        };
        let mut logs = Vec::new();
        // Two panes in alpha: the project is walked once, not twice.
        ctx.refresh_seen(
            [
                (root.as_path(), "-p-alpha"),
                (root.as_path(), "-p-beta"),
                (root.as_path(), "-p-alpha"),
            ]
            .into_iter(),
            &mut logs,
        );

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
        ctx.refresh_seen([(root.as_path(), "-p-alpha")].into_iter(), &mut logs2);
        assert_eq!(ctx.seen.len(), SESSIONS_KEPT_PER_PROJECT);
        assert!(ctx.seen.values().all(|s| s.project_dir == "-p-alpha"));

        let _ = fs::remove_dir_all(&root);
    }

    fn filetime_plus(secs: u64) -> SystemTime {
        // A fixed base well in the past keeps the ordering deterministic
        // regardless of when the test runs.
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs)
    }

    fn set_mtime(path: &Path, t: SystemTime) {
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
            statusline_state: None,
            seen: HashMap::new(),
            model_cutoff: HashMap::new(),
            banner_tried: HashMap::new(),
            last_refusal: HashMap::new(),
            last_model: HashMap::new(),
            last_model_by_pane: HashMap::new(),
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

    /// 2026-08-02 report: after a profile switch the badge lost its
    /// model half and kept it lost.
    ///
    /// The fence is right — the old process's records do not describe
    /// the new one — but a resumed claude writes nothing that names a
    /// model until it finishes a turn (`mode` / `permission-mode` are
    /// what it does write), so between the switch and the session's
    /// next answer there is nothing behind the fence to read.  On a
    /// parked pane that is hours of a badge saying `P4` alone.
    #[test]
    fn a_switched_profile_keeps_showing_the_last_model_it_saw() {
        let dir = std::env::temp_dir().join("cc-model-carry-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let assistant =
            |m: &str| format!("{{\"role\":\"assistant\",\"model\":\"claude-{m}\",\"x\":1}}\n");
        fs::write(&path, assistant("fable-5")).unwrap();

        let mut ctx = WorkerCtx {
            projects_root: dir.clone(),
            shelld: Arc::new(ShelldClient::new(None)),
            statusline_state: None,
            seen: HashMap::new(),
            model_cutoff: HashMap::new(),
            banner_tried: HashMap::new(),
            last_refusal: HashMap::new(),
            last_model: HashMap::new(),
            last_model_by_pane: HashMap::new(),
        };
        assert_eq!(rendered(ctx.model_for(&path, 111, 0)).as_deref(), Some("fable-5"));

        // The switch: new pid, so the fence lands at end of file — and
        // the startup records a resumed process writes name no model.
        use std::io::Write;
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{{\"type\":\"mode\",\"mode\":\"default\"}}").unwrap();
        writeln!(f, "{{\"type\":\"permission-mode\"}}").unwrap();
        drop(f);
        assert_eq!(
            rendered(ctx.model_for(&path, 222, 0)).as_deref(),
            Some("fable-5"),
            "the badge keeps what it last saw rather than going blank"
        );

        // …and yields the moment the new process actually says so.
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        write!(f, "{}", assistant("opus-5")).unwrap();
        drop(f);
        assert_eq!(rendered(ctx.model_for(&path, 222, 0)).as_deref(), Some("opus-5"));

        // A session that has never named one has nothing to show, and
        // nothing is invented for it.
        let fresh = dir.join("fresh.jsonl");
        fs::write(&fresh, "{\"type\":\"mode\"}\n").unwrap();
        assert_eq!(ctx.model_for(&fresh, 333, 0), None);

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
        assert_eq!(rendered(tail_model_short(&path, 0)).as_deref(), Some("opus-4-8"));

        // ...and vice versa: an assistant turn after the switch wins.
        // (older "system"/"local_command" record shape)
        let path = tmpfile(concat!(
            r#"{"type":"system","subtype":"local_command","content":"<local-command-stdout>Kept model as \u001b[1mOpus 4.8\u001b[22m</local-command-stdout>"}"#,
            "\n",
            r#"{"type":"message","role":"assistant","model":"claude-fable-5","content":[]}"#,
            "\n",
        ));
        assert_eq!(rendered(tail_model_short(&path, 0)).as_deref(), Some("fable-5"));

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
        assert_eq!(rendered(tail_model_short(&path, 0)).as_deref(), Some("fable-5"));
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

    // ── the whole loop, on a PTY this test owns ───────────────────
    //
    // Everything below runs against real machinery: a real zsh on a
    // real PTY, a real registry entry, the real scan, the real state
    // machine, a real signal, and the real wake path writing into the
    // real PTY.  The master fd IS the keyboard here — no window, no
    // synthetic events, nothing of the user's touched.
    //
    // What it cannot cover is claude's own behaviour on `--resume`;
    // that belongs to claude, and the byte path it arrives on is the
    // one the profile cycle has used in production every day.

    /// Wake injections land here instead of going L1 → L2 → L3, so the
    /// test can assert on exactly what would reach the PTY — and then
    /// actually put it there.
    struct PtyInject {
        master: std::os::fd::RawFd,
        sent: std::sync::Mutex<Vec<u8>>,
        /// Every hold/release the session asked for, in order — the
        /// sequence is the whole point (hold before the kill, release
        /// only once the new picture is ready).
        holds: std::sync::Mutex<Vec<bool>>,
        /// Every "the foreground program is gone" the op sent, so a
        /// test can assert the kill was followed by one.
        mouse_resets: std::sync::Mutex<Vec<u64>>,
    }

    impl InjectInputProxy for PtyInject {
        fn inject_input(&self, _sid: u64, bytes: &[u8]) -> std::io::Result<()> {
            self.sent.lock().unwrap().extend_from_slice(bytes);
            let n = unsafe {
                libc::write(
                    self.master,
                    bytes.as_ptr() as *const libc::c_void,
                    bytes.len(),
                )
            };
            (n > 0).then_some(()).ok_or_else(|| {
                std::io::Error::other("write to pty master failed")
            })
        }

        fn hold_grid(&self, _sid: u64, on: bool) -> std::io::Result<()> {
            self.holds.lock().unwrap().push(on);
            Ok(())
        }

        fn paste(&self, _sid: u64, text: &str) -> std::io::Result<()> {
            // Same destination as `inject_input` for the test's
            // purposes: what matters is the bytes that reach the PTY.
            self.inject_input(0, text.as_bytes())
        }

        fn reset_mouse_reporting(&self, sid: u64) -> std::io::Result<()> {
            self.mouse_resets.lock().unwrap().push(sid);
            Ok(())
        }
    }

    /// Drain a PTY master (set non-blocking by the caller) into a
    /// string, so the test can see what the shell echoed.
    fn drain_pty(master: std::os::fd::RawFd) -> String {
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = unsafe {
                libc::read(master, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
            };
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    fn poll_until<F: FnMut() -> bool>(what: &str, mut f: F) {
        // Sixty, not ten.  What is being waited for is a process
        // appearing and a scan noticing it; how long that takes is a
        // property of the machine, and ten seconds was enough here and
        // not enough on a shared build runner.  The budget only has to
        // be long enough that exceeding it means something is actually
        // wrong.
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for {what}");
    }

    /// Hibernate → dormant → keypress → resume, end to end.
    #[test]
    fn the_whole_idle_loop_runs_on_a_pty_this_test_owns() {
        // This one drives a real pseudo-terminal, types into a shell,
        // waits for a process to appear in the tree and then reads the
        // transcript it writes.  It is shaped like the machines this
        // project is developed on, and on a shared build runner it
        // binds the pane and then reports no activity for it — a
        // difference in the environment, not in the code, and not one
        // worth weakening the test to paper over.
        //
        // So it is opt-in, and `bin/test.sh` opts in.  The gate still
        // runs it every time; CI says it skipped rather than pretending
        // to have run it.
        if std::env::var_os("MARSPOT_PTY_INTEGRATION").is_none() {
            eprintln!(
                "SKIPPED: needs a real pty and process tree — set \
                 MARSPOT_PTY_INTEGRATION=1 (bin/test.sh does)"
            );
            return;
        }
        use marspot_term::pty::{Pty, PtyConfig, TerminalSize};

        let root = std::env::temp_dir().join(format!("marspot-loop-{}", std::process::id()));
        let bin = root.join("bin");
        let proj = root.join("proj");
        let state = root.join("state");
        for d in [&bin, &proj, &state] {
            fs::create_dir_all(d).unwrap();
        }
        // A "claude" that is a real binary, so argv[0] ends in
        // `claude` — the identity both the scan and the pid guard
        // check.  A #! wrapper would give argv[0] = /bin/sh and be
        // (correctly) rejected.
        //
        // Symlink, not copy: a copied system binary is killed on exec
        // ("zsh: killed") because the signature no longer matches the
        // file it is being loaded from.  The symlink runs the original,
        // signed binary under the name we need.
        std::os::unix::fs::symlink("/bin/sleep", bin.join("claude")).unwrap();

        // The transcript the scan binds against: one finished assistant
        // turn, in the on-disk shape (`message` before `type`).
        let uuid = "aaaaaaaa-1111-2222-3333-444444444444";
        let home = std::env::var("HOME").unwrap();
        // Use the plugin's own encoder, and canonicalise first: the
        // scan encodes what `proc_cwd` reports, which is the resolved
        // path (`/private/var/...`), while `temp_dir()` hands back the
        // symlinked one (`/var/...`).  Encoding the wrong one puts the
        // fixture transcript in a directory the scan never looks at.
        let proj = fs::canonicalize(&proj).unwrap();
        let encoded = encode_project_dir(&proj);
        let proj_dir = PathBuf::from(&home).join(".claude/projects").join(&encoded);
        fs::create_dir_all(&proj_dir).unwrap();
        let jsonl = proj_dir.join(format!("{uuid}.jsonl"));
        fs::write(
            &jsonl,
            format!(
                r#"{{"parentUuid":"p","isSidechain":false,"message":{{"model":"m","id":"i","type":"message","role":"assistant","content":[{{"type":"text","text":"done"}}]}},"type":"assistant","uuid":"u","sessionId":"{uuid}"}}"#
            ) + "\n",
        )
        .unwrap();

        // A real shell on a PTY this test owns.
        let pty = Pty::spawn(PtyConfig {
            program: "/bin/zsh".into(),
            args: vec!["-f".into()],
            size: TerminalSize { cols: 80, rows: 24, pixel_width: 0, pixel_height: 0 },
            argv0: None,
            cwd: Some(proj.to_string_lossy().into_owned()),
            env_remove_prefixes: marspot_term::pty::SESSION_ENV_PREFIXES
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            env_set: Vec::new(),
        })
        .expect("spawn zsh");
        let master = pty.raw_master();
        unsafe {
            let fl = libc::fcntl(master, libc::F_GETFL);
            libc::fcntl(master, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        let shell_pid = pty.child_pid();

        // Registry entry, so the plugin's session list finds this pane
        // exactly as it finds a real one.
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", &state) };
        marspot_term::session_registry::write_session_entry(
            &marspot_term::session_registry::SessionEntry {
                id: 1,
                pid: std::process::id() as i32,
                socket: state.join("s.sock"),
                cols: 80,
                rows: 24,
                title: "loop".into(),
                cwd: proj.to_string_lossy().into_owned(),
                proto_version: 1,
                created_at_unix: 0,
                shm_name: String::new(),
                shell_child_pid: shell_pid,
            },
        )
        .unwrap();

        // Start the stand-in claude in the pane — by writing to the
        // master, which is what a keyboard is.
        // Put the stand-in on the pane's PATH before starting it: the
        // wake writes a bare `claude --resume …`, so PATH is what
        // decides whether this test drives the stand-in or launches a
        // real claude (it launched a real one until this line existed
        // — visible in the pane's output as the trust prompt).
        // A profile dir on the stand-in's environment, so the scan
        // reads it exactly as it reads a real one — and so the wake
        // has to carry it back.  Without it the policy now (rightly)
        // refuses to reclaim a session it cannot name the profile of.
        let profile_dir = root.join(".claude-profile-9");
        fs::create_dir_all(&profile_dir).unwrap();
        let start = format!(
            "export PATH={}:$PATH CLAUDE_CONFIG_DIR={}\rclaude 100000\r",
            bin.to_string_lossy(),
            profile_dir.to_string_lossy(),
        );
        unsafe {
            libc::write(
                master,
                start.as_ptr() as *const libc::c_void,
                start.len(),
            )
        };
        poll_until("the stand-in claude to be running", || {
            drain_pty(master);
            let procs = pidtree::list_all_procs();
            pidtree::descendants_of(shell_pid, &procs)
                .iter()
                .any(looks_like_claudecode)
        });
        // The scan only binds a transcript that is newer than the
        // process writing it, same as in production.
        fs::write(&jsonl, fs::read(&jsonl).unwrap()).unwrap();

        // Real scan → real binding.
        let mut worker = WorkerCtx {
            projects_root: PathBuf::from(&home).join(".claude/projects"),
            shelld: Arc::new(ShelldClient::new(None)),
            statusline_state: None,
            seen: HashMap::new(),
            model_cutoff: HashMap::new(),
            banner_tried: HashMap::new(),
            last_refusal: HashMap::new(),
            last_model: HashMap::new(),
            last_model_by_pane: HashMap::new(),
        };
        let mut scan = worker.scan_once();
        poll_until("the scan to bind the pane", || {
            scan = worker.scan_once();
            scan.new_meta.contains_key(&1)
        });
        let claude_pid = scan.new_meta[&1].claude_pid;
        assert_eq!(
            scan.new_activity.get(&1),
            Some(&marspot::pane_state::Activity::AwaitingUser),
            "a finished turn reads as awaiting the user"
        );

        // The real state machine, fed the real kernel observation.
        let mut machine = marspot::pane_state::PaneMachine::new(std::time::Instant::now());
        let base = std::time::Instant::now();
        for i in 1..=marspot::pane_state::CONFIRM_TICKS {
            machine.observe(
                marspot::pane_state::Observation {
                    generic: pidtree::observe_pane(shell_pid),
                    activity: marspot::pane_state::Activity::AwaitingUser,
                    pty_quiet: true,
                },
                base + Duration::from_secs(i as u64),
            );
        }
        assert!(
            machine.quiescent(),
            "machine should call this pane quiet: {:?}",
            machine.status()
        );

        // Policy, with the pane presented as long-idle.
        let inject = Arc::new(PtyInject {
            master,
            sent: std::sync::Mutex::new(Vec::new()),
            holds: std::sync::Mutex::new(Vec::new()),
            mouse_resets: std::sync::Mutex::new(Vec::new()),
        });
        // The host owns the queue, so give it the route to this test's
        // PTY: otherwise the reclamation half would run against a
        // no-op client and the test would be watching a shadow.
        let host = FakeHost::with_io(
            state.clone(),
            Arc::new(ShelldClient::new(Some(inject.clone()))) as Arc<dyn pty_op::PtyIo>,
        );
        host.set_status(
            1,
            crate::plugins::PaneStatusView {
                status: machine.status().clone(),
                held: Duration::from_secs(7200),
                quiescent: true,
            },
        );
        let mut plugin = ClaudecodePlugin::new();
        plugin.shelld = Some(Arc::new(ShelldClient::new(Some(inject.clone()))));

        // First pass records the CPU baseline; make it old enough for
        // the second to be able to decide.
        let mut first = ScanResult {
            new_mapping: scan.new_mapping.clone(),
            new_models: HashMap::new(),
            new_meta: scan.new_meta.clone(),
            new_activity: scan.new_activity.clone(),
            new_cpu: HashMap::new(),
            new_vetoes: HashMap::from([(1, (false, false))]),
            scanned_at: SystemTime::now(),
            sessions_seen: vec![1],
            log_lines: Vec::new(),
        };
        // The stand-in is a system binary, and macOS redacts the
        // environment of hardened ones — `proc_cmdline` works on it,
        // `proc_env_value` returns None (measured).  A real claude is
        // an ordinary user binary and reads fine, which is why the
        // badge shows P1/P2/P3 in production.  So the profile is
        // supplied here rather than pretending the scan could read it;
        // everything downstream of the binding is still the real path.
        for m in first.new_meta.values_mut() {
            m.profile_num = 9;
            m.config_dir = Some(profile_dir.to_string_lossy().into_owned());
            // The clock that decides is the transcript's age; this
            // fixture's session has been idle for hours.
            m.transcript_at = SystemTime::now() - Duration::from_secs(7200);
        }
        first.new_cpu.insert(
            1,
            (
                pidtree::subtree_cpu_time_ns(claude_pid, &pidtree::list_all_procs()),
                SystemTime::now() - Duration::from_secs(120),
            ),
        );
        plugin.run_idle_policy_at(&host, &first, TEST_THRESHOLD);
        assert!(plugin.dormant.is_empty(), "one sample decides nothing");

        let mut second = ScanResult {
            new_mapping: first.new_mapping.clone(),
            new_models: HashMap::new(),
            new_meta: first.new_meta.clone(),
            new_activity: first.new_activity.clone(),
            new_cpu: HashMap::new(),
            new_vetoes: HashMap::from([(1, (false, false))]),
            scanned_at: SystemTime::now(),
            sessions_seen: vec![1],
            log_lines: Vec::new(),
        };
        second.new_cpu.insert(
            1,
            (
                pidtree::subtree_cpu_time_ns(claude_pid, &pidtree::list_all_procs()),
                SystemTime::now(),
            ),
        );
        plugin.run_idle_policy_at(&host, &second, TEST_THRESHOLD);
        assert_eq!(plugin.dormant.len(), 1, "the pane should now be dormant");
        assert_eq!(
            plugin.dormant[0].config_dir.as_deref(),
            Some(profile_dir.to_string_lossy().as_ref()),
            "the parked record carries the profile the session ran under"
        );
        assert_eq!(plugin.dormant[0].uuid, uuid);

        // Nothing has run yet: the decision queues an op, the service
        // starts it on the next tick.  Both halves are production's,
        // and the test drives them in the same order.
        host.pump_ops();
        // The signal is sent from the run's own ticks, not from the
        // policy call: the pane's picture has to be held first, and
        // that request crosses two process boundaries while a signal
        // crosses none.  Drive the ticks the way the real host does.
        let ticking_host = FakePaneSessionHost { sid: 1 };
        let tick_all = || {
            for s in host.sessions.lock().unwrap().iter_mut() {
                s.on_tick(&ticking_host);
            }
        };
        // The signal was real: claude is gone from the pane.
        poll_until("the stand-in claude to be reclaimed", || {
            tick_all();
            let procs = pidtree::list_all_procs();
            !pidtree::descendants_of(shell_pid, &procs)
                .iter()
                .any(looks_like_claudecode)
        });

        // …and the shell is back in front of its own tty, which is
        // what makes the pane usable again.
        poll_until("the shell to take the tty back", || {
            drain_pty(master);
            matches!(
                pidtree::observe_pane(shell_pid),
                marspot::pane_state::Generic::Idle
            )
        });

        // Wake: focus, which is what the user actually does first.
        // (A keypress works too — same path — but waiting for one
        // starts the restore after they have already tried to use the
        // pane.)
        // The run the policy armed is this same script, parked.  Rebuild
        // it at the park step — which is exactly what a re-arm after an
        // L1 restart does — and drive the wake directly.
        let client = Arc::new(ShelldClient::new(Some(inject.clone())));
        let op = reclaim_op(uuid, Some(&profile_dir.to_string_lossy()), 0, shell_pid, None)
            .expect("a quotable profile builds a script");
        // By name, exactly as the re-arm does -- a test that hard-codes
        // the position is a test that stops asking the question when a
        // step is inserted, which is how this one found the bug.
        let park = op
            .index_of(idle::RECLAIM_PARK_LABEL)
            .expect("the script has a park step");
        let mut session = pty_op::OpRunner::new(
            op,
            Box::new(pty_op::RealEnv::new(client as Arc<dyn pty_op::PtyIo>)),
        )
        .start_at(park);
        let host_session = FakePaneSessionHost { sid: 1 };
        assert!(session.is_awaiting_user(), "a re-armed run parks at the wake");
        crate::plugins::PaneSession::on_focus(&mut session, &host_session);
        assert!(
            !session.is_awaiting_user(),
            "focusing a parked pane starts the restore"
        );
        // Two ticks: one runs the "has it come back by itself?" check
        // the focus unblocked, the next types.  The host's loop is what
        // drives a script forward, one step per tick.
        crate::plugins::PaneSession::on_tick(&mut session, &host_session);
        crate::plugins::PaneSession::on_tick(&mut session, &host_session);
        // A keystroke arriving mid-wake is swallowed rather than run
        // as a shell command.
        let handling = crate::plugins::PaneSession::on_user_key(
            &mut session,
            &host_session,
            &marspot::shell_proto::WireKeyEvent {
                state: marspot::shell_proto::WireKeyState::Pressed,
                mods: 0,
                kind: marspot::shell_proto::WireLogicalKind::Char,
                key_data: 'x' as u32,
                text: "x".into(),
                ..Default::default()
            },
        );
        assert!(
            matches!(handling, crate::plugins::KeyHandling::Swallow),
            "keys during the wake are consumed, not run as shell commands"
        );
        // In pieces, not as one string: the line carries a
        // `--settings` document naming this binary by absolute path,
        // which differs between a dev build, the bundle and
        // `binaries/current`.
        let sent = String::from_utf8_lossy(&inject.sent.lock().unwrap()).into_owned();
        assert!(
            sent.starts_with(&format!(
                "printf '\\033[H\\033[2J'; CLAUDE_CONFIG_DIR='{}' claude ",
                profile_dir.to_string_lossy()
            )),
            "the wake resumes under the profile it was running, behind a \
             screen wipe so the line itself never shows: {sent}"
        );
        assert!(sent.ends_with(&format!(" --resume {uuid}\r")), "{sent}");
        // It really reached the PTY: the shell echoes it back…
        let mut echoed = String::new();
        poll_until("the pane to echo the resume line", || {
            echoed.push_str(&drain_pty(master));
            echoed.contains("--resume")
        });
        // …and the shell RUNS it.  Echo alone was what the first
        // version asserted, and it is not the same claim: the user
        // reported the line sitting on the prompt waiting for them to
        // press Enter, which an echo-only assertion cannot see.
        // `claude` here is the stand-in (a `sleep` symlink), so it
        // exits immediately on the unknown `--resume` argument — what
        // matters is that the shell dispatched the line at all, which
        // shows up as the prompt coming back after it.
        poll_until("the shell to execute the resume line", || {
            echoed.push_str(&drain_pty(master));
            // The stand-in is `sleep` under another name, so the
            // resume's arguments make it fail loudly — which is proof
            // the line was DISPATCHED, not left sitting on the prompt
            // for the user to press Enter on (the bug this asserts
            // against).
            echoed.contains("invalid time interval") || echoed.contains("usage:")
        });

        // Teardown: kill whatever the pane still holds, then the shell.
        for pid in pidtree::child_pids(shell_pid) {
            if let Some(row) = pidtree::proc_row(pid) {
                unsafe {
                    libc::killpg(row.pgid, libc::SIGCONT);
                    libc::killpg(row.pgid, libc::SIGKILL);
                }
            }
        }
        drop(pty);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&proj_dir);
    }

    /// Minimal `PaneSessionHost` for driving a PaneSession directly.
    struct FakePaneSessionHost {
        sid: u64,
    }

    impl crate::plugins::PaneSessionHost for FakePaneSessionHost {
        fn shelld_session_id(&self) -> u64 {
            self.sid
        }
        fn end(&self) {}
        fn set_badge(&self, _text: &str) {}
        fn set_pane_title(&self, _text: &str) {}
        fn log(&self, _l: LogLevel, _t: &str, _m: &str) {}
    }

    // ── the reclamation itself, against a real process ────────────


    /// The freeze lifts when claude has finished drawing, not when its
    /// process appears.
    ///
    /// Those are seconds apart, and ending on "the process exists" is
    /// what the user saw: the pane unfroze onto the echoed
    /// `claude --resume …` line and the startup output scrolling by,
    /// The wake waits for claude to finish *painting*, and only counts
    /// output that arrives after claude exists.
    ///
    /// Both halves shipped broken.  Ending on "the process exists"
    /// unfroze the pane onto the echoed resume line and the startup
    /// scroll; then counting ticks instead of milliseconds made the
    /// still-window 48 ms (`on_tick` rides the redraw pump: 16 ms while
    /// a pane is drawing, ~250 ms when the window is idle), which lands
    /// inside claude's own startup pause.  The rule itself now lives in
    /// `pty_op::StepKind::AwaitQuiet`; what stays claudecode's to choose
    /// is how long, and that it comes after the process check.
    #[test]
    fn the_wake_waits_for_the_repaint_after_the_process_appears() {
        let op = reclaim_op("u", None, 1, 2, None).expect("script builds");
        let kinds: Vec<&pty_op::StepKind> = op.steps.iter().map(|s| &s.kind).collect();
        let process_at = kinds
            .iter()
            .position(|k| matches!(k, pty_op::StepKind::AwaitProcess { .. }))
            .expect("waits for claude to come back");
        let quiet_at = kinds
            .iter()
            .position(|k| matches!(k, pty_op::StepKind::AwaitQuiet { .. }))
            .expect("waits for it to finish drawing");
        assert!(
            process_at < quiet_at,
            "output before claude exists is the shell's, not claude's"
        );
        assert!(
            matches!(
                kinds[quiet_at],
                pty_op::StepKind::AwaitQuiet { still, min_bytes }
                    if *still >= Duration::from_millis(400) && *min_bytes > 0
            ),
            "a still-window measured in tens of ms lands inside claude's \
             startup pause, and silence alone lands inside its load"
        );
    }

    /// A `PluginHost` that records what the plugin asked it to do.
    /// Only the methods the idle policy touches do anything.
    struct FakeHost {
        state_dir: PathBuf,
        status: std::sync::Mutex<HashMap<u64, crate::plugins::PaneStatusView>>,
        begun: std::sync::Mutex<Vec<u64>>,
        /// The sessions handed over, kept so a test can tick them.
        /// The real host does exactly this; dropping them on the floor
        /// made the reclamation look synchronous, which it is not.
        sessions: std::sync::Mutex<Vec<Box<dyn crate::plugins::PaneSession>>>,
        /// The host owns the op queue in production, so the fake does
        /// too — a test that pumped a plugin-private queue would be
        /// testing a shape that no longer exists.
        ops: std::sync::Mutex<pty_op::PtyOps>,
        /// Every op handed over, as (session, step labels).  The queue
        /// itself runs on wall-clock deadlines, so a test that wants to
        /// know WHAT was asked for — rather than wait for it — reads
        /// this.
        submitted: std::sync::Mutex<Vec<(u64, Vec<&'static str>)>>,
        /// What each op would write into the pane, as (session, bytes
        /// of every `Send` step joined).  The sentence a moved pane is
        /// told now travels on claude's command line, so asserting it
        /// means reading the bytes, not a step label.
        sent: std::sync::Mutex<Vec<(u64, String)>>,
        /// Every badge / title push, in order.
        pane_badges: std::sync::Mutex<Vec<(u64, String)>>,
        pane_titles: std::sync::Mutex<Vec<(u64, String)>>,
    }

    impl FakeHost {
        fn new(dir: PathBuf) -> Self {
            Self::with_io(dir, Arc::new(ShelldClient::new(None)) as Arc<dyn pty_op::PtyIo>)
        }

        fn with_io(dir: PathBuf, io: Arc<dyn pty_op::PtyIo>) -> Self {
            Self {
                state_dir: dir,
                status: std::sync::Mutex::new(HashMap::new()),
                begun: std::sync::Mutex::new(Vec::new()),
                sessions: std::sync::Mutex::new(Vec::new()),
                ops: std::sync::Mutex::new(pty_op::PtyOps::new(io)),
                submitted: std::sync::Mutex::new(Vec::new()),
                sent: std::sync::Mutex::new(Vec::new()),
                pane_badges: std::sync::Mutex::new(Vec::new()),
                pane_titles: std::sync::Mutex::new(Vec::new()),
            }
        }

        /// Start what was submitted, as the supervisor loop does.
        fn pump_ops(&self) {
            let mut ops = self.ops.lock().unwrap();
            ops.pump(self);
        }
        fn set_status(&self, sid: u64, view: crate::plugins::PaneStatusView) {
            self.status.lock().unwrap().insert(sid, view);
        }
    }

    impl PluginHost for FakeHost {
        fn set_pane_badge(
            &self,
            shelld_session_id: u64,
            text: &str,
        ) -> Result<(), PluginError> {
            self.pane_badges
                .lock()
                .unwrap()
                .push((shelld_session_id, text.to_string()));
            Ok(())
        }
        fn set_pane_title(
            &self,
            shelld_session_id: u64,
            text: &str,
        ) -> Result<(), PluginError> {
            self.pane_titles
                .lock()
                .unwrap()
                .push((shelld_session_id, text.to_string()));
            Ok(())
        }
        fn pane_count(&self) -> usize {
            1
        }
        fn pane_pty_device(&self, _: usize) -> Result<Option<PathBuf>, PluginError> {
            Ok(None)
        }
        fn pane_pty_pid_tree(
            &self,
            _: usize,
        ) -> Result<Vec<crate::plugins::PtyChild>, PluginError> {
            Ok(Vec::new())
        }
        fn pane_focused(&self) -> Option<usize> {
            None
        }
        fn pane_status(
            &self,
            sid: u64,
        ) -> Result<Option<crate::plugins::PaneStatusView>, PluginError> {
            Ok(self.status.lock().unwrap().get(&sid).cloned())
        }
        fn state_dir(&self) -> Result<PathBuf, PluginError> {
            Ok(self.state_dir.clone())
        }
        fn log(&self, _: LogLevel, _: &str, _: &str) {}
        fn submit_pty_op_at(
            &self,
            sid: u64,
            op: pty_op::PtyOp,
            start_at: usize,
        ) -> Result<(), PluginError> {
            self.submitted
                .lock()
                .unwrap()
                .push((sid, op.steps.iter().map(|s| s.label).collect()));
            let sent: Vec<String> = op
                .steps
                .iter()
                .filter_map(|s| match &s.kind {
                    pty_op::StepKind::Send(bytes) => {
                        Some(String::from_utf8_lossy(bytes).into_owned())
                    }
                    _ => None,
                })
                .collect();
            self.sent.lock().unwrap().push((sid, sent.join("\n")));
            self.ops
                .lock()
                .unwrap()
                .submit_at(sid, op, start_at)
                .map(|_| ())
                .ok_or_else(|| PluginError::Other("queue full".into()))
        }

        fn begin_pane_session(
            &self,
            sid: u64,
            _session: Box<dyn crate::plugins::PaneSession>,
        ) -> Result<(), PluginError> {
            self.begun.lock().unwrap().push(sid);
            self.sessions.lock().unwrap().push(_session);
            Ok(())
        }
    }

    /// Build a process the policy will accept as claude, and that can
    /// be observed dying.
    ///
    /// The guard reads `proc_cmdline`, i.e. **argv**, so what matters
    /// is argv[0] — not the executable's name.  Measured on a live
    /// claude: `ps comm` (argv[0]) is `claude` while `pbi_comm` (the
    /// executable file) is the version string `2.1.220`, so argv is
    /// the field with the program's identity in it.  A wrapper script
    /// would produce argv[0] = `/bin/sh` and be rejected, which is
    /// correct behaviour and useless as a fixture; `arg0` gives the
    /// real shape instead.
    fn spawn_fake_claude(_dir: &std::path::Path) -> std::process::Child {
        use std::os::unix::process::CommandExt;
        std::process::Command::new("/bin/sleep")
            .arg0("claude")
            .arg("120")
            .spawn()
            .unwrap()
    }

    fn idle_view(held: Duration) -> crate::plugins::PaneStatusView {
        crate::plugins::PaneStatusView {
            status: marspot::pane_state::PaneStatus::AwaitingUser,
            held,
            quiescent: true,
        }
    }

    fn scan_with(sid: u64, claude_pid: i32, uuid: &str) -> ScanResult {
        let mut new_meta = HashMap::new();
        new_meta.insert(
            sid,
            BindMeta {
                profile_num: 2,
                config_dir: Some("/Users/x/.claude-profile-2".into()),
                uuid: uuid.to_string(),
                claude_pid,
                project_basename: "proj".into(),
            project_dir: None,
            refused_at: None,
                // Long enough ago that the transcript clock is not what
                // any of these tests are about.
                transcript_at: SystemTime::now() - Duration::from_secs(7200),
                has_transcript: true,
            },
        );
        let mut new_cpu = HashMap::new();
        new_cpu.insert(sid, (1_000u64, SystemTime::now()));
        ScanResult {
            new_mapping: HashMap::new(),
            new_models: HashMap::new(),
            new_meta,
            new_activity: HashMap::new(),
            new_cpu,
            new_vetoes: HashMap::from([(sid, (false, false))]),
            scanned_at: SystemTime::now(),
            sessions_seen: vec![sid],
            log_lines: Vec::new(),
        }
    }

    /// The 2026-08-03 report: a pane running claudecode with no badge
    /// at all.  claude writes its session file on the first turn, so
    /// between `claude` starting and the user's first prompt there was
    /// nothing to bind to and the corner stayed empty for minutes.
    ///
    /// The profile is readable from the process the whole time, so
    /// that window badges `P<n>` and gains `@model` once the session
    /// file shows up.  What such a pane must NOT do is get taken down
    /// and resumed — there is no session to resume.
    #[test]
    fn a_pane_with_no_session_file_yet_is_badged_but_never_reclaimed() {
        assert!(
            reclaim_op("", Some("/Users/x/.claude-profile-2"), 4242, 4200, None).is_none(),
            "no uuid ⇒ no resume line ⇒ claude must not be taken down"
        );
        assert!(
            profile_cycle_op(true, "", false, 3, 4242, None, 4200, None, &CliChoice::default()).is_none(),
            "…and the badge-click cycle refuses for the same reason"
        );
        // The same call with a uuid is the normal path, so the guard
        // above is the only thing being tested here.
        assert!(reclaim_op("u-1", None, 4242, 4200, None).is_some());
        assert!(profile_cycle_op(true, "u-1", true, 3, 4242, None, 4200, None, &CliChoice::default()).is_some());
    }

    /// The record the hook leaves is found by the pid of the claude
    /// that ran it — the key that works before there is a transcript.
    #[test]
    fn the_hooks_record_is_found_by_which_claude_asked_for_it() {
        let state = std::env::temp_dir().join(format!("marspot-push-{}", std::process::id()));
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", &state) };
        let dir = model_push_dir();
        fs::create_dir_all(&dir).unwrap();
        let write = |uuid: &str, body: &str| fs::write(dir.join(uuid), body).unwrap();

        write("u-mine", "opus-5\n/t/u-mine.jsonl\nhigh\npids=9001,4242,1\n");
        write("u-theirs", "fable-5\n/t/u-theirs.jsonl\nhigh\npids=9002,7777,1\n");
        // Written before the hook recorded a chain: skipped, because a
        // record that cannot say which claude it belongs to must not
        // be guessed onto one.
        write("u-old", "sonnet-4.5\n/t/u-old.jsonl\nhigh\n");

        let got = pushed_session_for_pid(4242).expect("the claude that ran the hook");
        assert_eq!(got.0, "u-mine");
        assert_eq!(got.2.render(), "opus-5\u{b7}high");
        assert_eq!(got.1, PathBuf::from("/t/u-mine.jsonl"));

        assert!(
            pushed_session_for_pid(4243).is_none(),
            "a pid in no chain matches nothing",
        );
        fs::remove_dir_all(&state).ok();
        unsafe { std::env::remove_var("MARSPOT_STATE_DIR") };
    }

    /// A claude inside a claude: both records name the outer pid, and
    /// the outer pane must take the one whose hook ran nearest to it.
    #[test]
    fn the_nearer_claude_in_the_chain_wins() {
        let state = std::env::temp_dir().join(format!("marspot-push-near-{}", std::process::id()));
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", &state) };
        let dir = model_push_dir();
        fs::create_dir_all(&dir).unwrap();
        // The outer claude is 4242.  Its own hook sits one hop from
        // it; a subagent's hook sits three, through its own claude.
        fs::write(dir.join("u-outer"), "opus-5\n/t/u-outer.jsonl\nhigh\npids=5000,4242,1\n")
            .unwrap();
        fs::write(dir.join("u-inner"), "fable-5\n/t/u-inner.jsonl\nhigh\npids=6000,5900,4242,1\n")
            .unwrap();

        let got = pushed_session_for_pid(4242).expect("matched");
        assert_eq!(got.0, "u-outer", "the nearer hook is this pane's own");
        fs::remove_dir_all(&state).ok();
        unsafe { std::env::remove_var("MARSPOT_STATE_DIR") };
    }

    /// The chain the hook writes starts at its own parent and walks
    /// up — claude is somewhere above it, however many shells deep it
    /// is invoked.
    #[test]
    fn the_recorded_chain_starts_at_this_process_parent() {
        let chain = ancestor_pids(8);
        assert!(!chain.is_empty(), "a process always has a parent");
        assert_eq!(chain[0], unsafe { libc::getppid() });
        assert!(chain.iter().all(|&p| p > 1), "{chain:?}");
        assert!(chain.len() <= 8, "{chain:?}");
    }

    /// Everything a `PtyOp` types into the pane, joined.
    fn sent_text(op: &pty_op::PtyOp) -> String {
        op.steps
            .iter()
            .filter_map(|s| match &s.kind {
                pty_op::StepKind::Send(b) => Some(String::from_utf8_lossy(b).into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// A session the hook has named but claude has not written a
    /// transcript for: nothing to resume, so the cycle starts claude
    /// plain rather than refusing.
    ///
    /// Reported 2026-09-16 — entering cc and clicking the badge did
    /// nothing, twice, and kept doing nothing until the session had
    /// answered something.
    #[test]
    fn a_session_with_no_turn_yet_cycles_by_starting_fresh() {
        let op = profile_cycle_op(true, "u-1", false, 3, 4242, None, 4200, None, &CliChoice::default())
            .expect("a named session with no transcript can still cycle");
        let sent = sent_text(&op);
        assert!(
            sent.contains(".claude-profile-3"),
            "must still switch profile: {sent:?}"
        );
        assert!(
            !sent.contains("--resume"),
            "nothing to resume, and --resume on a uuid claude never wrote fails: {sent:?}"
        );

        // The counter-case: once the transcript exists, the session
        // travels with the switch.
        let op = profile_cycle_op(true, "u-1", true, 3, 4242, None, 4200, None, &CliChoice::default()).expect("normal path");
        let sent = sent_text(&op);
        assert!(sent.contains("--resume"), "{sent:?}");
        assert!(sent.contains("u-1"), "{sent:?}");
    }

    /// …and the idle policy stops before signalling such a pane, with
    /// a reason, rather than building a broken resume line.
    #[test]
    fn the_idle_policy_leaves_an_unbound_pane_running() {
        let dir = std::env::temp_dir().join(format!(
            "marspot-hib-{}-{}",
            std::process::id(),
            line!()
        ));
        fs::create_dir_all(&dir).unwrap();
        let mut child = spawn_fake_claude(&dir);
        let pid = child.id() as i32;
        let host = FakeHost::new(dir.clone());
        host.set_status(7, idle_view(Duration::from_secs(7200)));

        let mut plugin = ClaudecodePlugin::new();
        plugin.shelld = Some(Arc::new(ShelldClient::new(None)));

        // Two passes — the second is the one that would decide.
        let mut scan = scan_with(7, pid, "");
        scan.new_cpu.insert(7, (1_000, SystemTime::now() - Duration::from_secs(120)));
        plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
        let scan = scan_with(7, pid, "");
        plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
        host.pump_ops();

        assert!(host.begun.lock().unwrap().is_empty(), "nothing was reclaimed");
        assert!(plugin.dormant.is_empty());
        assert_eq!(
            plugin.blocked_reason.get(&7).map(String::as_str),
            Some("unknown_session"),
            "and it says why"
        );
        assert!(
            !matches!(child.try_wait(), Ok(Some(_))),
            "the process is left alone"
        );
        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_dir_all(&dir);
    }

    /// 2026-08-03 report: the reclamation is visible.
    ///
    /// The picture stands still, but killing claude drops the pane out
    /// of the scan's mapping — and the mapping is what clears badges.
    /// A parked pane blanked its corner and then wore `zZ`, which is
    /// the reclamation announcing itself on a pane whose whole point is
    /// that nothing about it moved.
    #[test]
    fn a_held_pane_keeps_the_badge_it_was_frozen_with() {
        let dir = std::env::temp_dir()
            .join(format!("marspot-look-{}-{}", std::process::id(), line!()));
        fs::create_dir_all(&dir).unwrap();
        let host = FakeHost::new(dir.clone());
        let mut plugin = ClaudecodePlugin::new();

        // Two panes wore a badge last tick; one of them is now held.
        plugin.last_mapping.insert(7, "P2@opus-5".to_string());
        plugin.last_mapping.insert(8, "P1@fable-5".to_string());
        plugin.held.insert(
            7,
            FrozenLook { badge: "P2@opus-5".into(), title: "proj".into() },
        );

        // This scan sees neither: pane 7 because we killed its claude,
        // pane 8 because the user quit it themselves.
        plugin.publish_looks(&host, &empty_scan());

        let badges = host.pane_badges.lock().unwrap().clone();
        assert!(
            badges.contains(&(7, "P2@opus-5".to_string())),
            "the held pane keeps exactly what it was frozen with: {badges:?}"
        );
        assert!(
            !badges.iter().any(|(sid, text)| *sid == 7 && text.is_empty()),
            "and is never blanked: {badges:?}"
        );
        assert!(
            badges.contains(&(8, String::new())),
            "a pane that lost claude on its own still clears: {badges:?}"
        );
        assert!(
            host.pane_titles
                .lock()
                .unwrap()
                .contains(&(7, "proj".to_string())),
            "its title is frozen too"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    fn empty_scan() -> ScanResult {
        ScanResult {
            new_mapping: HashMap::new(),
            new_models: HashMap::new(),
            new_meta: HashMap::new(),
            new_activity: HashMap::new(),
            new_cpu: HashMap::new(),
            new_vetoes: HashMap::new(),
            scanned_at: SystemTime::now(),
            sessions_seen: Vec::new(),
            log_lines: Vec::new(),
        }
    }

    /// The whole reclamation path, with a real process on the other
    /// end of the signal: first pass only samples CPU, second pass
    /// decides, signals, and records the pane as dormant.
    #[test]
    fn idle_policy_reclaims_a_real_process_and_records_it_dormant() {
        let dir = std::env::temp_dir().join(format!(
            "marspot-hib-{}-{}",
            std::process::id(),
            line!()
        ));
        fs::create_dir_all(&dir).unwrap();
        let mut child = spawn_fake_claude(&dir);
        let pid = child.id() as i32;
        let host = FakeHost::new(dir.clone());
        host.set_status(7, idle_view(Duration::from_secs(7200)));

        let mut plugin = ClaudecodePlugin::new();
        plugin.shelld = Some(Arc::new(ShelldClient::new(None)));
        let uuid = "aaaa-bbbb";

        // Pass 1: no previous CPU sample, so nothing may be decided.
        let mut scan = scan_with(7, pid, uuid);
        scan.new_cpu.insert(7, (1_000, SystemTime::now() - Duration::from_secs(120)));
        plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
        assert!(
            host.begun.lock().unwrap().is_empty(),
            "the first CPU sample cannot decide anything"
        );
        assert!(plugin.dormant.is_empty());

        // Pass 2: same CPU total, sample window wide enough.
        let scan = scan_with(7, pid, uuid);
        plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
        assert!(
            host.begun.lock().unwrap().is_empty(),
            "the decision queues the op; starting it is the service's job"
        );
        // …which happens on the next tick, exactly as in production.
        host.pump_ops();
        assert_eq!(*host.begun.lock().unwrap(), vec![7], "a wake session is armed");
        assert_eq!(plugin.dormant.len(), 1);
        assert_eq!(plugin.dormant[0].uuid, uuid);

        // Nothing has been signalled yet: the session starts in
        // `Holding`, so the picture is safely held before the process
        // is touched.  A kill here would race the hold across two
        // process boundaries and let claude's parting message reach
        // the screen.
        assert!(
            !matches!(child.try_wait(), Ok(Some(_))),
            "the signal must wait for the hold to land"
        );

        // Driving the session's ticks is what sends it — the real host
        // does this from its own loop.
        let session_host = FakePaneSessionHost { sid: 7 };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut exited = false;
        while std::time::Instant::now() < deadline {
            for s in host.sessions.lock().unwrap().iter_mut() {
                s.on_tick(&session_host);
            }
            if matches!(child.try_wait(), Ok(Some(_))) {
                exited = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        assert!(exited, "the fake claude should have been signalled");

        // …and it survives a restart: the record is on disk.
        let text = fs::read_to_string(dir.join("dormant.tsv")).unwrap();
        assert_eq!(decode_dormant(&text).len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The CPU baseline must survive across scans, or the delta always
    /// spans one scan (~2 s) and the "sample must span real time" gate
    /// can never pass.  That shipped: a pane past the one-hour
    /// threshold sat logging `cpu sample spans only 2s`, meaning
    /// reclamation would never have fired for anyone.
    #[test]
    fn the_cpu_baseline_is_held_across_scans_not_replaced_every_time() {
        let dir = std::env::temp_dir().join(format!(
            "marspot-hib-{}-{}",
            std::process::id(),
            line!()
        ));
        fs::create_dir_all(&dir).unwrap();
        let host = FakeHost::new(dir.clone());
        // Below the threshold, so the policy keeps looking rather than
        // reclaiming — which is when the baseline handling matters.
        host.set_status(7, idle_view(Duration::from_secs(10)));
        let mut plugin = ClaudecodePlugin::new();
        plugin.shelld = Some(Arc::new(ShelldClient::new(None)));

        let t0 = SystemTime::now() - Duration::from_secs(120);
        // Recent transcript: the session did something a moment ago,
        // so the policy keeps looking rather than reclaiming — which is
        // when the baseline handling is what's under test.
        let recent = |mut scan: ScanResult| -> ScanResult {
            if let Some(m) = scan.new_meta.get_mut(&7) {
                m.transcript_at = SystemTime::now();
            }
            scan
        };
        let mut scan = recent(scan_with(7, std::process::id() as i32, "u"));
        scan.new_cpu.insert(7, (1_000, t0));
        plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
        assert_eq!(plugin.cpu_samples.get(&7).map(|(c, _)| *c), Some(1_000));

        // A scan two seconds later must NOT move the baseline.
        let mut scan = recent(scan_with(7, std::process::id() as i32, "u"));
        scan.new_cpu.insert(7, (2_000, t0 + Duration::from_secs(2)));
        plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
        assert_eq!(
            plugin.cpu_samples.get(&7).map(|(c, _)| *c),
            Some(1_000),
            "a 2s-old baseline must be kept, or the delta measures nothing"
        );

        // Past the window, it rolls forward.
        let mut scan = recent(scan_with(7, std::process::id() as i32, "u"));
        scan.new_cpu.insert(7, (3_000, t0 + CPU_BASELINE_WINDOW + Duration::from_secs(1)));
        plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
        assert_eq!(
            plugin.cpu_samples.get(&7).map(|(c, _)| *c),
            Some(3_000),
            "past the window the baseline should move"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The pid guard: `last_meta` can name a pid that has since been
    /// recycled into something else, and signalling that would hit an
    /// unrelated process.
    #[test]
    fn idle_policy_refuses_to_signal_a_pid_that_is_no_longer_claude() {
        let dir = std::env::temp_dir().join(format!(
            "marspot-hib-{}-{}",
            std::process::id(),
            line!()
        ));
        fs::create_dir_all(&dir).unwrap();
        // A live process that is emphatically not claude: this test
        // process itself.
        let host = FakeHost::new(dir.clone());
        host.set_status(7, idle_view(Duration::from_secs(7200)));
        let mut plugin = ClaudecodePlugin::new();
        plugin.shelld = Some(Arc::new(ShelldClient::new(None)));
        let me = std::process::id() as i32;

        let mut scan = scan_with(7, me, "uuid-1");
        scan.new_cpu.insert(7, (1_000, SystemTime::now() - Duration::from_secs(120)));
        plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
        let scan = scan_with(7, me, "uuid-1");
        plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);

        assert!(
            host.begun.lock().unwrap().is_empty(),
            "nothing may be armed against a pid that isn't claude"
        );
        assert!(plugin.dormant.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A record must not be judged by a scan older than itself.
    ///
    /// The scan that triggers a reclamation was taken while claude was
    /// still alive, so it still holds a binding for that pane — judging
    /// the fresh record by it reads as "claude is back" and drops it on
    /// the spot.  That shipped: three sessions were reclaimed on the
    /// real machine at 10:14 and `dormant.tsv` came out **empty**, so
    /// the wake path would not have survived the next restart.
    #[test]
    fn a_record_is_not_dropped_by_the_scan_that_created_it() {
        let dir = std::env::temp_dir().join(format!(
            "marspot-hib-{}-{}",
            std::process::id(),
            line!()
        ));
        fs::create_dir_all(&dir).unwrap();
        let host = FakeHost::new(dir.clone());
        let mut plugin = ClaudecodePlugin::new();
        plugin.shelld = Some(Arc::new(ShelldClient::new(None)));

        // The scan that will trigger the reclamation: taken while the
        // binding was still live.
        let mut scan = scan_with(7, 1, "u7");
        scan.new_mapping.insert(7, "P1 u7".into());
        // …and the record created just after it.
        plugin.dormant.push(DormantRecord {
            shelld_sid: 7,
            uuid: "u7".into(),
            profile_num: 1,
                config_dir: None,
                project_dir: None,
            created_at: scan.scanned_at + Duration::from_millis(1),
        });

        plugin.rearm_dormant(&host, &scan);
        assert_eq!(
            plugin.dormant.len(),
            1,
            "the scan predates the record and says nothing about it"
        );

        // Nor may the scans taken while the kill is still landing.
        // This is the one that shipped: claude takes longer than a
        // tick to flush and exit, those scans still hold its binding,
        // and reading that as "claude is back" emptied `dormant.tsv`
        // seconds after the reclamation wrote it.
        let mut during_kill = scan_with(7, 1, "u7");
        during_kill.new_mapping.insert(7, "P1 u7".into());
        during_kill.scanned_at = plugin.dormant[0].created_at + Duration::from_secs(2);
        plugin.rearm_dormant(&host, &during_kill);
        assert_eq!(
            plugin.dormant.len(),
            1,
            "claude is still exiting; its lingering binding proves nothing"
        );

        // Past the grace window, a binding does mean claude came back —
        // and then the record goes.
        let mut later = scan_with(7, 1, "u7");
        later.new_mapping.insert(7, "P1 u7".into());
        later.scanned_at =
            plugin.dormant[0].created_at + KILL_GRACE + Duration::from_secs(1);
        plugin.rearm_dormant(&host, &later);
        assert!(plugin.dormant.is_empty(), "a later scan may judge it");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A pane that already has a live claude must never get a wake
    /// session armed on it.
    ///
    /// The scan's binding can lag a restart by a pass or two, so
    /// "no binding" is not "no claude".  Arm on that and the next
    /// focus types `claude --resume …` into the running claude's own
    /// prompt, where it sits waiting for Enter — the exact shape of
    /// the bug the user reported.  This test uses THIS process as the
    /// pane's shell: it is definitely alive and definitely has no
    /// claude under it, so the guard's positive path is the one under
    /// test, and the negative path is covered by the guard reading the
    /// same proc table everything else does.
    #[test]
    fn rearm_asks_the_kernel_whether_the_pane_is_really_empty() {
        let dir = std::env::temp_dir().join(format!(
            "marspot-hib-{}-{}",
            std::process::id(),
            line!()
        ));
        fs::create_dir_all(&dir).unwrap();
        let host = FakeHost::new(dir.clone());
        let mut plugin = ClaudecodePlugin::new();
        plugin.shelld = Some(Arc::new(ShelldClient::new(None)));
        // A session id the registry has never heard of: `shell_pid_for`
        // returns 0, which the guard reads as "cannot tell" and
        // refuses — an unknown pane is not an empty one.
        plugin.dormant = vec![DormantRecord {
            shelld_sid: 999_999,
            uuid: "u".into(),
            profile_num: 1,
            config_dir: None,
            project_dir: None,
            created_at: std::time::UNIX_EPOCH,
        }];
        let mut scan = scan_with(999_999, 1, "u");
        scan.sessions_seen = vec![999_999];
        scan.new_mapping.clear();
        plugin.rearm_dormant(&host, &scan);
        assert!(
            host.begun.lock().unwrap().is_empty(),
            "a pane we cannot inspect must not get a wake session"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A pane whose claude came back (woken, or restarted by hand)
    /// stops being dormant, which is what keeps the set bounded.
    ///
    /// Retention only — whether a still-dormant pane gets a wake
    /// session armed depends on a kernel check that a unit test has no
    /// pane to satisfy; that half is covered by
    /// `rearm_asks_the_kernel_whether_the_pane_is_really_empty` and by
    /// the full-loop PTY test.
    #[test]
    fn rearm_drops_records_for_panes_whose_claude_is_back() {
        let dir = std::env::temp_dir().join(format!(
            "marspot-hib-{}-{}",
            std::process::id(),
            line!()
        ));
        fs::create_dir_all(&dir).unwrap();
        let host = FakeHost::new(dir.clone());
        let mut plugin = ClaudecodePlugin::new();
        plugin.shelld = Some(Arc::new(ShelldClient::new(None)));
        plugin.dormant = vec![
            DormantRecord {
                shelld_sid: 7,
                uuid: "u7".into(),
                profile_num: 1,
                config_dir: None,
                project_dir: None,
                created_at: std::time::UNIX_EPOCH,
            },
            DormantRecord {
                shelld_sid: 8,
                uuid: "u8".into(),
                profile_num: 1,
                config_dir: None,
                project_dir: None,
                created_at: std::time::UNIX_EPOCH,
            },
        ];

        let mut scan = scan_with(7, 1, "u7");
        scan.sessions_seen = vec![7, 8];
        // Pane 7 has a binding again → its claude is back → the record
        // goes.  Pane 8 is still live and still without one → it stays.
        scan.new_mapping.insert(7, "badge".into());
        plugin.rearm_dormant(&host, &scan);

        assert_eq!(plugin.dormant.len(), 1);
        assert_eq!(plugin.dormant[0].shelld_sid, 8);

        // A pane that vanished from the registry drops out entirely.
        let mut gone = scan_with(9, 1, "u9");
        gone.sessions_seen = vec![9];
        plugin.rearm_dormant(&host, &gone);
        assert!(plugin.dormant.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    // ── idle reclamation policy ───────────────────────────────────
    // Every one of these is about NOT killing something.  The upside
    // of hibernating is memory; the downside is a lost turn, a lost
    // pending tool call, or a lost background task — so the tests are
    // written from the veto side.

    const HOUR: Duration = Duration::from_secs(3600);
    const HALF_HOUR: Duration = Duration::from_secs(1800);

    /// Evidence for a session that has been idle `secs` seconds — by
    /// its own transcript, which is the clock that decides.
    fn idle_for(secs: u64) -> IdleEvidence {
        IdleEvidence {
            idle_for: Duration::from_secs(secs),
            quiescent: true,
            awaiting_user: true,
            held: Duration::from_secs(secs),
            cpu_delta_ns: 0,
            since_sample: Duration::from_secs(120),
            user_here: false,
            work_in_flight: false,
            own_timer: false,
        }
    }

    /// A pane this plugin has parked reports `Dormant`, not `Absent`.
    /// Both look the same from outside (no claude, shell at a prompt);
    /// only one of them owes a restore, and the difference has to be
    /// visible to anyone reading the pane's state.
    #[test]
    fn an_unbound_pane_reports_dormant_only_when_something_is_parked() {
        let parked = vec![DormantRecord {
            shelld_sid: 7,
            uuid: "u".into(),
            profile_num: 1,
                config_dir: None,
                project_dir: None,
            created_at: std::time::UNIX_EPOCH,
        }];
        assert_eq!(activity_for_unbound(7, &parked), CcActivity::Dormant);
        assert_eq!(activity_for_unbound(8, &parked), CcActivity::Absent);
        assert_eq!(activity_for_unbound(7, &[]), CcActivity::Absent);
        // …and the composed state keeps the two apart.
        assert!(
            marspot::pane_state::compose(
                &marspot::pane_state::Generic::Idle,
                CcActivity::Dormant,
                true,
            )
            .owes_restore()
        );
        assert!(
            !marspot::pane_state::compose(
                &marspot::pane_state::Generic::Idle,
                CcActivity::Absent,
                true,
            )
            .owes_restore()
        );
    }

    /// The threshold is the user's, so it comes from the settings
    /// file — and the env var still overrides it, because the sandbox
    /// scripts and soak tests set that and mean "this run", not "what
    /// the user wants".
    #[test]
    fn the_reclaim_threshold_follows_the_settings_file() {
        // SAFETY: nextest runs one test per process.
        unsafe { std::env::remove_var("MARSPOT_CC_IDLE_HIBERNATE_S") };

        marspot::settings::set_for_test(marspot::settings::Settings {
            reclaim_enabled: true,
            reclaim_idle_minutes: 45,
            reclaim_prefetch: true,
            ..marspot::settings::Settings::default()
        });
        assert_eq!(hibernate_after(), Some(Duration::from_secs(45 * 60)));

        // Both ways of saying never.
        marspot::settings::set_for_test(marspot::settings::Settings {
            reclaim_enabled: false,
            ..marspot::settings::Settings::default()
        });
        assert_eq!(hibernate_after(), None, "the switch");
        marspot::settings::set_for_test(marspot::settings::Settings {
            reclaim_idle_minutes: 0,
            ..marspot::settings::Settings::default()
        });
        assert_eq!(hibernate_after(), None, "the zero");

        // And the env var wins over any of it.
        // SAFETY: as above.
        unsafe { std::env::set_var("MARSPOT_CC_IDLE_HIBERNATE_S", "60") };
        assert_eq!(hibernate_after(), Some(Duration::from_secs(60)));
        unsafe { std::env::remove_var("MARSPOT_CC_IDLE_HIBERNATE_S") };
    }

    /// 2026-08-03 report: come back to marspot after a short break and
    /// the first pane you touch takes seconds before it answers.
    ///
    /// It had been reclaimed.  The clock that decides is the session's
    /// transcript age, and that clock knows nothing about where the
    /// user is — so the pane they were sitting in, reading the last
    /// answer, was parked out from under them and the next keystroke
    /// paid for a wake.  Invisibly, since a reclamation is deliberately
    /// silent: it reads as marspot simply going unresponsive.
    #[test]
    fn the_pane_the_user_is_sitting_in_is_never_reclaimed() {
        // Idle for two hours by its own clock, and eligible on every
        // other count.
        let e = IdleEvidence { user_here: true, ..idle_for(7200) };
        assert!(!should_hibernate(e, HOUR));
        assert_eq!(
            blocking_reason(e, HOUR).map(|(cat, _)| cat),
            Some("user_here"),
            "and it says so, rather than looking like a policy that never fires"
        );

        // The same pane, once they move to another one.
        let gone = IdleEvidence { user_here: false, ..e };
        assert!(should_hibernate(gone, HOUR));
    }

    /// The reclamation gate excludes an already-parked pane on the
    /// same evidence it uses for everything else — no private "have I
    /// done this one" flag.
    #[test]
    fn a_dormant_pane_is_not_a_reclamation_candidate() {
        let e = IdleEvidence {
            quiescent: true,
            // `Dormant` is quiet but is not `AwaitingUser`.
            awaiting_user: false,
            held: Duration::from_secs(86_400),
            idle_for: Duration::from_secs(86_400),
            cpu_delta_ns: 0,
            since_sample: Duration::from_secs(300),
            user_here: false,
            work_in_flight: false,
            own_timer: false,
        };
        assert!(!should_hibernate(e, HOUR));
    }

    /// The clock that decides is the session's, not the terminal's.
    ///
    /// claude prints `Checking for updates` into the corner every
    /// thirty minutes.  Those few dozen bytes make the pane busy for
    /// half a minute, which resets the terminal's quiet clock — and
    /// with a thirty-minute threshold that is a race the update check
    /// always wins.  Measured on this machine: panes idle for eleven
    /// hours, `held_s=1767` at every reset (thirty-three seconds
    /// short), 23 update checks in one pane's log, zero reclamations.
    ///
    /// The transcript does not move for chrome, so it is what counts.
    #[test]
    fn chrome_that_keeps_the_terminal_busy_does_not_stop_reclamation() {
        let half_hour = Duration::from_secs(1800);
        // Exactly the observed shape: the session has done nothing for
        // eleven hours, but the pane went quiet again only moments ago
        // because the update check just fired.
        let e = IdleEvidence {
            idle_for: Duration::from_secs(11 * 3600),
            held: Duration::from_secs(1),
            ..idle_for(0)
        };
        assert!(
            should_hibernate(e, half_hour),
            "eleven idle hours is idle, whatever the terminal was doing"
        );

        // And the converse still holds: a session that has been
        // working is not reclaimed just because its pane is quiet
        // while it thinks.
        let e = IdleEvidence {
            idle_for: Duration::from_secs(60),
            held: Duration::from_secs(11 * 3600),
            ..idle_for(0)
        };
        assert!(!should_hibernate(e, half_hour));
    }

    /// The log has to be able to say why a candidate is still waiting.
    /// "Nothing happened" reads the same whether the policy is patient
    /// or broken, and only one of those is fine.
    #[test]
    fn a_waiting_candidate_can_say_what_it_is_waiting_on() {
        // Ready to go → nothing to explain.
        assert_eq!(blocking_reason(idle_for(7200), HOUR), None);

        // Still accruing idle time.
        let (cat, line) = blocking_reason(idle_for(1800), HOUR).expect("a reason");
        assert_eq!(cat, "below_threshold");
        assert!(line.contains("1800s of 3600s"), "got {line:?}");

        // The category must NOT move with the number, or "log on
        // change" logs on every scan — which is exactly what happened
        // in production: 3030 lines in 25 minutes.
        let (later, _) = blocking_reason(idle_for(1801), HOUR).expect("a reason");
        assert_eq!(later, cat, "the dedup key must be value-free");

        // Past the threshold, but the CPU window is too short to mean
        // anything yet.
        let e = IdleEvidence {
            since_sample: Duration::from_secs(5),
            ..idle_for(7200)
        };
        let (cat, line) = blocking_reason(e, HOUR).expect("a reason");
        assert_eq!(cat, "short_cpu_sample");
        assert!(line.contains("spans only 5s"), "got {line:?}");

        // Past the threshold, window fine, but something is running.
        let e = IdleEvidence {
            cpu_delta_ns: 900_000_000,
            ..idle_for(7200)
        };
        let (cat, line) = blocking_reason(e, HOUR).expect("a reason");
        assert_eq!(cat, "cpu_busy");
        assert!(line.contains("900ms of cpu"), "got {line:?}");

        // Not a candidate at all → silence, not noise.  Every busy pane
        // in the window would otherwise explain itself once a second.
        let e = IdleEvidence { awaiting_user: false, ..idle_for(7200) };
        assert_eq!(blocking_reason(e, HOUR), None);
        let e = IdleEvidence { quiescent: false, ..idle_for(7200) };
        assert_eq!(blocking_reason(e, HOUR), None);
    }

    /// A session with a process of its own running is not resting,
    /// however quiet it looks.  Measured on a live pane:
    /// `zsh → cargo-fuzz + tail`, all at 0.0 % CPU and no transcript
    /// activity — the clock and the CPU gate both say "idle", and
    /// reclaiming would kill the build.
    #[test]
    fn a_session_running_something_of_its_own_is_never_reclaimed() {
        let e = IdleEvidence { work_in_flight: true, ..idle_for(86_400) };
        assert!(!should_hibernate(e, HALF_HOUR));
        let (cat, _) = blocking_reason(e, HALF_HOUR).expect("a reason");
        assert_eq!(cat, "work_in_flight");
    }

    /// A `/loop` autorun waits on a timer inside claude: no processes,
    /// no transcript, and a pane that looks exactly like one waiting
    /// for a person.  Reclaiming it ends the run.
    #[test]
    fn a_session_waiting_on_its_own_timer_is_never_reclaimed() {
        let e = IdleEvidence { own_timer: true, ..idle_for(86_400) };
        assert!(!should_hibernate(e, HALF_HOUR));
        let (cat, _) = blocking_reason(e, HALF_HOUR).expect("a reason");
        assert_eq!(cat, "own_timer");
    }

    /// The traces those two leave, as they appear in a transcript and
    /// a process table.
    #[test]
    fn the_two_vetoes_recognise_what_they_are_looking_for() {
        assert!(tail_mentions_own_timer(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"ScheduleWakeup"}]}}"#
        ));
        assert!(tail_mentions_own_timer("… <<autonomous-loop-dynamic>> …"));
        assert!(!tail_mentions_own_timer(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#
        ));

        let row = |pid: i32, ppid: i32, comm: &str| pidtree::ProcRow {
            pid,
            ppid,
            comm: comm.into(),
            start_unix: 0,
            pgid: pid,
            tty_dev: 1,
            tty_fg_pgid: 1,
            status: libc::SSLEEP,
        };
        let claude = 100;
        // A resting session: an MCP server, a language server with its
        // own helper, caffeinate, and an empty shell.
        let resting = vec![
            row(claude, 1, "claude"),
            row(101, claude, "smix-mcp"),
            row(102, claude, "rust-analyzer"),
            row(103, 102, "rust-analyzer-proc-macro-srv"),
            row(104, claude, "caffeinate"),
            row(105, claude, "zsh"),
        ];
        assert!(!has_work_in_flight(claude, &resting), "none of that is work");

        // The same session with a job under its shell.
        let mut working = resting.clone();
        working.push(row(106, 105, "cargo-fuzz"));
        assert!(has_work_in_flight(claude, &working), "a job under the shell is work");

        // An unrecognised helper counts as work: the allowlist fails
        // towards not reclaiming, never towards killing something.
        let mut unknown = resting.clone();
        unknown.push(row(107, claude, "some-new-helper"));
        assert!(has_work_in_flight(claude, &unknown));
    }

    #[test]
    fn hibernates_a_pane_that_has_been_quiet_past_the_threshold() {
        assert!(should_hibernate(idle_for(3600), HOUR));
        assert!(should_hibernate(idle_for(7200), HOUR));
    }

    #[test]
    fn refuses_below_the_threshold() {
        assert!(!should_hibernate(idle_for(3599), HOUR));
        assert!(!should_hibernate(idle_for(0), HOUR));
    }

    /// The state machine's own verdict is a veto: without confirmed
    /// quiescence nothing else matters.
    #[test]
    fn refuses_when_the_state_machine_is_not_confident() {
        let e = IdleEvidence { quiescent: false, ..idle_for(7200) };
        assert!(!should_hibernate(e, HOUR));
    }

    /// `Empty` is quiet too — an empty pane with no claude in it.
    /// There is nothing to reclaim, and acting on it would mean
    /// killing whatever the user is about to start.
    #[test]
    fn refuses_a_quiet_pane_that_has_no_claude() {
        let e = IdleEvidence { awaiting_user: false, ..idle_for(7200) };
        assert!(!should_hibernate(e, HOUR));
    }

    /// A silent transcript with a busy subtree is the case a
    /// transcript-only policy would get wrong: a background task, or a
    /// tool that hasn't written its result yet.
    #[test]
    fn refuses_when_the_subtree_is_burning_cpu() {
        let e = IdleEvidence {
            cpu_delta_ns: IDLE_CPU_TOLERANCE_NS + 1,
            ..idle_for(7200)
        };
        assert!(!should_hibernate(e, HOUR));
        // …but an idling MCP server's tick is under the tolerance and
        // must not block reclamation forever.
        let e = IdleEvidence {
            cpu_delta_ns: IDLE_CPU_TOLERANCE_NS / 2,
            ..idle_for(7200)
        };
        assert!(should_hibernate(e, HOUR));
    }

    /// A CPU delta measured across no time says nothing.  This is the
    /// state right after a restart, when the first sample has just
    /// been taken.
    #[test]
    fn refuses_on_a_cpu_sample_that_spans_no_time() {
        let e = IdleEvidence { since_sample: Duration::from_secs(1), ..idle_for(7200) };
        assert!(!should_hibernate(e, HOUR));
    }

    /// The resume line goes into a shell verbatim, so the profile
    /// number has to produce a binary that exists — `claude255` was
    /// what a naive format produced for "no profile".
    /// The profile decides which config dir — which account — the
    /// The reclamation types the session back under the profile it was
    /// running.
    ///
    /// `claudeN` is an interactive alias in the user's rc file
    /// (`alias claude1='CLAUDE_CONFIG_DIR=~/.claude-profile-1 claude'`),
    /// so reproducing the alias would depend on that file still defining
    /// it; setting the variable is the same thing without the
    /// dependency.  Read off the script itself — that is what runs.
    #[test]
    fn the_reclamation_resumes_under_the_profile_it_was_running() {
        let line = |dir: Option<&str>| -> String {
            let op = reclaim_op("abc-123", dir, 1, 2, None).expect("script builds");
            let bytes = op
                .steps
                .iter()
                .find_map(|s| match &s.kind {
                    pty_op::StepKind::Send(b) => Some(b.clone()),
                    _ => None,
                })
                .expect("the script types something");
            String::from_utf8(bytes).unwrap()
        };
        // Asserted in pieces rather than as one string: the line now
        // carries a `--settings` document naming this binary by its
        // absolute path, which differs between a dev build, the
        // bundle, and `binaries/current`. Pinning the whole line would
        // go red on where it was built rather than on what it does.
        let with_dir = line(Some("/Users/x/.claude-profile-3"));
        assert!(
            with_dir.starts_with(
                "printf '\\033[H\\033[2J'; CLAUDE_CONFIG_DIR='/Users/x/.claude-profile-3' claude "
            ),
            "{with_dir}"
        );
        assert!(with_dir.ends_with(" --resume abc-123\r"), "{with_dir}");

        // No dir observed = the default profile's own entry point.
        let no_dir = line(None);
        assert!(no_dir.starts_with("printf '\\033[H\\033[2J'; claude "), "{no_dir}");
        assert!(no_dir.ends_with(" --resume abc-123\r"), "{no_dir}");
    }

    /// The dir lands inside single quotes on a real command line, so a
    /// A profile that cannot be quoted stops the whole reclamation.
    ///
    /// The old code fell back to a plain `claude --resume`, which brings
    /// the session back under a *different account* — worse than leaving
    /// it running.  Now the command refuses to build and the script is
    /// never created.
    #[test]
    fn a_config_dir_that_could_break_out_of_quoting_blocks_the_whole_op() {
        for bad in [
            "/tmp/a'; rm -rf ~; '",
            "/tmp/a\nclaude --dangerously",
            "/tmp/$(whoami)",
            "/tmp/`id`",
            "",
        ] {
            assert!(
                reclaim_op("u", Some(bad), 1, 2, None).is_none(),
                "{bad:?} should stop the reclamation"
            );
        }
        assert!(reclaim_op("u", Some("/home/u/.claude-profile-1"), 1, 2, None).is_some());
    }

    /// A session whose profile could not be read must not be
    /// reclaimed at all: there is no resume line that is known to put
    /// it back where it was, and guessing means a different account.
    #[test]
    fn an_unreadable_profile_blocks_reclamation() {
        let dir = std::env::temp_dir().join(format!(
            "marspot-hib-{}-{}",
            std::process::id(),
            line!()
        ));
        fs::create_dir_all(&dir).unwrap();
        let host = FakeHost::new(dir.clone());
        host.set_status(7, idle_view(Duration::from_secs(7200)));
        let mut plugin = ClaudecodePlugin::new();
        plugin.shelld = Some(Arc::new(ShelldClient::new(None)));

        let claude_like = std::process::id() as i32;
        let mut scan = scan_with(7, claude_like, "uuid-1");
        scan.new_meta.get_mut(&7).unwrap().profile_num = PROFILE_UNKNOWN;
        scan.new_cpu.insert(7, (1_000, SystemTime::now() - Duration::from_secs(120)));
        plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
        let mut scan = scan_with(7, claude_like, "uuid-1");
        scan.new_meta.get_mut(&7).unwrap().profile_num = PROFILE_UNKNOWN;
        plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);

        assert!(
            host.begun.lock().unwrap().is_empty(),
            "an unnamed profile must not be reclaimed"
        );
        assert!(plugin.dormant.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dormant_records_round_trip() {
        // Whole seconds: the file stores unix seconds, so a round
        // trip cannot carry sub-second precision.
        let t = std::time::UNIX_EPOCH + Duration::from_secs(1_785_000_000);
        let records = vec![
            DormantRecord {
                shelld_sid: 7,
                uuid: "9cff8661-3275-4dce-8c93-89797bc63f44".into(),
                profile_num: 1,
                config_dir: Some("/Users/x/.claude-profile-1".into()),
                project_dir: None,
                created_at: t,
            },
            DormantRecord {
                shelld_sid: 9,
                uuid: "abc".into(),
                profile_num: 255,
                config_dir: None,
                project_dir: None,
                created_at: t,
            },
        ];
        assert_eq!(decode_dormant(&encode_dormant(&records)), records);
    }

    /// The directory a wake must return to survives the restart with
    /// the rest of the record.
    ///
    /// Without it a re-armed wake resumes wherever the pane's shell is
    /// standing, and `claude --resume` binds the session to that --
    /// which is how a session came back under another project's rules.
    /// A directory may hold a space or a `$` and mean nothing by it;
    /// what it may not hold is the quote that would end the argument.
    #[test]
    fn a_dormant_record_remembers_where_its_session_lives() {
        let t = std::time::UNIX_EPOCH + Duration::from_secs(1_785_000_000);
        let records = vec![DormantRecord {
            shelld_sid: 7,
            uuid: "9cff8661-3275-4dce-8c93-89797bc63f44".into(),
            profile_num: 1,
            config_dir: Some("/Users/x/.claude-profile-1".into()),
            project_dir: Some("/Users/x/My Work/$proj".into()),
            created_at: t,
        }];
        assert_eq!(decode_dormant(&encode_dormant(&records)), records);

        // A row from before the column decodes as "do not know", which
        // is the behaviour it had.
        let older = "2\tgood-uuid-1\t2\t1785000000\t/Users/x/.claude-profile-2\n";
        assert_eq!(decode_dormant(older)[0].project_dir, None);

        // And one that could end the quoting is dropped rather than
        // repaired, like every other field that reaches a command line.
        let hostile = "3\tgood-uuid-2\t2\t1785000000\t\t/tmp/it's here\n";
        assert_eq!(decode_dormant(hostile)[0].project_dir, None);
    }

    /// The uuid ends up inside a shell command line, so a corrupt or
    /// tampered state file must not be able to smuggle anything into
    /// it.  Rejected rows are dropped, not repaired.
    #[test]
    fn dormant_decode_rejects_a_uuid_that_is_not_a_uuid() {
        let bad = "1\tabc; rm -rf ~\t1\n2\t\t1\n3\tok-uuid\tnot_a_number\n";
        assert!(decode_dormant(bad).is_empty());
        // A well-formed row alongside bad ones still survives.
        // A row without the 4th column is an older file: it decodes,
        // with a creation time old enough to be judged normally.
        let mixed = "1\tbad;line\t1\n2\tgood-uuid-1\t2\n";
        assert_eq!(
            decode_dormant(mixed),
            vec![DormantRecord {
                shelld_sid: 2,
                uuid: "good-uuid-1".into(),
                profile_num: 2,
                config_dir: None,
                project_dir: None,
                created_at: std::time::UNIX_EPOCH,
            }]
        );
    }

    /// The banner is the only place a just-opened pane says its model.
    ///
    /// The sample is the real thing off a screenshot (2026-08-11) — a
    /// pane that had been open for a minute with a bare `P1` badge
    /// while `Opus 5` sat on its own first line.
    #[test]
    fn a_startup_banner_names_the_model_before_the_transcript_can() {
        let screen = "\
 devops                                                    P1

   Claude Code v2.1.227
   Opus 5 (1M context) with high effort · Claude Max
   ~/workspace/goliajp/devops

";
        assert_eq!(rendered(parse_banner_model(screen)).as_deref(), Some("opus-5\u{b7}high"));

        // The remark after `·` is a plan, not a model.
        assert_eq!(
            rendered(parse_banner_model("Claude Code v2.0.1\nSonnet 4.5 · Claude Pro\n")).as_deref(),
            Some("sonnet-4-5"),
        );
        // No parenthesised remark, no separator — still just the name.
        assert_eq!(
            rendered(parse_banner_model("Claude Code v9\nFable 5\n")).as_deref(),
            Some("fable-5"),
        );
        // A screen with no banner says nothing rather than guessing —
        // most panes are not claude, and every one of them reaches
        // here while its badge is being decided.
        assert_eq!(parse_banner_model("$ ls -la\ntotal 0\n"), None);
        assert_eq!(parse_banner_model(""), None);
        // A version line with nothing under it must not read the next
        // screenful as a model name.
        assert_eq!(parse_banner_model("Claude Code v1\n\n\n\n"), None);
    }


    /// The banner as claudecode v2.1.232 actually draws it — logo
    /// glyphs sharing the row, and an effort qualifier after the
    /// version.  Both broke it, in opposite directions: the logo made
    /// the whole line non-ASCII and produced *no* model, and without
    /// the logo the qualifier produced `fable-5-with-hig`.
    #[test]
    fn a_banner_with_a_logo_names_the_model_and_its_effort() {
        let with_logo = "  ▛▀▜  Claude Code v2.1.232\n  ▙▄▟  Fable 5 with high effort · Claude Max\n";
        assert_eq!(rendered(parse_banner_model(with_logo)).as_deref(), Some("fable-5·high"));

        let bare = "Claude Code v2.1.232\nFable 5 with high effort · Claude Max\n";
        assert_eq!(rendered(parse_banner_model(bare)).as_deref(), Some("fable-5·high"));

        // The parenthesised form used to work only because dropping
        // everything from `(` also dropped the qualifier.  It has to
        // keep working now that the qualifier is read on purpose —
        // and the qualifier lives past the `(`, so the effort has to
        // come from the whole line, not the trimmed name.
        let paren = "Claude Code v2.1.227\nOpus 5 (1M context) with high effort · Claude Max\n";
        assert_eq!(rendered(parse_banner_model(paren)).as_deref(), Some("opus-5·high"));

        // A two-word family name survives; the version still ends it.
        let two_word = "Claude Code v9\nClaude Opus 5 with xhigh effort · Claude Max\n";
        assert_eq!(
            rendered(parse_banner_model(two_word)).as_deref(),
            Some("claude-opus-5·xhigh")
        );

        // A model with no effort setting says so by not saying it,
        // and the badge draws the model alone rather than inventing
        // a level for it.
        let no_effort = "Claude Code v9\nHaiku 4.5 · Claude Max\n";
        assert_eq!(rendered(parse_banner_model(no_effort)).as_deref(), Some("haiku-4-5"));

        // No model line at all is still None, not a guess.
        assert_eq!(rendered(parse_banner_model("Claude Code v9\n\n\n\n")).as_deref(), None);
    }

    /// `/clear` moves the session on; argv does not.
    ///
    /// 2026-08-11 report: switching profile on `torajs` resumed a
    /// session from hours earlier.  argv said `--resume f7a8a54b`
    /// (where the process started); the transcript actually being
    /// written was `e024458b`, three minutes newer.  Every downstream
    /// use of the binding was therefore aimed at the wrong
    /// conversation — the model in the badge, the reclamation resume
    /// line, and the profile cycle.
    #[test]
    fn a_cleared_session_supersedes_the_one_named_in_argv() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mk = |uuid: &str, mtime_off: u64| SessionInfo {
            session_id: uuid.to_string(),
            project_dir: "-p-torajs".into(),
            jsonl_path: PathBuf::from(format!("/tmp/{uuid}.jsonl")),
            last_mtime: t0 + Duration::from_secs(mtime_off),
            last_size: 1,
            last_message_kind: None,
        };
        let mut ctx = WorkerCtx {
            projects_root: PathBuf::from("/fake"),
            shelld: Arc::new(ShelldClient::new(None)),
            statusline_state: None,
            seen: HashMap::new(),
            model_cutoff: HashMap::new(),
            banner_tried: HashMap::new(),
            last_refusal: HashMap::new(),
            last_model: HashMap::new(),
            last_model_by_pane: HashMap::new(),
        };
        // argv's session, and the one `/clear` started after it.
        ctx.seen.insert(PathBuf::from("/tmp/argv.jsonl"), mk("argv-uuid", 10));
        ctx.seen.insert(PathBuf::from("/tmp/live.jsonl"), mk("live-uuid", 200));
        let none = std::collections::HashSet::new();

        assert_eq!(
            ctx.successor_session("-p-torajs", &none, t0, "argv-uuid").as_deref(),
            Some("live-uuid"),
            "the transcript being written is the session in front of the user"
        );

        // A session that stopped before this claude started belongs to
        // an earlier run and must not be adopted.
        let later_start = t0 + Duration::from_secs(500);
        assert_eq!(
            ctx.successor_session("-p-torajs", &none, later_start, "argv-uuid"),
            None,
            "nothing here has been written since this process started"
        );

        // Already spoken for by another pane — leave it alone.
        let mut taken = std::collections::HashSet::new();
        taken.insert("live-uuid".to_string());
        assert_eq!(
            ctx.successor_session("-p-torajs", &taken, t0, "argv-uuid"),
            None,
        );

        // Two files touched in the same instant at startup must not
        // make the binding flip: within the margin, argv keeps it.
        ctx.seen.insert(PathBuf::from("/tmp/tie.jsonl"), mk("tie-uuid", 12));
        ctx.seen.remove(&PathBuf::from("/tmp/live.jsonl"));
        assert_eq!(
            ctx.successor_session("-p-torajs", &none, t0, "argv-uuid"),
            None,
            "2 s apart is the same instant, not a succession"
        );

        // Another project's session is never a successor.
        let mut other = mk("other-uuid", 900);
        other.project_dir = "-p-elsewhere".into();
        ctx.seen.insert(PathBuf::from("/tmp/other.jsonl"), other);
        assert_eq!(
            ctx.successor_session("-p-torajs", &none, t0, "argv-uuid"),
            None,
        );
    }

    /// A pane on a non-default profile keeps both halves of its badge.
    ///
    /// 2026-08-10 report: `torajs` showed `P3` and never `P3@model`.
    /// The tag comes from the process's `CLAUDE_CONFIG_DIR`, so it was
    /// right; the transcript walk was pinned to `~/.claude/projects`,
    /// so for any profile but the default it found nothing and the
    /// model half was simply absent.  Two answers about the same pane
    /// have to come from the same place.
    #[test]
    fn a_second_profile_is_scanned_under_its_own_config_dir() {
        let base = std::env::temp_dir().join(format!(
            "marspot-cc-profiles-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        // Two profiles, the same project open under each, a different
        // session id in each — exactly the shape on the machine.
        let mk = |profile: &str, uuid: &str| -> PathBuf {
            let dir = base.join(profile).join("projects").join("-p-torajs");
            fs::create_dir_all(&dir).unwrap();
            let p = dir.join(format!("{uuid}.jsonl"));
            fs::write(&p, format!("{{\"sessionId\":\"{uuid}\"}}\n")).unwrap();
            p
        };
        let default_path = mk(".claude", "11111111-1111-1111-1111-111111111111");
        let third_path = mk(".claude-profile-3", "33333333-3333-3333-3333-333333333333");

        let mut ctx = WorkerCtx {
            projects_root: base.join(".claude").join("projects"),
            shelld: Arc::new(ShelldClient::new(None)),
            statusline_state: None,
            seen: HashMap::new(),
            model_cutoff: HashMap::new(),
            banner_tried: HashMap::new(),
            last_refusal: HashMap::new(),
            last_model: HashMap::new(),
            last_model_by_pane: HashMap::new(),
        };
        let mut logs = Vec::new();
        // The default root alone sees only the default profile's file.
        ctx.refresh_seen(
            [(ctx.projects_root.clone(), "-p-torajs")]
                .iter()
                .map(|(r, d)| (r.as_path(), *d))
                .collect::<Vec<_>>()
                .into_iter(),
            &mut logs,
        );
        assert!(ctx.seen.contains_key(&default_path));
        assert!(
            !ctx.seen.contains_key(&third_path),
            "the default root cannot see another profile — that is the bug"
        );

        // Both roots, one pass — which is how the scan calls it: every
        // live pane contributes its own root, and `seen` is pruned to
        // what this pass walked.  The same project under two profiles
        // is two directories and two files, not one walked twice.
        let default_root = base.join(".claude").join("projects");
        let third_root = base.join(".claude-profile-3").join("projects");
        ctx.refresh_seen(
            [
                (default_root.as_path(), "-p-torajs"),
                (third_root.as_path(), "-p-torajs"),
            ]
            .into_iter(),
            &mut logs,
        );
        assert!(
            ctx.seen.contains_key(&third_path),
            "a pane on profile 3 must find its own transcript"
        );
        assert!(
            ctx.seen.contains_key(&default_path),
            "and the default profile's pane keeps its own"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn hibernate_can_be_turned_off_and_tuned() {
        // Pin the settings this test reasons about.
        //
        // `hibernate_after` reads the settings file when the env knob
        // is unset, and with nothing pinned that is the **developer's
        // own** `~/Library/Caches/marspot/settings.toml`.  This test
        // passed until the day its author turned reclamation off in
        // the panel, and then failed on their machine and nobody
        // else's — a test that reports the state of the machine it
        // runs on rather than the state of the code.
        marspot::settings::set_for_test(marspot::settings::Settings::default());
        // Serialised through the env, so keep the assertions in one
        // test rather than racing sibling tests in the same process.
        let key = "MARSPOT_CC_IDLE_HIBERNATE_S";
        let saved = std::env::var(key).ok();
        unsafe {
            std::env::set_var(key, "0");
            assert_eq!(hibernate_after(), None, "0 disables reclamation");
            std::env::set_var(key, "90");
            assert_eq!(hibernate_after(), Some(Duration::from_secs(90)));
            std::env::set_var(key, "nonsense");
            assert_eq!(
                hibernate_after(),
                Some(HALF_HOUR),
                "an unparsable value falls back to the default rather than to off"
            );
            std::env::remove_var(key);
            assert_eq!(hibernate_after(), Some(HALF_HOUR));
            if let Some(v) = saved {
                std::env::set_var(key, v);
            }
        }
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

    /// The labels land in logs on both sides of the report channel,
    /// so pin them — they are what a future reader greps for.
    #[test]
    fn activity_label_strings_are_stable() {
        assert_eq!(CcActivity::AwaitingUser.label(), "awaiting_user");
        assert_eq!(
            CcActivity::ToolPending { executing: true }.label(),
            "tool_executing"
        );
        assert_eq!(
            CcActivity::ToolPending { executing: false }.label(),
            "tool_awaiting_approval"
        );
        assert_eq!(CcActivity::Working.label(), "working");
        assert_eq!(CcActivity::Unknown.label(), "unknown");
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

    /// The record a real refusal makes, copied off disk rather than
    /// imagined: `isApiErrorMessage` with the CLI's own text and the
    /// instant it happened.
    const REFUSAL: &str = concat!(
        r#"{"parentUuid":"8f670e3e","isSidechain":false,"type":"assistant","#,
        r#""isApiErrorMessage":true,"timestamp":"2026-10-01T11:51:52.440Z","#,
        r#""message":{"role":"assistant","content":[{"type":"text","text":"#,
        r#""You've reached your Fable limit. Run /usage-credits to continue "#,
        r#"or switch models with /model."}]}}"#
    );

    /// A refusal is in the tail window at the instant it is written
    /// and gone from it soon after, so it has to be remembered.
    ///
    /// Measured on the real file this was written for: thirty-two
    /// kilobytes of a 21.8 MB transcript reached back six minutes, and
    /// the refusals that stranded the pane were outside it twenty-five
    /// minutes later. A scan that re-derives this every time answers
    /// "no refusal" for a pane that is still shut out -- which is the
    /// same outcome as not having built any of this.
    #[test]
    fn a_refusal_that_has_scrolled_out_of_the_window_is_still_remembered() {
        let dir = std::env::temp_dir().join(format!(
            "marspot-refusal-window-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");

        let mut ctx = ctx_for_test(dir.clone());
        // The moment it happens: the record is the newest line.
        fs::write(&path, format!("{REFUSAL}\n")).unwrap();
        assert_eq!(
            ctx.remember_refusal(&path, &tail_window(&path)).map(|r| r.at),
            Some(1_790_855_512),
            "seen when it is written"
        );

        // And later: the session keeps talking until the record is
        // past the window.
        let filler = format!("{}\n", r#"{"type":"assistant","timestamp":"2026-10-01T12:30:00.000Z"}"#);
        let mut body = format!("{REFUSAL}\n");
        while body.len() < 80 * 1024 {
            body.push_str(&filler);
        }
        fs::write(&path, &body).unwrap();
        let window = tail_window(&path);
        assert_eq!(
            last_quota_refusal(&window),
            None,
            "the fixture has to actually push it out, or this proves nothing"
        );
        assert_eq!(
            ctx.remember_refusal(&path, &window).map(|r| r.at),
            Some(1_790_855_512),
            "but the pane is no less shut out than it was"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refusal_in_the_transcript_is_read_with_its_time() {
        let r = last_quota_refusal(REFUSAL).expect("the record says when");
        assert_eq!(r.at, 1_790_855_512, "2026-10-01T11:51:52Z");
        assert_eq!(r.model, "fable", "and which model it is about");
    }

    /// The newest one wins: a transcript holds every refusal it ever
    /// had, and what matters is whether the account said no *since*
    /// the feed last looked.
    #[test]
    fn the_newest_refusal_is_the_one_that_counts() {
        let older = REFUSAL.replace("11:51:52", "09:18:13");
        let tail = format!("{older}\n{REFUSAL}\n");
        assert_eq!(last_quota_refusal(&tail).map(|r| r.at), Some(1_790_855_512));
    }

    /// Being overloaded is not being out of quota, and moving a pane
    /// for it restarts a session for nothing. The HTTP status and the
    /// error type are shared between the two, so neither is matched.
    /// The transcript follows a session across a switch; the refusal
    /// the old account gave stays in it and is not the new one's.
    #[test]
    fn only_a_refusal_after_this_claude_started_counts() {
        let started = SystemTime::UNIX_EPOCH + Duration::from_secs(1_791_002_529);
        let at = |at| Some(quota::Refusal { at, model: "fable".into() });
        assert_eq!(refused_since(at(1_790_966_543), started), None, "the day before");
        assert_eq!(refused_since(at(1_791_002_600), started), at(1_791_002_600));
        assert_eq!(refused_since(None, started), None);
    }

    #[test]
    fn a_busy_server_is_not_a_spent_account() {
        let overloaded = REFUSAL.replace(
            "You've reached your Fable limit. Run /usage-credits to continue or switch models with /model.",
            "API Error: 429 overloaded_error · Retrying in 8 seconds",
        );
        assert_eq!(
            last_quota_refusal(&overloaded),
            None,
            "a retryable error is not a reason to move the pane"
        );
    }

    /// And a record that is not flagged as an API error does not
    /// count, however it reads -- a transcript that quotes the message
    /// is not a transcript that received it. (This one is written from
    /// a session that was discussing the bug: the text was in four
    /// profiles' transcripts before any of them had been refused.)
    #[test]
    fn quoting_the_message_is_not_receiving_it() {
        let quoted = REFUSAL.replace(r#""isApiErrorMessage":true,"#, "");
        assert_eq!(last_quota_refusal(&quoted), None);
    }

    #[test]
    fn the_credits_wording_counts_too() {
        let credits = REFUSAL.replace(
            "You've reached your Fable limit. Run /usage-credits to continue or switch models with /model.",
            "Opus 5.5 requires usage credits.",
        );
        assert_eq!(last_quota_refusal(&credits).map(|r| r.model), Some("opus-5-5".into()));
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


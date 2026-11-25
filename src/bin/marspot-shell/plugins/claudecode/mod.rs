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
mod idle_rule;
pub(in crate::plugins) mod quota;
pub(in crate::plugins) mod rooms;
mod rebalance;
mod switching;
mod heads_up_due;
mod burn;
mod leave_early;
mod leave_plan;
mod pane_tokens;
pub(in crate::plugins) mod heads_up;
mod refusal;
mod fable_need;
pub(in crate::plugins) mod seen;
mod profile_tag;
mod model;
mod statusline;
mod settings_hook;
mod worker;
mod scan_result;
mod binding;
mod transcript;
mod profiles;
mod argv;
mod records;
mod context;
mod continued;
mod sessions_view;
mod wake;
mod monitors;
mod due;
mod client;
mod api_error;
mod reclaim;
mod looks;
mod dormant;
mod cycle;
mod cycle_op;
mod plugin;
mod bind_meta;
mod allocation_adapter;
mod allocation_decide;
pub(in crate::plugins) mod allocation_events;
mod allocation_inputs;
mod allocation_line;
mod allocation_prices;
mod allocation_usage;
mod allocation_view;
mod alloc_moves;
mod worker_loop;
pub(in crate::plugins) mod with_helpers;

use transcript::{activity_of, has_conversation, parse_session_id, tail_last_message_type};
#[cfg(test)]
use transcript::{tail_activity, tail_window};
pub(crate) use profiles::discover_profiles;
pub(crate) use profiles::{discover_profiles_in, email_in};
use argv::argv_session_uuid;
#[cfg(test)]
use argv::{flag_uuid, is_uuid};
pub use statusline::statusline_ingest;
// the settings-file plumbing the peerbus hooks share with the status line
pub(crate) use settings_hook::{cc_settings_files, sh_quote, write_through_symlink};
use idle::*;
use idle_rule::*;
use model::*;
use statusline::*;
use settings_hook::*;
use worker::*;
use scan_result::*;
use profile_tag::*;
use records::*;
#[cfg(test)]
use refusal::{last_agent_refusal, refused_since};
use super::handoff::resume_word;
use client::ShelldClient;
pub use client::InjectInputProxy;
use monitors::MonitorState;
use cycle_op::*;
use bind_meta::*;
pub(crate) use cycle_op::badge_menu_for;
pub(crate) use api_error::retryable_error_kind;
pub(crate) use sessions_view::{ClaudePane, pane_of_session, sessions};
pub(crate) use idle::is_resting_helper;
#[cfg(test)]
use api_error::strip_ansi;

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};
use crate::plugins::{pidtree, pty_op};
use marspot::ui::strings::{self, t, Arg, Msg};
use crate::plugins::{
    LogLevel, Plugin, PluginError, PluginHost, PluginMetadata,
    PLUGIN_API_VERSION,
};

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
    /// Per pane: `(work running under claude, own timer)`, as the scan
    /// last read it, for a switch someone asks for between scans.
    last_vetoes: HashMap<u64, (bool, bool)>,
    /// The machine's one-minute load, which holds a resting pane's move
    /// on an overloaded machine. A test gives its own.
    host_load: fn() -> Option<f64>,
    /// Panes stranded on an account that is out: how many moves have
    /// been spent on each, and where the last one was aimed.  An entry
    /// disappears when the account answers again.
    stranded: HashMap<u64, Stranding>,
    /// When each pane was last moved off a refused account.
    moved_at: HashMap<u64, Instant>,
    /// Resting panes moved onto a better account: when, and to which profile,
    /// so a move on its way is not made twice nor repeated within `COOLDOWN`.
    rebalanced: HashMap<u64, (Instant, u8)>,
    /// What was last logged about moving each resting pane.
    rebalance_said: HashMap<u64, String>,
    /// Busy panes told their account was nearly out, so each is told once each time its account runs low.
    heads_up: heads_up::Ledger,
    /// Accounts' speeds, and who leaves one running short.
    leave: leave_early::State,
    /// Panes left out of the sweep because their model or effort could not be
    /// confirmed, kept so the reason is logged once, not every pass.
    unjudged: std::collections::HashSet<u64>,
    /// What panes have been told about their accounts.
    refused: seen::Seen,
    /// Panes refused Fable, judged as Fable panes for a while.
    fable_need: fable_need::Marks,
    /// The allocation's view of the last scan, for a badge click.
    alloc_view: Option<allocation_view::View>,
    /// Where moves made by the allocation's choice are written.
    alloc_moved: Option<allocation_events::EventLog>,

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
    /// How each reclaim run ended; see `wake`.
    wake_ends: wake::WakeEnds,
    /// `shelld_session_id → (the claude sampled, its subtree CPU ns,
    /// sampled at)` from the previous scan, so the idle policy can look
    /// at a delta rather than an absolute.  A sample of another process
    /// is no baseline: a claude that replaced the old one is not idle
    /// because its counter started low.
    cpu_samples: HashMap<u64, (pidtree::ProcId, u64, SystemTime)>,
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
    /// What the ticks need while no pane runs claude; see `due`.
    quiet: due::Quiet,
}

impl ClaudecodePlugin {
    pub fn new() -> Self {
        Self {
            initialised: false,
            shelld: None,
            last_mapping: HashMap::new(),
            last_meta: HashMap::new(),
            last_model: HashMap::new(),
            last_vetoes: HashMap::new(),
            host_load: marspot::host_load::load1,
            stranded: HashMap::new(),
            moved_at: HashMap::new(),
            rebalanced: HashMap::new(),
            rebalance_said: HashMap::new(),
            heads_up: heads_up::Ledger::default(),
            leave: Default::default(),
            unjudged: std::collections::HashSet::new(),
            refused: seen::Seen::default(),
            fable_need: fable_need::Marks::default(),
            alloc_view: None,
            alloc_moved: None,
            dormant: Vec::new(),
            armed: std::collections::HashSet::new(),
            held: HashMap::new(),
            wake_ends: Default::default(),
            cpu_samples: HashMap::new(),
            blocked_reason: HashMap::new(),
            focused_sid: None,
            monitors: HashMap::new(),
            monitor_unsupported: false,
            worker: None,
            scan_req_tx: None,
            scan_res_rx: None,
            scan_inflight: false,
            quiet: Default::default(),
        }
    }

}

/// `CLAUDE_CONFIG_DIR` as a path, or the CLI's default when unset.
pub(crate) fn config_dir_or_default(config_dir: Option<&str>) -> PathBuf {
    match config_dir {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude"),
    }
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

/// The claude a pane is running, by the rule in
/// [`Session::agent`](crate::observe::Session::agent): the program in
/// front of the pane, and nothing a program there started.
pub(crate) fn pane_claude(s: &crate::observe::Session) -> Option<&crate::observe::Proc> {
    s.agent(|p| p.name() == "claude")
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use super::refusal::last_quota_refusal;

    mod acting;
    mod refused;
    mod settings_file;
    mod telling;
    mod leaving;
    mod leaving_targets;
    mod heads_up_keep_working;
    mod api_errors;
    mod refusal_text;
    mod activity;
    mod scan_pass;
    mod idle_rules;
    mod dormant_records;
    mod reclaim_runs;
    mod idle_loop;
    mod session_binding;
    mod model_read;
    mod profile_list;
    mod cycle_script;
    mod moves;
    mod core_moves;
    mod held_back;
    mod helpers_switch;
    mod cycle_clicks;
    mod fable_refused;
    mod support;
    use support::*;

    use marspot_term::test_env::{sandbox_state_dir, ChildGuard, EnvGuard, TempDir};

    use super::transcript::{
        activity_from_last_record, is_bookkeeping_record, record_type, top_level_str,
    };

    use std::io::Write;
    use std::path::Path;

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

}

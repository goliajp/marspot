// CARVE-OUT: pre-existing god file: the claudecode plugin; to be split by concern; shrink on every touch
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
pub(in crate::plugins) mod quota;
pub(in crate::plugins) mod rooms;
mod rebalance;
mod switching;
pub(in crate::plugins) mod heads_up;
mod refusal;
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
mod continued;
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
use model::*;
use statusline::*;
use settings_hook::*;
use worker::*;
use scan_result::*;
use profile_tag::*;
use records::*;
use refusal::{last_agent_refusal, refused_since};
use super::handoff::resume_word;
use client::ShelldClient;
pub use client::InjectInputProxy;
use monitors::MonitorState;
use cycle_op::*;
pub(crate) use cycle_op::badge_menu_for;
pub(crate) use api_error::retryable_error_kind;
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

/// Per-session state cached so we only re-log on real change.
#[derive(Clone, Debug)]
struct SessionInfo {
    session_id: String,
    project_dir: String, // encoded form, e.g. -Users-ada-workspace-...
    jsonl_path: PathBuf,
    last_mtime: SystemTime,
    last_size: u64,
    /// Holds at least one user or assistant record, so `--resume` can
    /// open it. See `transcript::has_conversation`.
    has_conversation: bool,
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
    /// Resting panes moved onto a better account: when, and to which profile,
    /// so a move on its way is not made twice nor repeated within `COOLDOWN`.
    rebalanced: HashMap<u64, (Instant, u8)>,
    /// What was last logged about moving each resting pane.
    rebalance_said: HashMap<u64, String>,
    /// Busy panes told their account was nearly out, so each is told once each time its account runs low.
    heads_up: heads_up::Ledger,
    /// Panes left out of the sweep because their model or effort could not be
    /// confirmed, kept so the reason is logged once, not every pass.
    unjudged: std::collections::HashSet<u64>,
    /// What panes have been told about their accounts.
    refused: seen::Seen,

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
    /// The reason for holding last said.  A hold is a steady state,
    /// and the sweep runs every two seconds: said every pass, one held
    /// pane wrote 170 identical lines (2026-09-29, sid 430).  Said only
    /// once, a pane held from 01:26 to 02:59 left one line, and nothing
    /// showed what kept it there (2026-10-08).  So: again whenever it
    /// changes.
    hold_said: Option<&'static str>,
}

impl Default for Stranding {
    fn default() -> Self {
        Self { moves: 0, last_target: None, since: Instant::now(), hold_said: None }
    }
}

#[derive(Clone, Debug)]
struct BindMeta {
    /// Which binding pass named the session: `record`,
    /// `record_overridden`, `argv`, `argv_successor`,
    /// `newest_transcript`, `statusline_hook`, or `unbound`.
    bound_by: &'static str,
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
    /// The claude this binding was read off; what any action on the
    /// pane re-verifies before it signals or types.
    claude: pidtree::ProcId,
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
            rebalanced: HashMap::new(),
            rebalance_said: HashMap::new(),
            heads_up: heads_up::Ledger::default(),
            unjudged: std::collections::HashSet::new(),
            refused: seen::Seen::default(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use super::refusal::last_quota_refusal;

    mod acting;
    mod refused;
    mod settings_file;
    mod telling;
    mod idle_rules;
    mod dormant_records;
    mod reclaim_runs;
    mod idle_loop;
    mod session_binding;
    mod model_read;
    mod profile_list;
    mod cycle_script;
    mod moves;
    mod cycle_clicks;
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
            has_conversation: true,
        };
        let mut ctx = WorkerCtx {
            projects_root: PathBuf::from("/fake"),
            caches: Default::default(),
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
        let tmp = TempDir::new("cc-profiles");
        let base = tmp.path().to_path_buf();
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
            caches: Default::default(),
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
        let tmp = TempDir::new("refusal-window");
        let dir = tmp.path().to_path_buf();
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
        let at = |at| Some(quota::Refusal { at, model: "fable".into(), logged_out: false, window: quota::Window::Week });
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

    /// Running out of the account's own windows names no model, so it
    /// covers every pane. These went unread until 2026-10-31: 389
    /// session and 103 weekly refusals sat in the transcripts.
    #[test]
    fn the_account_window_wording_counts_for_every_model() {
        for text in [
            "You've hit your session limit · resets 4:50pm (Asia/Tokyo)",
            "You've hit your weekly limit · resets Oct 14 at 2pm (Asia/Tokyo)",
        ] {
            let line = REFUSAL.replace(
                "You've reached your Fable limit. Run /usage-credits to continue or switch models with /model.",
                text,
            );
            let r = last_quota_refusal(&line).expect(text);
            assert!(r.model.is_empty(), "{text}: names no model");
            assert!(!r.logged_out);
        }
        let fast = REFUSAL.replace(
            "You've reached your Fable limit. Run /usage-credits to continue or switch models with /model.",
            "You've hit your fast limit",
        );
        assert_eq!(last_quota_refusal(&fast), None, "fast mode is not the account");
    }

    /// The record the CLI writes when it cannot log in, as it was
    /// written on 2026-09-28 (trimmed).
    const LOGIN_EXPIRED: &str = concat!(
        r#"{"parentUuid":"18fba766","isSidechain":false,"type":"assistant","#,
        r#""timestamp":"2026-09-28T08:41:45.587Z","message":{"model":"<synthetic>","#,
        r#""role":"assistant","content":[{"type":"text","text":"Login expired · Please run /login"}]},"#,
        r#""error":"authentication_failed","isApiErrorMessage":true}"#,
    );

    #[test]
    fn a_failed_login_is_a_refusal_for_every_model() {
        let r = last_quota_refusal(LOGIN_EXPIRED).expect("a failed login shuts the pane out");
        assert_eq!(r.at, 1_790_584_905, "2026-09-28T08:41:45Z");
        assert!(r.logged_out);
        assert!(r.model.is_empty(), "it names no model, so it covers them all");
        assert!(!last_quota_refusal(REFUSAL).unwrap().logged_out, "running out is not being logged out");
        let quoted = LOGIN_EXPIRED.replace(r#","isApiErrorMessage":true"#, "");
        assert_eq!(last_quota_refusal(&quoted), None, "only a record the CLI flagged");
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


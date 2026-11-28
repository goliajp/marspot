use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use marspot::settings::AgentAllocation;
use marspot_term::test_env::{EnvGuard, TempDir};

use super::super::alloc_helper::{self, HelperChoice};
use super::super::fixture::*;
use super::super::pane::{PaneGemini, config_dir};
use super::super::quota::from_info;
use super::*;
use crate::observe::Proc;
use crate::plugins::allocation::replay::{Why, replay};
use crate::plugins::claudecode::allocation_events::{DAY, EventLog};
use crate::plugins::{Plugin, PluginError, pidtree, pty_op};

const NOW: i64 = 1_795_600_800;
const ID: &str = "11111111-2222-4333-8444-555555555555";
const PRO: &str = "gemini-3-pro-preview";
const SID: u64 = 77;

#[derive(Default)]
struct Recorder {
    submitted: Mutex<Vec<(u64, &'static str)>>,
}

impl PluginHost for Recorder {
    fn state_dir(&self) -> Result<PathBuf, PluginError> {
        Ok(std::env::temp_dir())
    }
    fn log(&self, _l: LogLevel, _t: &str, _m: &str) {}
    fn submit_pty_op_at(&self, sid: u64, op: pty_op::PtyOp, _start_at: usize) -> Result<(), PluginError> {
        self.submitted.lock().unwrap().push((sid, op.name));
        Ok(())
    }
}

/// A home with three gemini profiles; the pane runs on profile 1 in
/// `/w/a`, and profile 2, the first with room, holds its session when
/// `p2_holds`.
struct World {
    home: TempDir,
    _env: EnvGuard,
    plugin: GeminiPlugin,
    events: PathBuf,
}

/// The pane's facts: its session file last written `quiet` ago.
fn seen(refused: Option<Refusal>, quiet: Duration) -> AllocSeen {
    AllocSeen { sid: SID, started_at: NOW - 3600, refused, written: Some(SystemTime::now() - quiet), tool_running: false, helper: false }
}

fn fell_back() -> Option<Refusal> {
    from_info("Switched to fallback model gemini-3-flash-preview", NOW - 60, Some(PRO))
}

impl World {
    fn new(tag: &str, p2_holds: bool) -> World {
        let home = TempDir::new(tag);
        for n in 1..=3 {
            let root = home.join(format!(".gemini-profile-{n}"));
            let chats = project(&config_dir(&root), "/w/a", "a");
            if n == 1 || (n == 2 && p2_holds) {
                std::fs::write(chats.join(file_name(ID)), head(ID)).unwrap();
            }
        }
        let mut env = EnvGuard::new();
        env.set("HOME", home.path()).set("MARSPOT_STATE_DIR", home.join("state"));
        let mut plugin = GeminiPlugin::new();
        let pane = PaneGemini {
            worker: ProcId { pid: i32::MAX, start_us: 1 },
            shell_pid: i32::MAX,
            root: home.join(".gemini-profile-1"),
            profile: Some(1),
            cwd: Some("/w/a".into()),
            session: Some(ID.into()),
            model: Some(PRO.into()),
        };
        plugin.panes.insert(SID, pane);
        let events = home.join("events");
        plugin.alloc.dir = Some(events.clone());
        plugin.alloc.screen = |_| None;
        // the user is in another pane
        plugin.alloc.focused = Some(SID + 1);
        World { home, _env: env, plugin, events }
    }

    fn pass_with(&mut self, mode: AgentAllocation, s: AllocSeen, now: i64) -> Vec<(u64, &'static str)> {
        let host = Recorder::default();
        self.plugin.allocate(&host, mode, vec![s], now);
        host.submitted.into_inner().unwrap()
    }

    fn pass(&mut self, mode: AgentAllocation) -> Vec<(u64, &'static str)> {
        self.pass_with(mode, seen(fell_back(), Duration::from_secs(120)), NOW)
    }

    fn rows(&self, kind: &str) -> Vec<String> {
        let log = EventLog::for_agent(self.events.clone(), "gemini");
        log.read_since(NOW - DAY, NOW + DAY).into_iter().filter(|e| e.kind == kind).map(|e| e.detail).collect()
    }
}

#[test]
fn gemini_allocation_off_reads_nothing_writes_nothing_and_moves_nothing() {
    let mut w = World::new("gemini-wire-off", true);
    w.plugin.alloc.screen = |_| panic!("off reads no screen");
    assert!(w.pass(AgentAllocation::Off).is_empty());
    assert!(!w.events.exists(), "no events directory");
    assert!(w.plugin.alloc.core.is_none());
    let dir = w.home.join("helper-events");
    assert_eq!(alloc_helper::choose(AgentAllocation::Off, &[1, 2], Some(PRO), NOW, &dir), HelperChoice::TakeTurns, "helpers take turns as before");
    assert!(!dir.exists());
    // the tick gathers nothing either
    let obs = Arc::new(Observation { at: Instant::now(), table: pidtree::ProcTable::from_rows(Vec::new()), sessions: Vec::new() });
    let mut g = Gather::new(AgentAllocation::Off, &obs);
    g.push(SID, ProcId { pid: 1, start_us: 1 }, ProcId { pid: 1, start_us: 1 }, fell_back().as_ref(), Some(SystemTime::now()));
    assert!(g.seen.is_empty());
}

#[test]
fn gemini_allocation_live_resumes_where_the_target_holds_the_session() {
    let mut w = World::new("gemini-wire-resume", true);
    assert_eq!(w.pass(AgentAllocation::Live), [(SID, "gemini.profile_switch")]);
    let moved = w.rows("moved");
    assert_eq!(moved.len(), 1);
    assert!(moved[0].starts_with("to=P2 why=out via=resume "), "{}", moved[0]);
    assert!(w.pass(AgentAllocation::Live).is_empty(), "one move per pane in ten minutes");
}

#[test]
fn gemini_allocation_live_hands_off_where_the_target_does_not_hold_it() {
    let mut w = World::new("gemini-wire-handoff", false);
    assert_eq!(w.pass(AgentAllocation::Live), [(SID, "handoff.switch")]);
    assert!(w.rows("moved")[0].starts_with("to=P2 why=out via=handoff "));
}

#[test]
fn gemini_allocation_a_quiet_pane_that_is_not_idle_stays() {
    let quiet = Duration::from_secs(120);
    let mut w = World::new("gemini-wire-tool", true);
    assert!(w.pass_with(AgentAllocation::Live, AllocSeen { tool_running: true, ..seen(fell_back(), quiet) }, NOW).is_empty(), "a tool runs");

    drop(w);
    let mut w = World::new("gemini-wire-approval", true);
    w.plugin.alloc.screen = |_| Some("│ Allow execution of: 'cargo'? │\n│ ● 1. Allow once │\n│   3. No, suggest changes (esc) │".into());
    assert!(w.pass(AgentAllocation::Live).is_empty(), "an approval is asked");

    drop(w);
    let mut w = World::new("gemini-wire-typed", true);
    w.plugin.alloc.screen = |_| Some("╭────╮\n│ > half a question │\n╰────╯".into());
    assert!(w.pass(AgentAllocation::Live).is_empty(), "text is typed");
    assert!(w.rows("would_move").is_empty());

    // a fallback keeps gemini working: the user must be elsewhere, and
    // the session quiet a minute
    drop(w);
    let mut w = World::new("gemini-wire-focus", true);
    w.plugin.alloc.focused = Some(SID);
    assert!(w.pass(AgentAllocation::Live).is_empty(), "the user is in the pane");
    assert!(w.pass_with(AgentAllocation::Live, seen(fell_back(), Duration::from_secs(30)), NOW).is_empty());
    w.plugin.alloc.focused = Some(SID + 1);
    assert!(w.pass_with(AgentAllocation::Live, seen(fell_back(), Duration::from_secs(30)), NOW + 1).is_empty(), "quiet under a minute");
    assert_eq!(w.pass_with(AgentAllocation::Live, seen(fell_back(), quiet), NOW + 2), [(SID, "gemini.profile_switch")]);

    // the dialog has stopped gemini: no need to wait for the user
    drop(w);
    let mut w = World::new("gemini-wire-dialog-focused", true);
    w.plugin.alloc.focused = Some(SID);
    w.plugin.alloc.screen = |_| Some("│ Usage limit reached for all Pro models. │\n│ ● 1. Switch to gemini-3-flash-preview │\n│   2. Stop │".into());
    assert_eq!(w.pass_with(AgentAllocation::Live, seen(None, Duration::from_secs(30)), NOW), [(SID, "gemini.profile_switch")]);
}

#[test]
fn gemini_allocation_before_any_focus_event_the_user_may_be_in_the_pane() {
    let mut w = World::new("gemini-wire-no-focus", true);
    w.plugin.alloc.focused = None;
    assert!(w.pass(AgentAllocation::Live).is_empty(), "a shell just restarted: a working gemini stays");
    w.plugin.on_pane_focused(&Recorder::default(), SID + 1);
    assert_eq!(w.pass(AgentAllocation::Live), [(SID, "gemini.profile_switch")]);
}

#[test]
fn gemini_allocation_a_tool_is_a_child_started_after_gemini() {
    let row = |pid: i32, ppid: i32, start_us: u64, argv: &[&str]| Proc {
        row: pidtree::ProcRow { pid, ppid, comm: "x".into(), start_unix: 0, start_us, pgid: pid, tty_dev: 1, tty_fg_pgid: 10, status: libc::SSLEEP, execed: true },
        argv: Some(argv.iter().map(|a| a.to_string()).collect::<Vec<_>>().into()),
    };
    let worker = ProcId { pid: 11, start_us: 1_000_000 };
    let gemini = [row(10, 1, 500_000, &["gemini"]), row(11, 10, 1_000_000, &["gemini", "gemini"])];
    let with = |child: Proc| crate::observe::tests::session_with(gemini.iter().cloned().chain([child]).collect(), 10);
    assert!(!tool_running(&with(row(12, 11, 2_000_000, &["node", "mcp-server.js"])), worker), "started with gemini: an MCP server");
    assert!(tool_running(&with(row(12, 11, 60_000_000, &["/bin/bash", "-c", "cargo test"])), worker));
    assert!(!tool_running(&with(row(12, 10, 60_000_000, &["/bin/bash"])), worker), "not the worker's");
}

#[test]
fn gemini_allocation_a_dialog_s_reset_counts_from_when_it_was_first_seen() {
    let mut w = World::new("gemini-wire-reset-after", true);
    w.plugin.alloc.screen = |_| Some("│ Usage limit reached for gemini-3-pro-preview. Your quota will reset after 2h. │\n│ ● 1. Stop │".into());
    let quiet = Duration::from_secs(120);
    w.pass_with(AgentAllocation::DryRun, seen(None, quiet), NOW);
    // the file is written (a footer refresh, say) and the dialog is still up
    w.pass_with(AgentAllocation::DryRun, seen(None, quiet - Duration::from_secs(1)), NOW + 300);
    let refused = w.rows("refused");
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(refused[0].contains(&format!("resets={}", NOW + 7200)), "{}", refused[0]);
}

#[test]
fn gemini_allocation_an_old_limit_line_on_screen_is_no_refusal() {
    let mut w = World::new("gemini-wire-history", true);
    w.plugin.alloc.screen = |_| Some("✕ Usage limit reached for gemini-3-pro-preview.\n✦ later answer\n╭────╮\n│ >   Type your message or @path/to/file │\n╰────╯".into());
    w.pass_with(AgentAllocation::DryRun, seen(None, Duration::from_secs(120)), NOW);
    assert!(w.rows("refused").is_empty());
    assert!(w.rows("would_move").is_empty());
}

#[test]
fn gemini_allocation_dryrun_moves_nothing_and_every_row_replays() {
    let mut w = World::new("gemini-wire-dryrun", false);
    w.plugin.alloc.screen = |_| Some("│ Usage limit reached for all Pro models. │\n│ You have 5 AI Credits available. │\n│ ● 1. Use AI Credits - Continue this request (Overage) │".into());
    assert!(w.pass(AgentAllocation::DryRun).is_empty());
    assert!(w.rows("moved").is_empty());
    let refused = w.rows("refused");
    assert_eq!(refused.len(), 2, "the fallback and the overage dialog: {refused:?}");
    assert!(refused[1].contains("how=overage"));
    let would = w.rows("would_move");
    assert_eq!(would.len(), 1);
    assert!(would[0].starts_with("to=P2 why=out via=handoff "), "{}", would[0]);
    assert_eq!(replay(&would[0]), Some(Some((2, Why::Out))));

    // a helper, with the pane's refusal published: P1 is passed over,
    // the row it writes replays, and dry run still takes turns
    let dir = w.home.join("helper-events");
    assert_eq!(alloc_helper::choose(AgentAllocation::DryRun, &[1, 2, 3], Some(PRO), NOW, &dir), HelperChoice::TakeTurns);
    let rows = EventLog::for_agent(dir.clone(), "gemini").read_since(NOW - DAY, NOW + DAY);
    assert_eq!(rows.len(), 1);
    let line = &rows[0].detail;
    let to = replay(line).unwrap().unwrap().0;
    assert_ne!(to, 1, "{line}");
    assert!(line.starts_with(&format!("to=P{to} ")), "{line}");

    // every home refuses: dry run writes so and still takes turns; live refuses
    let back = super::super::quota::pacific_midnight_after(NOW - 60);
    assert_eq!(alloc_helper::choose(AgentAllocation::DryRun, &[1], Some(PRO), NOW, &dir), HelperChoice::TakeTurns);
    let rows = EventLog::for_agent(dir.clone(), "gemini").read_since(NOW - DAY, NOW + DAY);
    assert_eq!((rows[1].kind.as_str(), rows[1].detail.clone()), ("would_refuse_new", format!("model={PRO} back=P1@{back}")));
    assert_eq!(alloc_helper::choose(AgentAllocation::Live, &[1], Some(PRO), NOW, &dir), HelperChoice::Nowhere(vec![(1, back)]));
}

#[path = "alloc_wire_reads_tests.rs"]
mod reads;

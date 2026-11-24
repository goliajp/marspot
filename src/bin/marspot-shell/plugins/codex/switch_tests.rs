//! The badge menu's account switch submits a switch only once peerbus
//! has said the session may move, and resumes the thread by id.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use super::pane::PaneCodex;
use super::CodexPlugin;
use crate::plugins::handoff::{self, Agent};
use crate::plugins::peerbus::moving::{self, Ask, Held, Listener, Verdict};
use crate::plugins::peerbus::{publish_threads, ThreadView};
use crate::plugins::{pty_op, LogLevel, Plugin, PluginError, PluginHost};
use marspot::pidtree::ProcId;
use marspot::ui::strings::Msg;
use marspot_term::test_env::{EnvGuard, TempDir};

const THREAD: &str = "01a133f4-8e7b-7331-9630-9c40ff4b467f";

#[derive(Default)]
struct Recorder {
    submitted: Mutex<Vec<(u64, pty_op::PtyOp)>>,
}

impl PluginHost for Recorder {
    fn state_dir(&self) -> Result<PathBuf, PluginError> {
        Ok(std::env::temp_dir())
    }
    fn log(&self, _l: LogLevel, _t: &str, _m: &str) {}
    fn submit_pty_op_at(&self, sid: u64, op: pty_op::PtyOp, _start_at: usize) -> Result<(), PluginError> {
        self.submitted.lock().unwrap().push((sid, op));
        Ok(())
    }
}

impl Recorder {
    fn names(&self) -> Vec<&'static str> {
        self.submitted.lock().unwrap().iter().map(|(_, op)| op.name).collect()
    }
}

struct World {
    _home: TempDir,
    _env: EnvGuard,
    plugin: CodexPlugin,
    host: Recorder,
    asks: Listener,
}

const SID: u64 = 31;
const CODEX: ProcId = ProcId { pid: i32::MAX, start_us: 1 };

/// A codex pane on profile 1, peerbus bound to its thread when `bound`,
/// and a worker that takes asks without answering them.
fn world(argv: &[&str], bound: bool) -> World {
    let home = TempDir::new("codex-switch");
    for d in [".codex-profile-1", ".codex-profile-2"] {
        std::fs::create_dir_all(home.path().join(d)).unwrap();
    }
    let mut env = EnvGuard::new();
    env.set("HOME", home.path()).set("MARSPOT_STATE_DIR", home.join("state"));
    let mut plugin = CodexPlugin::new();
    let argv = Some(argv.iter().map(|a| a.to_string()).collect());
    plugin.panes.insert(SID, PaneCodex { codex: CODEX, shell_pid: i32::MAX, profile: Some(1), argv });
    let mut views = HashMap::new();
    if bound {
        let home1 = home.path().join(".codex-profile-1");
        views.insert(SID, ThreadView { codex: CODEX, home: home1, thread: THREAD.into(), idle: true });
    }
    publish_threads(views);
    let asks = Listener::connect();
    World { _home: home, _env: env, plugin, host: Recorder::default(), asks }
}

impl World {
    fn pick_p2(&mut self) {
        self.plugin.on_pane_badge_menu_action(&self.host, SID, handoff::profile_tag(Agent::Codex, 2));
    }
}

#[test]
fn a_session_peerbus_holds_is_not_switched_and_says_why() {
    for (held, hint) in [
        (Held::Busy, Msg::BadgeCodexTurnRunning),
        (Held::ChildRunning, Msg::BadgeCodexAgentRunning),
        (Held::Queued, Msg::BadgeCodexQueued),
        (Held::Moving, Msg::BadgeCodexSwitching),
    ] {
        let mut w = world(&["codex"], true);
        w.pick_p2();
        assert_eq!(w.asks.next(), Some(Ask::Begin { sid: SID, codex: CODEX }), "peerbus was asked");
        moving::post(SID, CODEX, Verdict::Held(held));
        w.plugin.follow_switches(&w.host);
        assert_eq!(w.host.names(), Vec::<&str>::new(), "{held:?}: no op");
        assert_eq!(w.plugin.hint_for(SID), Some(hint), "{held:?}");
        assert!(w.plugin.hinted(SID, "P1@codex".into()).starts_with("P1@codex ⚠"), "{held:?}");
    }
}

#[test]
fn a_pick_while_a_switch_is_on_its_way_is_held() {
    let mut w = world(&["codex", "resume", THREAD], true);
    w.pick_p2();
    w.pick_p2();
    assert!(w.asks.next().is_some());
    assert_eq!(w.asks.next(), None, "asked once");
    assert_eq!(w.plugin.hint_for(SID), Some(Msg::BadgeCodexSwitching));
    assert_eq!(w.host.names(), Vec::<&str>::new());
}

#[test]
fn a_custom_command_line_is_not_switched() {
    // what the old menu left behind: `resume --last '<prompt>'`
    let mut w = world(&["codex", "resume", "--last", "go on"], true);
    w.pick_p2();
    assert_eq!(w.asks.next(), None, "peerbus not even asked");
    assert_eq!(w.plugin.hint_for(SID), Some(Msg::BadgeCodexCustomCommand));
    assert_eq!(w.host.names(), Vec::<&str>::new());
}

#[test]
fn a_thread_peerbus_does_not_know_is_handed_off_never_resumed_by_last() {
    let mut w = world(&["codex"], false);
    w.pick_p2();
    assert_eq!(w.asks.next(), None);
    assert_eq!(w.host.names(), ["handoff.switch"], "no switch op without a thread id");
}

#[test]
fn a_session_that_may_move_is_resumed_by_id_under_the_new_home() {
    let mut w = world(&["codex"], true);
    w.pick_p2();
    let old = w._home.path().join(".codex-profile-1");
    moving::post(SID, CODEX, Verdict::Go { thread: THREAD.into(), home: old });
    w.plugin.follow_switches(&w.host);
    let ops = w.host.submitted.lock().unwrap();
    assert_eq!(ops.iter().map(|(_, o)| o.name).collect::<Vec<_>>(), ["codex.profile_switch"]);
    let line: String = ops[0]
        .1
        .steps
        .iter()
        .filter_map(|s| match &s.kind {
            pty_op::StepKind::Send(b) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        })
        .collect();
    assert!(line.contains(&format!("codex resume {THREAD}")) && !line.contains("--last"), "{line}");
    assert!(line.contains(".codex-profile-2'"), "{line}");
    drop(ops);
    // a verdict for a pane that asked nothing, or a second one, does nothing
    moving::post(SID, CODEX, Verdict::Go { thread: THREAD.into(), home: PathBuf::from("/x") });
    moving::post(SID + 1, CODEX, Verdict::Go { thread: THREAD.into(), home: PathBuf::from("/x") });
    w.plugin.follow_switches(&w.host);
    assert_eq!(w.host.names().len(), 1);
}

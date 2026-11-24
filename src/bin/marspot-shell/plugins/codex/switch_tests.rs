//! The badge menu's account switch submits a switch only once peerbus
//! has said the session may move, and resumes the thread by id.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use super::pane::{PaneCodex, RELEASED};
use super::CodexPlugin;
use crate::plugins::handoff::{self, Agent};
use crate::plugins::peerbus::moving::{self, Ask, Held, Listener, Released, Verdict};
use crate::plugins::peerbus::{publish_threads, ThreadView};
use crate::plugins::{pty_op, LogLevel, PaneStatusView, Plugin, PluginError, PluginHost};
use marspot::pane_state::{BusyKind, PaneStatus};
use marspot::pidtree::ProcId;
use marspot::ui::strings::Msg;
use marspot_term::test_env::{EnvGuard, TempDir};

const THREAD: &str = "01a133f4-8e7b-7331-9630-9c40ff4b467f";

#[derive(Default)]
struct Recorder {
    submitted: Mutex<Vec<(u64, pty_op::PtyOp)>>,
    /// How long the pane's terminal has been quiet, if it is.
    quiet: Mutex<Option<Duration>>,
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
    fn pane_status(&self, _sid: u64) -> Result<Option<PaneStatusView>, PluginError> {
        let quiet = *self.quiet.lock().unwrap();
        Ok(Some(match quiet {
            Some(held) => PaneStatusView { status: PaneStatus::Busy(BusyKind::ForegroundJob), held, quiescent: true },
            None => PaneStatusView { status: PaneStatus::Busy(BusyKind::Working), held: Duration::ZERO, quiescent: false },
        }))
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

fn go(home: PathBuf) -> Verdict {
    Verdict::Go { thread: THREAD.into(), home, released: Released(pty_op::Job::new()) }
}

impl World {
    fn pick_p2(&mut self) {
        self.plugin.on_pane_badge_menu_action(&self.host, SID, handoff::profile_tag(Agent::Codex, 2));
    }

    fn old_home(&self) -> PathBuf {
        self._home.path().join(".codex-profile-1")
    }

    /// Pick P2, be told to go, and return the switch op's end hook.
    fn switched(&mut self) -> pty_op::OnEnd {
        self.pick_p2();
        assert_eq!(self.asks.next(), Some(Ask::Begin { sid: SID, codex: CODEX }));
        moving::post(SID, CODEX, go(self.old_home()));
        self.plugin.follow_switches(&self.host);
        let ops = self.host.submitted.lock().unwrap();
        assert_eq!(ops.iter().map(|(_, o)| o.name).collect::<Vec<_>>(), ["codex.profile_switch"]);
        ops[0].1.on_end.clone().expect("the switch hears how it ended")
    }
}

fn line_of(op: &pty_op::PtyOp) -> String {
    op.steps
        .iter()
        .filter_map(|s| match &s.kind {
            pty_op::StepKind::Send(b) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_session_peerbus_holds_is_not_switched_and_says_why() {
    for (held, hint) in [
        (Held::Busy, Msg::BadgeCodexTurnRunning),
        (Held::ChildRunning, Msg::BadgeCodexAgentRunning),
        (Held::Queued, Msg::BadgeCodexQueued),
        (Held::Moving, Msg::BadgeCodexSwitching),
        (Held::NotBound, Msg::BadgeCodexNotTracked),
        (Held::Unknown, Msg::BadgeCodexStateUnknown),
    ] {
        let mut w = world(&["codex"], true);
        w.pick_p2();
        assert_eq!(w.asks.next(), Some(Ask::Begin { sid: SID, codex: CODEX }), "peerbus was asked");
        moving::post(SID, CODEX, Verdict::Held(held));
        w.plugin.follow_switches(&w.host);
        assert_eq!(w.host.names(), Vec::<&str>::new(), "{held:?}: no op");
        assert_eq!(w.plugin.hint_for(SID), Some(hint), "{held:?}");
        assert!(w.plugin.hinted(SID, "P1@codex".into()).starts_with("P1@codex ⚠"), "{held:?}");
        assert_eq!(w.asks.next(), None, "{held:?}: nothing marked, nothing to end");
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
    for bound in [true, false] {
        let mut w = world(&["codex", "resume", "--last", "go on"], bound);
        *w.host.quiet.lock().unwrap() = Some(Duration::from_secs(60));
        w.pick_p2();
        assert_eq!(w.asks.next(), None, "peerbus not even asked");
        assert_eq!(w.plugin.hint_for(SID), Some(Msg::BadgeCodexCustomCommand), "bound: {bound}");
        assert_eq!(w.host.names(), Vec::<&str>::new(), "not handed off either");
    }
}

#[test]
fn a_thread_peerbus_does_not_know_is_handed_off_never_resumed_by_last() {
    let mut w = world(&["codex"], false);
    *w.host.quiet.lock().unwrap() = Some(Duration::from_secs(60));
    w.pick_p2();
    assert_eq!(w.asks.next(), None);
    assert_eq!(w.host.names(), ["handoff.switch"], "no switch op without a thread id");
}

#[test]
fn a_thread_peerbus_does_not_know_is_not_handed_off_mid_turn() {
    let mut w = world(&["codex"], false);
    w.pick_p2();
    assert_eq!(w.host.names(), Vec::<&str>::new(), "its spinner is still drawing");
    assert_eq!(w.plugin.hint_for(SID), Some(Msg::BadgeCodexTerminalBusy));
    *w.host.quiet.lock().unwrap() = Some(Duration::from_millis(500));
    w.pick_p2();
    assert_eq!(w.host.names(), Vec::<&str>::new(), "half a second is a spinner's pause");
}

#[test]
fn a_session_that_may_move_is_resumed_by_id_under_the_new_home() {
    let mut w = world(&["codex"], true);
    w.switched();
    let ops = w.host.submitted.lock().unwrap();
    let line = line_of(&ops[0].1);
    assert!(line.contains(&format!("codex resume {THREAD}")) && !line.contains("--last"), "{line}");
    assert!(line.contains(".codex-profile-2'"), "{line}");
    assert!(ops[0].1.steps.iter().any(|s| s.label == RELEASED), "waits for the old home");
    drop(ops);
    // a second go for the switch under way does nothing
    moving::post(SID, CODEX, go(PathBuf::from("/x")));
    w.plugin.follow_switches(&w.host);
    assert_eq!(w.host.names().len(), 1);
    assert_eq!(w.asks.next(), None);
}

#[test]
fn a_go_for_a_switch_no_longer_asked_for_unmarks_the_pane() {
    // a go nobody asked for
    let mut w = world(&["codex"], true);
    moving::post(SID + 1, CODEX, go(PathBuf::from("/x")));
    w.plugin.follow_switches(&w.host);
    assert_eq!(w.asks.next(), Some(Ask::End { sid: SID + 1, codex: CODEX }));

    // the pane's codex went while the verdict was on its way
    drop(w);
    let mut w = world(&["codex"], true);
    w.pick_p2();
    assert!(matches!(w.asks.next(), Some(Ask::Begin { .. })));
    w.plugin.panes.remove(&SID);
    w.plugin.follow_switches(&w.host);
    assert_eq!(w.asks.next(), Some(Ask::End { sid: SID, codex: CODEX }), "dropped while waiting");
    moving::post(SID, CODEX, go(w.old_home()));
    w.plugin.follow_switches(&w.host);
    assert_eq!(w.asks.next(), Some(Ask::End { sid: SID, codex: CODEX }), "and the late go");
    assert_eq!(w.host.names(), Vec::<&str>::new());

    // gone in the same tick the go came
    drop(w);
    let mut w = world(&["codex"], true);
    w.pick_p2();
    assert!(matches!(w.asks.next(), Some(Ask::Begin { .. })));
    w.plugin.panes.remove(&SID);
    moving::post(SID, CODEX, go(w.old_home()));
    w.plugin.follow_switches(&w.host);
    assert_eq!(w.asks.next(), Some(Ask::End { sid: SID, codex: CODEX }));
    assert_eq!(w.host.names(), Vec::<&str>::new());
}

#[test]
fn a_switch_op_that_did_not_finish_unmarks_the_pane() {
    let mut w = world(&["codex"], true);
    let on_end = w.switched();
    on_end(&pty_op::OpOutcome::Cancelled("escape"));
    assert_eq!(w.asks.next(), Some(Ask::End { sid: SID, codex: CODEX }));
    w.plugin.follow_switches(&w.host);
    assert_eq!(w.host.names(), ["codex.profile_switch"], "nothing more");

    drop(w);
    let mut w = world(&["codex"], true);
    let on_end = w.switched();
    on_end(&pty_op::OpOutcome::Done);
    assert_eq!(w.asks.next(), None, "a switch that happened ends with the old binding");
}

#[test]
fn an_old_home_that_keeps_the_session_gets_the_tui_back() {
    let mut w = world(&["codex"], true);
    let on_end = w.switched();
    on_end(&pty_op::OpOutcome::Failed { step: 3, label: RELEASED, err: "still loaded".into() });
    assert_eq!(w.asks.next(), Some(Ask::End { sid: SID, codex: CODEX }));
    w.plugin.follow_switches(&w.host);
    assert_eq!(w.host.names(), ["codex.profile_switch", "codex.switch_back"]);
    let ops = w.host.submitted.lock().unwrap();
    let line = line_of(&ops[1].1);
    assert!(line.contains(&format!("codex resume {THREAD}")) && line.contains(".codex-profile-1'"), "{line}");
    assert!(!ops[1].1.steps.iter().any(|s| s.label == RELEASED), "the same home has nothing to let go of");
    drop(ops);
    assert_eq!(w.plugin.hint_for(SID), Some(Msg::BadgeCodexStillLoaded));
}

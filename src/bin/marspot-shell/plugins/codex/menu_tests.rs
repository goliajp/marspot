//! A pick on another agent's badge is not codex's to act on.
//!
//! Every plugin hears every badge-menu pick.  On 2026-11-17 claude's
//! "P2" row and codex's "P2" row had the same tag, and a pick on
//! claude's badge ran a codex profile switch on claude's pane.

use std::path::PathBuf;
use std::sync::Mutex;

use super::pane::PaneCodex;
use super::CodexPlugin;
use crate::plugins::handoff::{self, Agent};
use crate::plugins::{pty_op, LogLevel, PaneStatusView, Plugin, PluginError, PluginHost};
use marspot::pane_state::{BusyKind, PaneStatus};
use marspot::pidtree::ProcId;
use marspot_term::test_env::{EnvGuard, TempDir};

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
    /// A terminal long quiet: no turn is drawing.
    fn pane_status(&self, _sid: u64) -> Result<Option<PaneStatusView>, PluginError> {
        let held = std::time::Duration::from_secs(60);
        Ok(Some(PaneStatusView { status: PaneStatus::Busy(BusyKind::ForegroundJob), held, quiescent: true }))
    }
}

#[test]
fn a_pick_on_claude_s_badge_makes_codex_submit_nothing() {
    // two profiles of each, so every row claude's menu can carry exists
    let home = TempDir::new("codex-menu");
    for d in [".claude-profile-1", ".claude-profile-2", ".codex-profile-1", ".codex-profile-2"] {
        std::fs::create_dir_all(home.path().join(d)).unwrap();
    }
    let mut env = EnvGuard::new();
    env.set("HOME", home.path()).set("MARSPOT_STATE_DIR", home.join("state"));

    // the incident's state: the codex plugin holds the pane as a codex
    // pane on profile 1, as it wrongly did for pane 484
    const SID: u64 = 484;
    let mut plugin = CodexPlugin::new();
    plugin.panes.insert(SID, PaneCodex { codex: ProcId { pid: i32::MAX, start_us: 1 }, shell_pid: i32::MAX, profile: Some(1), argv: Some(vec!["codex".into()]) });
    let host = Recorder::default();

    let mut claude_rows = crate::plugins::claudecode::badge_menu_for(1, &[1, 2]);
    claude_rows.extend(handoff::menu_rows(Agent::Claude));
    assert!(claude_rows.len() >= 3, "claude's menu has its P2 row and its handoff rows: {}", claude_rows.len());
    for row in &claude_rows {
        plugin.on_pane_badge_menu_action(&host, SID, row.tag);
    }
    assert_eq!(*host.submitted.lock().unwrap(), [], "codex acted on a row of claude's menu");

    assert_eq!(plugin.hint_for(SID), None, "nor said anything beside codex's badge");

    // the same pane, codex's own P2 row: that one is codex's, and with
    // no thread known to resume by it is refused, not handed over
    plugin.on_pane_badge_menu_action(&host, SID, handoff::profile_tag(Agent::Codex, 2));
    assert_eq!(*host.submitted.lock().unwrap(), []);
    assert_eq!(plugin.hint_for(SID), Some(marspot::ui::strings::Msg::BadgeCodexThreadUnknown));
}

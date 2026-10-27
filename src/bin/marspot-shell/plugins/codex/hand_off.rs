//! Handing a codex pane's conversation to claude (RFC-009).

use super::{argv_thread_id, home_of, CodexPlugin};
use crate::plugins::{handoff, pidtree, LogLevel, PluginHost};

impl CodexPlugin {
    /// Move this pane's conversation to claude profile `n` (RFC-009).
    pub(super) fn hand_off(&mut self, host: &dyn PluginHost, sid: u64, n: u8) {
        let Some(pane) = self.panes.get(&sid).cloned() else {
            host.log(LogLevel::Warn, "handoff.no_codex", &format!("pane {sid}: no codex to hand off from"));
            return;
        };
        let Some((_, dir)) = handoff::profiles(handoff::Agent::Claude).into_iter().find(|(p, _)| *p == n) else {
            host.log(LogLevel::Warn, "handoff.profile_gone", &format!("claude profile {n} no longer exists"));
            return;
        };
        let pid = pane.codex.pid;
        let Some(home) = home_of(pid) else { return };
        let leaving = handoff::Leaving {
            agent: handoff::Agent::Codex,
            proc: pane.codex,
            shell_pid: pane.shell_pid,
            home,
            session_id: argv_thread_id(pid),
            cwd: pidtree::proc_cwd(pid).map(|c| c.to_string_lossy().into_owned()),
        };
        host.log(LogLevel::Info, "handoff.start", &format!("pane {sid} codex → claude P{n}: {leaving:?}"));
        let Some(op) = handoff::switch_op(sid, leaving, handoff::Agent::Claude, n, dir) else {
            host.log(LogLevel::Warn, "handoff.no_command", &format!("pane {sid}: cannot build the claude line"));
            return;
        };
        if let Err(e) = host.submit_pty_op(sid, op) {
            host.log(LogLevel::Warn, "handoff.submit_failed", &format!("pane {sid}: {e}"));
        }
    }
}

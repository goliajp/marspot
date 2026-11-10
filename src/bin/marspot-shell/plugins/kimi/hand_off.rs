//! Handing a kimi pane's conversation to another agent, or to another
//! kimi account.

use super::{KimiPlugin, PaneKimi};
use crate::plugins::{handoff, LogLevel, PluginHost};

impl KimiPlugin {
    /// Move this pane's conversation to profile `n` of `to`.
    ///
    /// Another kimi account goes the same way as another agent: a new
    /// session there, given the last exchanges to read.  A session
    /// lives in its account's home, and resuming it under another
    /// would need its files moved between two homes kimi may both
    /// have open.  What is kept is the model: the new kimi is started
    /// on the one this pane was using.
    ///
    /// Only with the session in hand.  A kimi that was never given a
    /// prompt has no conversation to carry, and stopping it to start
    /// another with an empty handoff would not be what the row said.
    pub(super) fn hand_off(&mut self, host: &dyn PluginHost, sid: u64, pane: &PaneKimi, to: handoff::Agent, n: u8) {
        let Some(session) = pane.session.clone() else {
            host.log(LogLevel::Warn, "handoff.no_session", &format!("pane {sid}: kimi has no session yet"));
            return;
        };
        let Some((_, dir)) = handoff::profiles(to).into_iter().find(|(p, _)| *p == n) else {
            host.log(LogLevel::Warn, "handoff.profile_gone", &format!("{} profile {n} no longer exists", to.key()));
            return;
        };
        let leaving = handoff::Leaving {
            agent: handoff::Agent::Kimi,
            proc: pane.proc,
            shell_pid: pane.shell_pid,
            home: pane.home.clone(),
            session_id: Some(session),
            cwd: pane.cwd.clone(),
        };
        host.log(LogLevel::Info, "handoff.start", &format!("pane {sid} kimi → {} P{n}: {leaving:?}", to.key()));
        // a model named by environment variables exists in this process only
        let alias = pane.facts.as_ref().and_then(|f| f.alias.clone()).filter(|a| a != super::ENV_ALIAS);
        let Some(op) = super::pane::with_model(alias, || handoff::switch_op(sid, leaving, to, n, dir)) else {
            host.log(LogLevel::Warn, "handoff.no_command", &format!("pane {sid}: cannot build the {} line", to.key()));
            return;
        };
        if let Err(e) = host.submit_pty_op(sid, op) {
            host.log(LogLevel::Warn, "handoff.submit_failed", &format!("pane {sid}: {e}"));
        }
    }
}

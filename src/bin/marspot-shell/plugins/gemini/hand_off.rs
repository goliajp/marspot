//! Handing a gemini pane's conversation to another agent.

use super::pane::PaneGemini;
use super::GeminiPlugin;
use crate::plugins::{handoff, LogLevel, PluginHost};

impl GeminiPlugin {
    /// Move this pane's conversation to profile `n` of `to`.
    ///
    /// Only with the session in hand.  A gemini whose session file
    /// could not be told has nothing that can be read for the next
    /// agent, and stopping it to start another with an empty handoff
    /// would lose the conversation the menu row promised to carry.
    pub(super) fn hand_off(&mut self, host: &dyn PluginHost, sid: u64, pane: &PaneGemini, to: handoff::Agent, n: u8) {
        let Some(session) = pane.session.clone() else {
            host.log(LogLevel::Warn, "handoff.no_session", &format!("pane {sid}: gemini's session could not be told"));
            return;
        };
        let Some((_, dir)) = handoff::profiles(to).into_iter().find(|(p, _)| *p == n) else {
            host.log(LogLevel::Warn, "handoff.profile_gone", &format!("{} profile {n} no longer exists", to.key()));
            return;
        };
        let leaving = handoff::Leaving {
            agent: handoff::Agent::Gemini,
            // the worker: its waiting parent follows it down
            proc: pane.worker,
            shell_pid: pane.shell_pid,
            home: pane.root.clone(),
            session_id: Some(session.clone()),
            cwd: pane.cwd.clone(),
        };
        // read back if the pane is handed to gemini again and resumes this session
        if let Err(e) = handoff::remember_gemini_model(sid, &session, pane.model.as_deref()) {
            host.log(LogLevel::Warn, "handoff.model_not_kept", &format!("pane {sid}: {e}"));
        }
        host.log(LogLevel::Info, "handoff.start", &format!("pane {sid} gemini → {} P{n}: {leaving:?}", to.key()));
        let Some(op) = handoff::switch_op(sid, leaving, to, n, dir) else {
            host.log(LogLevel::Warn, "handoff.no_command", &format!("pane {sid}: cannot build the {} line", to.key()));
            return;
        };
        if let Err(e) = host.submit_pty_op(sid, op) {
            host.log(LogLevel::Warn, "handoff.submit_failed", &format!("pane {sid}: {e}"));
        }
    }
}

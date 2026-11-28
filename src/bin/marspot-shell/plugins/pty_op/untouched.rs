//! [`StepKind::Untouched`]: a run that types into a pane nobody asked it
//! to type into goes on only while the pane is as the run found it.

use std::time::{Duration, SystemTime};

use super::{LogLevel, OpOutcome, OpRunner, PaneSessionHost, Step, StepKind};

impl Step {
    /// Stop the run, failed, if the pane has had any input since
    /// `since` or it is past `until`.  Checked when the step runs, so
    /// put it right before the line it guards.
    pub fn untouched(since: SystemTime, until: SystemTime) -> Self {
        let kind = StepKind::Untouched { since, until };
        Step { kind, timeout: Some(Duration::from_secs(1)), proceed_on_timeout: false, at_shell: None, label: "untouched" }
    }
}

impl OpRunner {
    /// One look; `true` either way, since the step never waits.
    pub(super) fn untouched_step(&mut self, host: &dyn PaneSessionHost, label: &'static str, since: SystemTime, until: SystemTime) -> bool {
        let sid = host.shelld_session_id();
        let why = if self.env.now() > until {
            Some("its window has passed")
        } else if self.env.last_input(sid).is_some_and(|t| t >= since) {
            Some("the pane has had input since")
        } else {
            None
        };
        if let Some(err) = why {
            host.log(LogLevel::Info, &format!("{}.not_typed", self.op.name), &format!("pane {sid}: {err}"));
            self.finish(host, OpOutcome::Failed { step: self.at, label, err: err.into() });
        }
        true
    }
}

#[cfg(test)]
#[path = "untouched_tests.rs"]
mod tests;

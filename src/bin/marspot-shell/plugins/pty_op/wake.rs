//! Moving a run parked on the user on, and how long what follows the
//! wake may still take.

use std::time::Duration;

use super::{OpRunner, PtyOp, StepKind};
use crate::plugins::{LogLevel, PaneSessionHost};

impl PtyOp {
    /// The longest bringing the pane back may take once the run is
    /// woken; asked while the run is busy, the answer is what is left.
    pub fn wake_bound(mut self, d: Duration) -> Self {
        self.wake_bound = Some(d);
        self
    }
}

impl OpRunner {
    /// The user is back — moves an `AwaitUser` step on.  Returns true
    /// if that is what it did.
    pub fn wake(&mut self, host: &dyn PaneSessionHost, trigger: &str) -> bool {
        if !self.is_awaiting_user() {
            return false;
        }
        host.log(
            LogLevel::Info,
            &format!("{}.woken", self.op.name),
            &format!("pane {} by {trigger}", host.shelld_session_id()),
        );
        self.woken_at = Some(self.env.now());
        self.advance(host);
        true
    }

    /// Is the run parked on its `AwaitUser` step?
    pub fn is_awaiting_user(&self) -> bool {
        matches!(self.op.steps.get(self.at).map(|s| &s.kind), Some(StepKind::AwaitUser))
    }

    /// What is left of [`PtyOp::wake_bound`] while the run is still
    /// bringing the pane back; none before a wake and once it is over.
    pub(super) fn wake_left(&self) -> Option<Duration> {
        let (bound, at) = (self.op.wake_bound?, self.woken_at?);
        if self.finished.is_some() || self.is_awaiting_user() {
            return None;
        }
        let gone = self.env.now().duration_since(at).unwrap_or_default();
        Some(bound.saturating_sub(gone))
    }
}

//! The one place a run types into its pane.
//!
//! Every byte and every paste an op writes goes through
//! [`OpRunner::type_into`], which asks again, right before the write,
//! whether the process meant to read it is still the one the run
//! decided on.  Output having gone quiet, or a step having passed a
//! moment ago, says nothing about who reads the next bytes: the shell a
//! line is meant for can have started a job, and the program a script
//! waited for can have exited and left the shell to read its input as
//! commands.

use super::OpRunner;
use crate::plugins::{LogLevel, PaneSessionHost};

/// What a run types: bytes as they are, or text delivered the way a
/// paste arrives (see [`super::PtyIo::paste`]).
#[derive(Clone, Copy, Debug)]
pub enum Typed<'a> {
    Bytes(&'a [u8]),
    Paste(&'a str),
}

impl OpRunner {
    /// Type `what` into the pane, after checking who will read it.
    ///
    /// A failed check writes nothing and is logged as `<op>.not_typed`;
    /// the caller gets the reason, as it gets a failed write.
    pub(super) fn type_into(&self, host: &dyn PaneSessionHost, what: Typed<'_>) -> Result<(), String> {
        let sid = host.shelld_session_id();
        if let Err(err) = self.reader_check() {
            host.log(LogLevel::Warn, &format!("{}.not_typed", self.op.name), &format!("pane {sid}: {err}"));
            return Err(err);
        }
        self.env.io().write(sid, what).map_err(|e| e.to_string())
    }

    /// The identities this run has for its reader, each checked now: the
    /// shell a step's line is for holds its terminal, and every program
    /// the run found is alive with the start time it had when found.
    /// A run that found nothing types to the pane as it is.  The awaited
    /// program is never forgotten, so a script must not type at a shell
    /// after terminating a program it awaited.
    fn reader_check(&self) -> Result<(), String> {
        if let Some(shell) = self.op.steps.get(self.at).and_then(|s| s.at_shell) {
            self.env.front(shell).shell_or_why()?;
        }
        for id in [self.watched, self.awaited].into_iter().flatten() {
            if !self.env.pid_alive(id) {
                return Err(format!("pid {} is no longer the program this run found", id.pid));
            }
        }
        Ok(())
    }
}

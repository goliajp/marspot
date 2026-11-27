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

use super::{OpRunner, PtyOp};
use crate::plugins::{LogLevel, PaneSessionHost};

/// What a run types: bytes as they are, or text delivered the way a
/// paste arrives (see [`PtyIo::paste`]).
#[derive(Clone, Copy, Debug)]
pub enum Typed<'a> {
    Bytes(&'a [u8]),
    Paste(&'a str),
}

/// Whose input a run types.  The core judges a program's input (it may
/// not send `/model` from Claude Code's box); marspot's own, such as the
/// command line an account switch types at the shell, it delivers as is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Origin {
    #[default]
    Marspot,
    /// A program outside marspot asked for it: a `--send`, an autorun.
    Program,
}

/// Bytes to a pane's PTY, and holds on its picture.
///
/// Implemented over whatever can reach the pane (in production, the
/// plugin host's inject-input channel).  Kept separate from
/// [`super::OpEnv`] so the process/clock half can be real while this
/// half is a fake.
pub trait PtyIo: Send + Sync {
    fn send(&self, sid: u64, bytes: &[u8]) -> std::io::Result<()>;
    /// Ask L3 to hold this pane's picture where it is (or release it).
    fn hold(&self, sid: u64, on: bool) -> std::io::Result<()>;
    /// Deliver text the way a paste would arrive.
    ///
    /// Not the same as [`send`](Self::send): only L3 knows whether the
    /// program in the pane has bracketed paste on, and multi-line text
    /// delivered without it is executed a line at a time.  Anything
    /// handing a *message* to a running program takes this route.
    fn paste(&self, sid: u64, text: &str) -> std::io::Result<()>;
    /// Typing a program outside marspot asked for, for the core to judge.
    fn program_input(&self, sid: u64, what: Typed<'_>) -> std::io::Result<()>;
    /// Tell L3 to stop believing the foreground program wants mouse
    /// reports.  Sent after a `terminate` step: a signal produces no
    /// bytes, so the `CSI ? 1002 l` a clean exit would have sent never
    /// arrives and the shell underneath inherits mouse tracking.
    fn reset_mouse_reporting(&self, sid: u64) -> std::io::Result<()>;
    /// Either kind of typing, by whose it is; what [`OpRunner::type_into`] calls.
    fn write(&self, sid: u64, what: Typed<'_>, origin: Origin) -> std::io::Result<()> {
        match (origin, what) {
            (Origin::Program, what) => self.program_input(sid, what),
            (Origin::Marspot, Typed::Bytes(b)) => self.send(sid, b),
            (Origin::Marspot, Typed::Paste(t)) => self.paste(sid, t),
        }
    }
}

impl PtyOp {
    /// The run types what a program outside marspot asked for.
    pub fn typed_for_program(mut self) -> Self {
        self.origin = Origin::Program;
        self
    }
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
        self.env.io().write(sid, what, self.op.origin).map_err(|e| e.to_string())
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

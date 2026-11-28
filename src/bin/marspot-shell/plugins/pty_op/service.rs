//! The queue every scripted pane operation goes through: one run at a
//! time per pane.

use std::sync::Arc;

use super::after_typed::{self, AfterTyped};
use super::{LogLevel, OpOutcome, OpRunner, PaneSession, PtyIo, PtyOp, RealEnv};

/// Identifies one submitted run, so a caller can recognise its own
/// outcome later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct OpId(pub u64);

/// What a finished run reports back.
#[derive(Debug, Clone)]
pub struct OpReport {
    pub id: OpId,
    pub sid: u64,
    pub name: &'static str,
    pub outcome: OpOutcome,
}

/// What the queue needs from whoever owns it.
///
/// Deliberately not `PluginHost`: that trait's `begin_pane_session`
/// checks the *current plugin's* permissions, and the queue has
/// submitters that are not plugins.  Routing through it denied the
/// first CLI delivery with `missing permission: PermissionSet(16)` (the
/// permission model that message came from is gone; see the plugins
/// module docs) —
/// the supervisor asking itself for permission it has no identity to
/// hold.  Plugins still submit through their host; starting the run is
/// the owner's business.
pub trait OpHost {
    fn begin(&self, sid: u64, session: Box<dyn PaneSession>) -> Result<(), String>;
    fn log(&self, level: LogLevel, tag: &str, msg: &str);
}

/// Any plugin host is an op host — that is how a plugin's own tests
/// (and the reclamation path) drive the queue without a supervisor.
impl<T: crate::plugins::PluginHost + ?Sized> OpHost for T {
    fn begin(&self, sid: u64, session: Box<dyn PaneSession>) -> Result<(), String> {
        crate::plugins::PluginHost::begin_pane_session(self, sid, session)
            .map_err(|e| e.to_string())
    }
    fn log(&self, level: LogLevel, tag: &str, msg: &str) {
        crate::plugins::PluginHost::log(self, level, tag, msg)
    }
}

/// Most runs that may wait their turn on one pane.
///
/// A pane is a single-threaded thing — one program, one keyboard — so
/// the queue exists to serialise, not to buffer.  Anything past this is
/// a caller in a loop, and dropping the newest with a loud line is a
/// better failure than growing forever.
pub(super) const MAX_QUEUED_PER_PANE: usize = 8;

/// The entry point for "do something to a pane".
///
/// Everything goes through here rather than each caller reaching for
/// `begin_pane_session` itself, because the invariant that matters
/// cannot be enforced anywhere else: **one run at a time per pane**.
/// Two scripts typing into the same PTY interleave their keystrokes,
/// and the result is neither of the commands they meant to send.
///
/// That invariant is the reason this exists now rather than when there
/// is a second caller: session-to-session delivery means ops aimed at
/// panes their sender does not own, arriving whenever the sender feels
/// like it, at panes that may already be mid-reclamation.
pub struct PtyOps {
    io: Arc<dyn PtyIo>,
    receipts: Option<Arc<crate::receipts::Receipts>>,
    queued: std::collections::HashMap<u64, std::collections::VecDeque<(OpId, PtyOp, usize)>>,
    active: std::collections::HashMap<u64, (OpId, &'static str)>,
    /// Where runners post their outcomes; drained by `pump`.
    sink: Arc<std::sync::Mutex<Vec<OpReport>>>,
    next_id: u64,
    after_typed: Option<AfterTyped>,
}

impl PtyOps {
    pub fn new(io: Arc<dyn PtyIo>) -> Self {
        Self {
            io,
            receipts: None,
            queued: std::collections::HashMap::new(),
            active: std::collections::HashMap::new(),
            sink: Arc::new(std::sync::Mutex::new(Vec::new())),
            next_id: 1,
            after_typed: None,
        }
    }

    /// Where panes report their own submits.
    ///
    /// Takes an `Option` because binding the socket is best-effort:
    /// without it every op reads the screen, which is what they all
    /// did before.
    pub fn with_receipts(mut self, receipts: Option<Arc<crate::receipts::Receipts>>) -> Self {
        self.receipts = receipts;
        self
    }

    /// Told of every line a run is about to type at a shell's prompt;
    /// what it hands back runs right after that line is typed.
    pub fn with_after_typed(mut self, hook: AfterTyped) -> Self {
        self.after_typed = Some(hook);
        self
    }

    /// Queue `op` against `sid`.  Runs immediately if the pane is free.
    ///
    /// `None` when the pane's queue is full; the caller decides whether losing it matters.
    pub fn submit(&mut self, sid: u64, op: PtyOp) -> Option<OpId> {
        self.submit_at(sid, op, 0)
    }

    /// Queue a run that starts partway in (a parked script re-armed in a new process).
    pub fn submit_at(&mut self, sid: u64, op: PtyOp, step: usize) -> Option<OpId> {
        let (op, step) = after_typed::insert(self.after_typed.as_ref(), sid, op, step);
        let q = self.queued.entry(sid).or_default();
        if q.len() >= MAX_QUEUED_PER_PANE {
            if let Some(f) = &op.on_end {
                f(&OpOutcome::Failed { step: 0, label: "queue", err: "the pane's queue is full".into() });
            }
            return None;
        }
        let id = OpId(self.next_id);
        self.next_id += 1;
        q.push_back((id, op, step));
        Some(id)
    }

    /// Forget closed panes: drop their queued runs and free their slot.
    pub fn retain(&mut self, open: impl Fn(u64) -> bool) {
        // a run that never started is still told it is over
        for (_, q) in self.queued.iter().filter(|(sid, _)| !open(**sid)) {
            for (_, op, _) in q {
                if let Some(f) = &op.on_end {
                    f(&OpOutcome::Cancelled("pane closed"));
                }
            }
        }
        self.queued.retain(|sid, _| open(*sid));
        self.active.retain(|sid, _| open(*sid));
    }

    /// Is a run in flight on this pane?
    pub fn is_busy(&self, sid: u64) -> bool {
        self.active.contains_key(&sid)
    }

    /// What is running on this pane, if anything.
    pub fn running(&self, sid: u64) -> Option<&'static str> {
        self.active.get(&sid).map(|(_, name)| *name)
    }

    /// Post an outcome as a runner would.  Test-only: exercising the
    /// queue's hand-off should not require driving a real script to
    /// completion.
    #[cfg(test)]
    pub fn report_for_test(&self, r: OpReport) {
        self.sink.lock().unwrap().push(r);
    }

    /// Start whatever can start, and collect whatever finished.
    ///
    /// Called once per plugin tick.  Returns the finished runs so the
    /// caller can do its own bookkeeping — this module has no opinion
    /// about what a completed op means.
    pub fn pump(&mut self, host: &dyn OpHost) -> Vec<OpReport> {
        let done: Vec<OpReport> = std::mem::take(&mut *self.sink.lock().unwrap());
        for r in &done {
            // Only clear the slot if the report belongs to the run that
            // holds it: a late report from a superseded run must not
            // free a pane someone else is using.
            if self.active.get(&r.sid).map(|(id, _)| *id) == Some(r.id) {
                self.active.remove(&r.sid);
            }
        }
        let mut free: Vec<u64> = self
            .queued
            .iter()
            .filter(|(sid, q)| !q.is_empty() && !self.active.contains_key(sid))
            .map(|(sid, _)| *sid)
            .collect();
        // Sorted, because the map's order is not one: two panes going
        // first in a different order on every tick would make the logs
        // — and any test of them — read as noise.
        free.sort_unstable();
        for sid in free {
            let Some((id, op, step)) = self.queued.get_mut(&sid).and_then(|q| q.pop_front()) else {
                continue;
            };
            let name = op.name;
            let notify = op.on_end.clone();
            let sink = Arc::clone(&self.sink);
            let runner = OpRunner::new(
                op,
                Box::new(RealEnv::with_receipts(
                    Arc::clone(&self.io),
                    self.receipts.clone(),
                )),
            )
                .start_at(step)
                .on_finish(move |_, outcome| {
                    sink.lock().unwrap().push(OpReport {
                        id,
                        sid,
                        name,
                        outcome: outcome.clone(),
                    });
                });
            match host.begin(sid, Box::new(runner)) {
                Ok(()) => {
                    self.active.insert(sid, (id, name));
                    host.log(
                        LogLevel::Info,
                        "pty_op.started",
                        &format!("{name} on pane {sid} (id={})", id.0),
                    );
                }
                Err(e) => {
                    host.log(
                        LogLevel::Warn,
                        "pty_op.begin_failed",
                        &format!("{name} on pane {sid}: {e}"),
                    );
                    let outcome = OpOutcome::Failed { step: 0, label: "begin", err: e };
                    if let Some(f) = &notify {
                        f(&outcome);
                    }
                    self.sink.lock().unwrap().push(OpReport { id, sid, name, outcome });
                }
            }
        }
        // Panes with nothing left to run are forgotten, so the maps are
        // bounded by what is actually in flight.
        self.queued.retain(|_, q| !q.is_empty());
        done
    }
}

//! Pane-control frames on their way to L1, kept until L1's queue takes
//! them.
//!
//! The writer to L1 refuses a frame when L1 has stopped draining, and a
//! refused frame is gone.  These ones are answers someone waits on, so a
//! refused one stays here, in order, and goes again on the next turn of
//! the main loop, which wakes for it on its own while any are held.
//!
//! Bounds.  Answers (`PaneTyped`, `PaneDone`, `PaneControlCaps`) are at
//! most `MAX_ANSWERS`; past that the oldest answer is dropped, logged:
//! L1 waits on each with a deadline of its own and answers its caller
//! `unknown` or asks again.  A `PaneWatchFired` is never dropped for an
//! answer.  A second one for the same session and watch replaces the
//! first: L1 has not seen the first, so it cannot have registered that
//! watch again, and the second is the same watch ending, reported
//! again.  A held "condition met" is the exception: it is kept and the
//! later one dropped, so a session ending or restarting right after a
//! watch fired cannot turn that result into something else.  Distinct ended watches are at most `MAX_FIRED`: L1 keeps a
//! watch in its table until it reads the report and cancels, and the
//! table holds 64 callers' watches and one per shadow pane, so a
//! working L1 cannot leave this many unread.  Reaching it means L1 is
//! not reading at all; the oldest report is then dropped, logged.

use std::collections::VecDeque;
use std::time::Duration;

use marspot::shell_proto::{Frame, MsgType};
use marspot::{lx_error, lx_warn};
use marspot_term::frame_writer::FrameWriter;

pub(super) const MAX_ANSWERS: usize = 64;
pub(super) const MAX_FIRED: usize = 512;
/// How soon a held frame is offered again when nothing else wakes the
/// loop; only while one is held.
pub(super) const RETRY: Duration = Duration::from_millis(50);

#[derive(Default)]
pub(super) struct Reports {
    queue: VecDeque<Frame>,
    /// The last flush stopped at a refused frame; logged once per stop.
    held: bool,
}

/// `(sid, watch_id)` of a `PaneWatchFired`.
fn fired_key(f: &Frame) -> Option<(u64, u32)> {
    if f.msg_type != MsgType::PaneWatchFired {
        return None;
    }
    let sid = u64::from_le_bytes(*f.payload.first_chunk::<8>()?);
    let watch = u32::from_le_bytes(*f.payload.get(8..12)?.first_chunk::<4>()?);
    Some((sid, watch))
}

/// A `PaneWatchFired` whose reason is "the condition was met".
fn met(f: &Frame) -> bool {
    f.payload.get(12) == Some(&1)
}

impl Reports {
    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.queue.len()
    }

    pub(super) fn push(&mut self, f: Frame) {
        let fired = f.msg_type == MsgType::PaneWatchFired;
        if let Some(key) = fired_key(&f)
            && let Some(slot) = self.queue.iter_mut().find(|q| fired_key(q) == Some(key))
        {
            if !met(slot) {
                *slot = f;
            }
            return;
        }
        let same_kind = |q: &Frame| (q.msg_type == MsgType::PaneWatchFired) == fired;
        let cap = if fired { MAX_FIRED } else { MAX_ANSWERS };
        if self.queue.iter().filter(|q| same_kind(q)).count() >= cap
            && let Some(i) = self.queue.iter().position(same_kind)
            && let Some(old) = self.queue.remove(i)
        {
            lx_error!(
                "core.pane_control.report_dropped",
                "L1 has read none of the held pane-control frames; the oldest is dropped",
                msg_type = format!("{:?}", old.msg_type),
                held = self.queue.len()
            );
        }
        self.queue.push_back(f);
    }

    /// Hand L1's writer every held frame it takes, in order; returns how
    /// many it took.
    pub(super) fn flush(&mut self, w: &FrameWriter) -> usize {
        let mut sent = 0;
        while let Some(f) = self.queue.front() {
            if !w.send(f.clone()) {
                if !std::mem::replace(&mut self.held, true) {
                    lx_warn!(
                        "core.pane_control.report_held",
                        "L1 not draining the control socket; pane-control frames held for the next turn",
                        held = self.queue.len(),
                        backlog = w.backlog()
                    );
                }
                return sent;
            }
            self.queue.pop_front();
            sent += 1;
        }
        self.held = false;
        sent
    }

    #[cfg(test)]
    pub(super) fn frames(&self) -> impl Iterator<Item = &Frame> {
        self.queue.iter()
    }

    /// The loop's wait while frames are held.
    pub(super) fn retry_after(&self) -> Option<Duration> {
        (!self.queue.is_empty()).then_some(RETRY)
    }
}

#[cfg(test)]
#[path = "pane_reports_tests.rs"]
mod tests;

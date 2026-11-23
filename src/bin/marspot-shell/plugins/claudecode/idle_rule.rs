//! The rule that decides whether a pane's claude is reclaimed, and the
//! reason it gives when it is not.

use std::time::Duration;

use super::idle::{CPU_BASELINE_WINDOW, IDLE_CPU_TOLERANCE_NS};

/// Everything the idle decision looks at, gathered so the rule itself
/// stays a pure function.
#[derive(Clone, Copy, Debug)]
pub(super) struct IdleEvidence {
    /// The state machine says nothing is in flight AND has held that
    /// view long enough to be believed (`CONFIRM_TICKS`).
    pub(super) quiescent: bool,
    /// The composed state is specifically "a program is bound and
    /// waiting for the user".  `Empty` is quiet too, but it means
    /// there is no claude here to reclaim.
    pub(super) awaiting_user: bool,
    /// How long that state has held.
    ///
    /// Kept for the log, no longer the clock that decides.  See
    /// `idle_for`.
    pub(super) held: Duration,
    /// How long since the session itself did anything — the age of its
    /// transcript.
    ///
    /// This is the clock that decides.  The pane's own quiet clock
    /// cannot be: claude writes `Checking for updates` into the corner
    /// every thirty minutes, which makes the terminal busy for half a
    /// minute and resets it.  Measured here: 23 of those in one pane's
    /// log, so a session idle for eleven hours never once accumulated
    /// thirty quiet minutes, and reclamation could not fire at all.
    pub(super) idle_for: Duration,
    /// CPU the claude subtree consumed since the previous sample, and
    /// how long ago that sample was taken.
    pub(super) cpu_delta_ns: u64,
    pub(super) since_sample: Duration,
    /// A process of the session's own is running — a background task,
    /// a watcher, a build.  CPU says nothing about these: measured on
    /// a live session, `zsh → cargo-fuzz + tail` sat at 0.0 %.
    pub(super) work_in_flight: bool,
    /// The session is waiting on a timer it set itself (a `/loop`
    /// autorun), not on the user.
    pub(super) own_timer: bool,
    /// The user's cursor is in this pane right now.
    ///
    /// The one thing the session's own clock cannot see.  A pane can
    /// be half an hour idle by its transcript while the user sits in
    /// it reading the last answer, and reclaiming it there charges
    /// them a three-second wake for their next keystroke — with no
    /// warning, because a reclamation is deliberately invisible.
    pub(super) user_here: bool,
    /// The pane is a helper, or helpers are working for it.  Either
    /// rests while it waits on the other, and nothing would bring a
    /// reclaimed helper back.
    pub(super) helper_bound: bool,
}

/// Why a candidate pane was not reclaimed on this pass, as
/// `(category, line)`.
///
/// The category is what the caller de-duplicates on, and it is
/// deliberately value-free.  The first version returned one string
/// containing the current idle seconds — which changes every scan, so
/// "log when the reason changes" logged **every** scan: 3030 lines in
/// 25 minutes on this machine, about 1 MB/hour of noise that would
/// rotate the real history out of an 8 MB log in under a day.
///
/// Only computed for panes the machine already calls quiet and
/// awaiting their user — i.e. ones that are *going* to be reclaimed
/// once something finishes ticking.  Anything busier is not a
/// candidate and has nothing to explain.
pub(super) fn blocking_reason(
    e: IdleEvidence,
    threshold: Duration,
) -> Option<(&'static str, String)> {
    if should_hibernate(e, threshold) {
        return None;
    }
    if !(e.quiescent && e.awaiting_user) {
        return None; // not a candidate; not this log's business
    }
    if e.helper_bound {
        return Some(("helper_bound", "it is a helper or helpers work for it".to_string()));
    }
    if e.user_here {
        return Some((
            "user_here",
            "the user's cursor is in this pane".to_string(),
        ));
    }
    if e.work_in_flight {
        return Some((
            "work_in_flight",
            "a process of its own is running".to_string(),
        ));
    }
    if e.own_timer {
        return Some((
            "own_timer",
            "waiting on a timer it set itself".to_string(),
        ));
    }
    Some(if e.idle_for < threshold {
        (
            "below_threshold",
            format!(
                "idle {}s of {}s (terminal quiet {}s)",
                e.idle_for.as_secs(),
                threshold.as_secs(),
                e.held.as_secs()
            ),
        )
    } else if e.since_sample < Duration::from_secs(30) {
        (
            "short_cpu_sample",
            format!("cpu sample spans only {}s", e.since_sample.as_secs()),
        )
    } else {
        (
            "cpu_busy",
            format!(
                "subtree burned {}ms of cpu in {}s",
                e.cpu_delta_ns / 1_000_000,
                e.since_sample.as_secs()
            ),
        )
    })
}

/// May this pane's claude be reclaimed right now?
///
/// Deliberately conjunctive and default-deny: every unknown answers
/// false.  The expensive mistake is killing a session that was doing
/// something (an unanswered tool call, a background task, a turn in
/// flight); the cheap mistake is leaving memory on the table for
/// another hour.
pub(super) fn should_hibernate(e: IdleEvidence, threshold: Duration) -> bool {
    e.quiescent
        && e.awaiting_user
        // Never the seat the user is in.  Everything else here is
        // about the session; this is the one clause about the person.
        && !e.user_here
        && !e.helper_bound
        // Two vetoes the clock cannot see: something of the session's
        // own is running, or the session is waiting on its own timer.
        // Both mean the pane is not resting, it is between steps.
        && !e.work_in_flight
        && !e.own_timer
        // The session's own clock, not the terminal's.  `held` resets
        // every time anything writes to the pane, and something does:
        // claude's thirty-minute update check.  With a thirty-minute
        // threshold that is a race the check always wins — measured
        // repeatedly at `held_s=1767`, thirty-three seconds short.
        && e.idle_for >= threshold
        // A CPU sample only means something once it spans real time;
        // the first sample after a restart spans none.  Half the
        // baseline window, so a pane becomes eligible partway through
        // one rather than having to wait for a full fresh one.
        && e.since_sample >= CPU_BASELINE_WINDOW / 2
        && e.cpu_delta_ns <= IDLE_CPU_TOLERANCE_NS
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: Duration = Duration::from_secs(3600);

    fn eligible() -> IdleEvidence {
        IdleEvidence {
            quiescent: true,
            awaiting_user: true,
            held: HOUR * 2,
            idle_for: HOUR * 2,
            cpu_delta_ns: 0,
            since_sample: Duration::from_secs(120),
            work_in_flight: false,
            own_timer: false,
            user_here: false,
            helper_bound: false,
        }
    }

    /// A helper waiting for its next message and a pane waiting for its
    /// helpers' results both read as hours idle.
    #[test]
    fn idle_reclaim_leaves_helpers_and_the_panes_they_work_for() {
        assert!(should_hibernate(eligible(), HOUR));
        let bound = IdleEvidence { helper_bound: true, ..eligible() };
        assert!(!should_hibernate(bound, HOUR));
        assert_eq!(blocking_reason(bound, HOUR).map(|(cat, _)| cat), Some("helper_bound"));
    }
}

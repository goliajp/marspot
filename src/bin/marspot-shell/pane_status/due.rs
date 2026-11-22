//! When the pane state machines next need a sweep.
//!
//! A sweep reads each pane's terminal owner, its jobs and the size of
//! its output log.  Between two sweeps a pane's state can move for two
//! kinds of reason: something happened (it printed, its terminal changed
//! hands, a plugin's report changed, input was sent to it), or time
//! passed (a quiet candidate waits for its next confirming look, a
//! printing pane's output reaches `PTY_QUIET_AFTER`, a finished turn
//! reaches `RESTING_AFTER`).  The first kind ask for a pass when they
//! happen; the second kind are deadlines, and this is where they are
//! read.  A pane whose state can move without either, or whose session
//! does not report, is swept every `SWEEP_INTERVAL`, as every pane was.

use std::time::Instant;

use marspot::pane_state::{BusyKind, PaneMachine, PaneStatus};

use super::{OUTPUT_REPORTED_EVERY, PTY_QUIET_AFTER, PaneStateTracker, RESTING_AFTER, SWEEP_INTERVAL};

/// What one pane needs from the sweeps.
#[derive(Debug, PartialEq, Eq)]
enum PaneDue {
    /// One every `SWEEP_INTERVAL`.
    Beat,
    /// One at this moment.
    At(Instant),
    /// None until something happens.
    Event,
}

impl PaneStateTracker {
    /// When the next sweep is due, given the sessions the registry
    /// lists: None while nothing but an event can move any pane.  Never
    /// sooner than `SWEEP_INTERVAL` after the last sweep.
    pub fn due(&self, sessions: impl Iterator<Item = u64>) -> Option<Instant> {
        let Some(last) = self.last_sweep else { return Some(Instant::now()) };
        let beat = last + SWEEP_INTERVAL;
        let mut listed = 0;
        for sid in sessions {
            listed += 1;
            if !self.machines.contains_key(&sid) {
                return Some(beat);
            }
        }
        if self.dirty || listed != self.machines.len() {
            return Some(beat);
        }
        let mut due: Option<Instant> = None;
        for (sid, m) in &self.machines {
            match self.pane_due(*sid, m, last) {
                PaneDue::Beat => return Some(beat),
                PaneDue::At(t) => due = Some(due.map_or(t, |d| d.min(t))),
                PaneDue::Event => {}
            }
        }
        due.map(|t| t.max(beat))
    }

    fn pane_due(&self, sid: u64, m: &PaneMachine, last: Instant) -> PaneDue {
        // a session that does not report is looked at as before
        let Some(output_at) = (self.reports)(sid) else { return PaneDue::Beat };
        if m.pending() || self.stat_failing.contains(&sid) {
            return PaneDue::Beat;
        }
        match m.status() {
            // a background job can end, and a pane nobody can read can
            // become readable, without a word on the terminal
            PaneStatus::Unknown
            | PaneStatus::Contradiction { .. }
            | PaneStatus::Busy(BusyKind::StoppedJobs | BusyKind::BackgroundJobs) => return PaneDue::Beat,
            _ => {}
        }
        let mut due = None;
        if let Some((_, since)) = self.pty_bytes.get(&sid) {
            let quiet_at = *since + PTY_QUIET_AFTER;
            if quiet_at <= last {
                // quiet when last swept: output since then is news the
                // pass that asked for it may not have swept yet
                if output_at.is_some_and(|r| r > last) {
                    return PaneDue::Beat;
                }
            } else {
                let ended_by = output_at.map_or(*since, |r| (*since).max(r + OUTPUT_REPORTED_EVERY));
                due = Some(ended_by + PTY_QUIET_AFTER);
            }
        }
        if *m.status() == PaneStatus::AwaitingUser && m.age(last) < RESTING_AFTER {
            let resting_at = last + (RESTING_AFTER - m.age(last));
            due = Some(due.map_or(resting_at, |d: Instant| d.min(resting_at)));
        }
        due.map_or(PaneDue::Event, PaneDue::At)
    }

    /// Whether `sid`'s terminal counts as quiet at `now`: its next output
    /// is a change of state.
    pub fn pty_quiet_now(&self, sid: u64, now: Instant) -> bool {
        self.pty_bytes.get(&sid).is_none_or(|(_, since)| now.saturating_duration_since(*since) >= PTY_QUIET_AFTER)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot::pane_state::{Activity, Generic};
    use std::sync::Mutex;
    use std::time::Duration;

    /// The last output each test pane reported; a pane missing from it
    /// has never reported.
    static REPORTS: Mutex<Vec<(u64, Option<Instant>)>> = Mutex::new(Vec::new());

    fn reports(sid: u64) -> Option<Option<Instant>> {
        REPORTS.lock().unwrap().iter().find(|(s, _)| *s == sid).map(|(_, at)| *at)
    }

    fn reported(sid: u64, at: Option<Instant>) {
        let mut r = REPORTS.lock().unwrap();
        r.retain(|(s, _)| *s != sid);
        r.push((sid, at));
    }

    fn fg() -> Generic {
        Generic::Foreground { pgid: 600, leader: None }
    }

    /// A tracker over panes `sids` with no bytelog behind them, swept once
    /// at `t0` with a job in front of every pane and no program bound.
    fn tracker(sids: &[u64], t0: Instant) -> PaneStateTracker {
        let mut t = PaneStateTracker::new();
        t.read_reports(reports);
        for sid in sids {
            t.report_activity(*sid, Activity::Absent);
        }
        t.sweep_with(sids.iter().map(|s| (*s, 100)).collect(), t0, |_| fg());
        t
    }

    fn ids(t: &PaneStateTracker) -> impl Iterator<Item = u64> + '_ {
        t.machines.keys().copied()
    }

    #[test]
    fn a_pane_whose_session_never_reported_is_swept_every_second() {
        let t0 = Instant::now();
        let t = tracker(&[901], t0);
        assert_eq!(t.due(ids(&t)), Some(t0 + SWEEP_INTERVAL));
    }

    #[test]
    fn a_reporting_pane_that_can_only_change_on_an_event_asks_for_no_sweep() {
        let t0 = Instant::now();
        reported(902, None);
        let mut t = tracker(&[902], t0);
        // no bytelog: always quiet; a job in front: busy at once
        assert_eq!(t.machines[&902].status(), &PaneStatus::Busy(BusyKind::ForegroundJob));
        assert_eq!(t.due(ids(&t)), None);
        t.poke();
        assert_eq!(t.due(ids(&t)), Some(t0 + SWEEP_INTERVAL), "input sent to a pane is looked at within a second");
    }

    #[test]
    fn a_quiet_candidate_waits_one_second_for_each_confirming_look() {
        let t0 = Instant::now();
        reported(903, None);
        let mut t = tracker(&[903], t0);
        t.sweep_with(vec![(903, 100)], t0 + SWEEP_INTERVAL, |_| Generic::Idle);
        assert!(t.machines[&903].pending(), "idle is quiet, so it waits");
        assert_eq!(t.due(ids(&t)), Some(t0 + SWEEP_INTERVAL * 2));
        for i in 2..=3 {
            t.sweep_with(vec![(903, 100)], t0 + SWEEP_INTERVAL * i, |_| Generic::Idle);
        }
        assert_eq!(t.machines[&903].status(), &PaneStatus::Empty);
        assert_eq!(t.due(ids(&t)), None, "confirmed: nothing left to wait for");
    }

    #[test]
    fn a_printing_pane_is_next_due_when_its_last_output_can_have_gone_quiet() {
        let t0 = Instant::now();
        reported(904, None);
        let mut t = tracker(&[904], t0);
        t.pty_bytes.insert(904, (10, t0));
        assert_eq!(t.due(ids(&t)), Some(t0 + PTY_QUIET_AFTER));
        // it went on printing: the date moves to a second past its last report
        let r = t0 + Duration::from_secs(12);
        reported(904, Some(r));
        assert_eq!(t.due(ids(&t)), Some(r + OUTPUT_REPORTED_EVERY + PTY_QUIET_AFTER));
        // the sweep at that moment dates the growth it finds by the report
        let at = r + OUTPUT_REPORTED_EVERY + PTY_QUIET_AFTER;
        assert!(t.pty_quiet_with(904, at, Ok(20)), "quiet within a second of the last output");
    }

    #[test]
    fn output_from_a_pane_that_was_quiet_is_swept_at_once() {
        let t0 = Instant::now();
        reported(905, None);
        let mut t = tracker(&[905], t0);
        t.pty_bytes.insert(905, (10, t0 - PTY_QUIET_AFTER));
        assert_eq!(t.due(ids(&t)), None);
        reported(905, Some(t0 + Duration::from_millis(300)));
        assert_eq!(t.due(ids(&t)), Some(t0 + SWEEP_INTERVAL));
    }

    #[test]
    fn a_finished_turn_is_next_due_when_it_starts_to_recede() {
        let t0 = Instant::now();
        reported(906, None);
        let mut t = tracker(&[906], t0);
        t.report_activity(906, Activity::AwaitingUser);
        for i in 1..=3 {
            t.sweep_with(vec![(906, 100)], t0 + SWEEP_INTERVAL * i, |_| fg());
        }
        let entered = t0 + SWEEP_INTERVAL * 3;
        assert_eq!(t.machines[&906].status(), &PaneStatus::AwaitingUser);
        assert_eq!(t.due(ids(&t)), Some(entered + RESTING_AFTER));
    }

    #[test]
    fn a_pane_with_background_jobs_and_a_pane_the_registry_added_are_swept_every_second() {
        let t0 = Instant::now();
        reported(907, None);
        let mut t = PaneStateTracker::new();
        t.read_reports(reports);
        t.sweep_with(vec![(907, 100)], t0, |_| Generic::PromptWithJobs { stopped: 0, running: 1 });
        assert_eq!(t.due(ids(&t)), Some(t0 + SWEEP_INTERVAL));
        let t = tracker(&[907], t0);
        assert_eq!(t.due([907, 908].into_iter()), Some(t0 + SWEEP_INTERVAL));
    }
}

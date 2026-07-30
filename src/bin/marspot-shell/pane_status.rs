//! Per-pane foreground status — the generic layer of "what is this
//! pane doing", swept in L1 and handed to plugins.
//!
//! The signal itself is `marspot::pidtree::pane_foreground_probe`:
//! kernel-side, no shell cooperation, 1–2 syscalls per pane.  This
//! module is only the bookkeeping around it — when to sweep, what
//! changed since last time, and what to forget when a pane closes.
//!
//! Why L1 and not L2: the consumers are plugins (claudecode refines
//! `Job` into its own states from the session jsonl; a hibernation
//! policy would gate on "no job, nothing under it"), and L1 can read
//! the session registry directly, so computing it here costs no wire
//! hop.  L2 does its own per-second `resolve_pane_cwd` sweep for the
//! pane title, which is a different signal with a different consumer.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use marspot::pidtree::{self, PaneForeground};

/// How often the status is re-probed.  Rides `poll_supervisor`'s
/// ~250 ms cadence but gates itself to this, so the syscall cost is
/// ~2 per pane per second — the same order as L2's cwd sweep, and
/// nothing like `list_all_procs`' one-per-pid-on-the-host.
///
/// The states this feeds are human-scale (a job starts, a job ends, a
/// session goes idle for an hour); sampling faster would buy nothing
/// and print a busier log.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// One observed change, for the caller to log.  Logging lives in the
/// caller so the sweep stays a pure-ish function that tests can drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub sid: u64,
    /// None = first observation of this pane.
    pub prev: Option<PaneForeground>,
    pub next: PaneForeground,
}

/// Last-known foreground per live session, plus the sweep clock.
///
/// Bounded by construction: every sweep replaces the map's key set
/// with the sessions the registry currently lists, so a pane that
/// closes is forgotten on the next tick rather than accumulating for
/// the lifetime of the shell (CLAUDE.md §3).
pub struct PaneStatusTracker {
    last: HashMap<u64, PaneForeground>,
    last_sweep: Option<Instant>,
}

impl PaneStatusTracker {
    pub fn new() -> Self {
        Self { last: HashMap::new(), last_sweep: None }
    }

    /// Current view — what `PluginHost::pane_status` serves.
    pub fn snapshot(&self) -> HashMap<u64, PaneForeground> {
        self.last.clone()
    }

    /// Probe every live session, at most once per `SWEEP_INTERVAL`.
    ///
    /// `None` = the interval gate skipped this call; `Some(vec)` = a
    /// sweep ran and these are its transitions (possibly empty).  The
    /// caller needs the distinction: "ran, nothing changed" still has
    /// to re-publish, because a pane *closing* drops a key without
    /// producing a transition — treating an empty vec as "nothing to
    /// do" would leave the closed pane's last status readable forever.
    pub fn sweep(&mut self) -> Option<Vec<Transition>> {
        if self
            .last_sweep
            .is_some_and(|t| t.elapsed() < SWEEP_INTERVAL)
        {
            return None;
        }
        self.last_sweep = Some(Instant::now());
        let panes: Vec<(u64, i32)> =
            marspot_term::session_registry::list_session_entries()
                .into_iter()
                .map(|e| (e.id, e.shell_child_pid))
                .collect();
        Some(self.sweep_with(panes, pidtree::pane_foreground_probe))
    }

    /// Sweep body, with the pane list and the probe injected — the
    /// transition + forgetting logic is what's worth testing, and it
    /// shouldn't need real panes on a real tty to test.
    pub fn sweep_with<F>(
        &mut self,
        panes: Vec<(u64, i32)>,
        mut probe: F,
    ) -> Vec<Transition>
    where
        F: FnMut(i32) -> PaneForeground,
    {
        let mut next_map: HashMap<u64, PaneForeground> =
            HashMap::with_capacity(panes.len());
        let mut transitions = Vec::new();
        for (sid, shell_pid) in panes {
            let next = probe(shell_pid);
            let prev = self.last.get(&sid);
            if prev != Some(&next) {
                transitions.push(Transition {
                    sid,
                    prev: prev.cloned(),
                    next: next.clone(),
                });
            }
            next_map.insert(sid, next);
        }
        // Replace rather than merge: sessions that vanished from the
        // registry are gone, and their last status is not news.
        self.last = next_map;
        transitions
    }
}

/// Log form of a status, short enough to sit in a TSV field.
/// `job:<comm>` deliberately carries `pbi_comm` and not a resolved
/// program name — resolving would cost a `proc_cmdline` per pane per
/// sweep, and for claude `comm` is a version string anyway (see
/// `pidtree::JobLeader::comm`).  Reading the log, `job:2.1.220` is
/// still the answer to "is something running".
pub fn describe(fg: &PaneForeground) -> String {
    match fg {
        PaneForeground::AtPrompt => "prompt".to_string(),
        PaneForeground::Job { pgid, leader: Some(l) } => {
            format!("job:{} pid={} pgid={}", l.comm, l.pid, pgid)
        }
        PaneForeground::Job { pgid, leader: None } => {
            format!("job:? pgid={}", pgid)
        }
        PaneForeground::Unknown => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot::pidtree::JobLeader;

    fn job(pid: i32, comm: &str) -> PaneForeground {
        PaneForeground::Job {
            pgid: pid,
            leader: Some(JobLeader { pid, comm: comm.into(), start_unix: 1000 }),
        }
    }

    #[test]
    fn first_sweep_reports_every_pane_as_a_transition_from_none() {
        let mut t = PaneStatusTracker::new();
        let out = t.sweep_with(vec![(1, 100), (2, 200)], |pid| match pid {
            100 => PaneForeground::AtPrompt,
            _ => job(201, "claude"),
        });
        assert_eq!(out.len(), 2, "both panes are news on first sight");
        assert!(out.iter().all(|tr| tr.prev.is_none()));
    }

    #[test]
    fn unchanged_panes_produce_no_transitions() {
        let mut t = PaneStatusTracker::new();
        let panes = vec![(1, 100)];
        t.sweep_with(panes.clone(), |_| PaneForeground::AtPrompt);
        let out = t.sweep_with(panes, |_| PaneForeground::AtPrompt);
        assert!(out.is_empty(), "steady state must be silent, got {out:?}");
    }

    #[test]
    fn a_job_starting_and_ending_are_both_transitions() {
        let mut t = PaneStatusTracker::new();
        let panes = vec![(7, 700)];
        t.sweep_with(panes.clone(), |_| PaneForeground::AtPrompt);
        let started = t.sweep_with(panes.clone(), |_| job(701, "cargo"));
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].prev, Some(PaneForeground::AtPrompt));
        let ended = t.sweep_with(panes, |_| PaneForeground::AtPrompt);
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].next, PaneForeground::AtPrompt);
    }

    /// Same job, different leader pid = a real transition: this is how
    /// "claude was restarted under the same pane" shows up.
    #[test]
    fn a_replaced_leader_is_a_transition_even_with_the_same_comm() {
        let mut t = PaneStatusTracker::new();
        let panes = vec![(3, 300)];
        t.sweep_with(panes.clone(), |_| job(301, "claude"));
        let out = t.sweep_with(panes, |_| job(999, "claude"));
        assert_eq!(out.len(), 1, "new pid under the same name is news");
    }

    #[test]
    fn closed_panes_are_forgotten_so_the_map_stays_bounded() {
        let mut t = PaneStatusTracker::new();
        t.sweep_with(vec![(1, 100), (2, 200)], |_| PaneForeground::AtPrompt);
        assert_eq!(t.snapshot().len(), 2);
        t.sweep_with(vec![(2, 200)], |_| PaneForeground::AtPrompt);
        let snap = t.snapshot();
        assert_eq!(snap.len(), 1, "pane 1 closed; its entry must not linger");
        assert!(snap.contains_key(&2));
    }

    /// A pane that reappears after being forgotten reads as first
    /// sight again — better than silently treating stale state as
    /// current.
    #[test]
    fn a_returning_sid_is_reported_as_first_sight() {
        let mut t = PaneStatusTracker::new();
        t.sweep_with(vec![(5, 500)], |_| PaneForeground::AtPrompt);
        t.sweep_with(vec![], |_| PaneForeground::AtPrompt);
        let out = t.sweep_with(vec![(5, 500)], |_| PaneForeground::AtPrompt);
        assert_eq!(out.len(), 1);
        assert!(out[0].prev.is_none());
    }

    #[test]
    fn interval_gate_skips_a_second_sweep_inside_the_window() {
        let mut t = PaneStatusTracker::new();
        // `sweep` (not `sweep_with`) owns the clock; drive it twice and
        // assert the second call is gated by checking the clock moved
        // only once.  The registry may be empty in a test process —
        // that's fine, the gate is what's under test.
        t.sweep();
        let first = t.last_sweep;
        t.sweep();
        assert_eq!(first, t.last_sweep, "second sweep inside the window must be a no-op");
    }

    #[test]
    fn describe_covers_every_variant() {
        assert_eq!(describe(&PaneForeground::AtPrompt), "prompt");
        assert_eq!(describe(&PaneForeground::Unknown), "unknown");
        assert_eq!(
            describe(&job(42, "vim")),
            "job:vim pid=42 pgid=42",
        );
        assert_eq!(
            describe(&PaneForeground::Job { pgid: 9, leader: None }),
            "job:? pgid=9",
        );
    }
}

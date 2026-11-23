//! What the sessions say happened in their panes (`PaneAwake`), kept
//! for the supervisor: when each pane last printed, when its terminal
//! last changed hands and to which process group.  The reader of the
//! core's frames records each report and knocks the pump only when the
//! report can change what the pane state machine concludes: the first
//! report from a pane, the terminal changing hands, and output from a
//! pane whose terminal the machine last found quiet (`arm`).  Output
//! from a pane that is already printing only moves the moment its
//! terminal can next count as quiet, which the supervisor reads from
//! here when it plans its next pass.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use marspot::shell_proto::PaneAwake;

/// More than the panes an app can have; past it the pane heard from
/// longest ago is dropped.
const PANES_MAX: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PaneWake {
    /// The last report of output.
    pub(crate) output_at: Option<Instant>,
    /// The last report of the terminal changing hands.
    pub(crate) foreground_at: Option<Instant>,
    /// The foreground group as of the last report; 0 = none.
    pub(crate) fg_pgid: i32,
    pub(crate) shell_in_front: bool,
    pub(crate) heard_at: Instant,
    /// The supervisor wants to hear this pane's next output at once.
    armed: bool,
}

#[derive(Default)]
pub(crate) struct Wakes {
    panes: Vec<(u64, PaneWake)>,
}

impl Wakes {
    /// Whether the report is worth a pass: see the module doc.
    fn record(&mut self, a: &PaneAwake, now: Instant) -> bool {
        let (i, first) = match self.panes.iter().position(|(sid, _)| *sid == a.session_id) {
            Some(i) => (i, false),
            None => {
                if self.panes.len() >= PANES_MAX
                    && let Some(oldest) = (0..self.panes.len()).min_by_key(|&i| self.panes[i].1.heard_at)
                {
                    self.panes.swap_remove(oldest);
                }
                let blank = PaneWake {
                    output_at: None,
                    foreground_at: None,
                    fg_pgid: 0,
                    shell_in_front: false,
                    heard_at: now,
                    armed: false,
                };
                self.panes.push((a.session_id, blank));
                (self.panes.len() - 1, true)
            }
        };
        let w = &mut self.panes[i].1;
        w.heard_at = now;
        if a.output {
            w.output_at = Some(now);
        }
        if a.foreground {
            w.foreground_at = Some(now);
        }
        w.fg_pgid = a.fg_pgid;
        w.shell_in_front = a.shell_in_front;
        let woke = a.output && w.armed;
        if woke {
            w.armed = false;
        }
        first || a.foreground || woke
    }

    fn latest(&self, sid: u64) -> Option<PaneWake> {
        self.panes.iter().find(|(s, _)| *s == sid).map(|(_, w)| *w)
    }

    fn retain(&mut self, open: impl Fn(u64) -> bool, since: Instant) {
        self.panes.retain(|(sid, w)| open(*sid) || w.heard_at >= since);
    }

    fn arm(&mut self, sid: u64, on: bool) {
        if let Some((_, w)) = self.panes.iter_mut().find(|(s, _)| *s == sid) {
            w.armed = on;
        }
    }
}

static WAKES: Mutex<Wakes> = Mutex::new(Wakes { panes: Vec::new() });
static WOKE: AtomicU64 = AtomicU64::new(0);

fn wakes() -> std::sync::MutexGuard<'static, Wakes> {
    WAKES.lock().unwrap_or_else(|p| p.into_inner())
}

/// Keep `a`, from the reader thread; true when the supervisor should
/// make a pass for it.
pub(crate) fn record(a: &PaneAwake) -> bool {
    let woke = wakes().record(a, Instant::now());
    if woke {
        WOKE.fetch_add(1, Ordering::Release);
    }
    if a.foreground {
        marspot::lx_debug!(
            "shell.pane_awake.foreground",
            "a pane's terminal changed hands",
            sid = a.session_id,
            fg_pgid = a.fg_pgid,
            shell_in_front = a.shell_in_front as u32
        );
    }
    woke
}

/// What pane `sid` last reported.
pub(crate) fn latest(sid: u64) -> Option<PaneWake> {
    wakes().latest(sid)
}

/// How many reports have asked for a pass: a pass compares it with the
/// count it saw last to tell whether one has since.
pub(crate) fn woke() -> u64 {
    WOKE.load(Ordering::Acquire)
}

/// Whether pane `sid`'s next output asks for a pass.
pub(crate) fn arm(sid: u64, on: bool) {
    wakes().arm(sid, on);
}

/// Forget the panes `open` says are gone, except those heard from at
/// or after `since`: a session reports as soon as its core connects,
/// which can be before the registry view the sweeps read lists it, and
/// a report dropped then is the only one a quiet pane ever sends.
pub(crate) fn retain(open: impl Fn(u64) -> bool, since: Instant) {
    wakes().retain(open, since);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn awake(sid: u64, output: bool, foreground: bool, fg: i32) -> PaneAwake {
        PaneAwake { session_id: sid, output, foreground, shell_in_front: false, fg_pgid: fg }
    }

    #[test]
    fn a_report_keeps_what_it_says_and_what_came_before() {
        let mut w = Wakes::default();
        let t0 = Instant::now();
        assert!(w.record(&awake(1, false, true, 50), t0));
        let t1 = t0 + Duration::from_secs(1);
        assert!(!w.record(&awake(1, true, false, 50), t1), "output from a pane nobody armed");
        let got = w.latest(1).unwrap();
        assert_eq!((got.output_at, got.foreground_at, got.fg_pgid), (Some(t1), Some(t0), 50));
        assert!(w.latest(2).is_none());
    }

    /// A pane heard from before the sweeps list it keeps its report
    /// through a sweep that does not list it yet; one long gone does not.
    #[test]
    fn a_report_from_a_pane_not_yet_listed_survives_a_sweep() {
        let mut w = Wakes::default();
        let t0 = Instant::now();
        w.record(&awake(1, false, true, 50), t0);
        w.record(&awake(2, false, true, 60), t0 + Duration::from_secs(10));
        w.retain(|_| false, t0 + Duration::from_secs(5));
        assert!(w.latest(1).is_none(), "heard before `since` and not listed");
        assert!(w.latest(2).is_some(), "heard since, not listed yet");
        w.retain(|sid| sid == 2, t0 + Duration::from_secs(60));
        assert!(w.latest(2).is_some(), "listed");
    }

    /// Only the first output after the supervisor armed a pane asks for
    /// a pass, and so do the first report and a change of hands.
    #[test]
    fn output_asks_for_a_pass_only_from_an_armed_pane() {
        let mut w = Wakes::default();
        let t0 = Instant::now();
        assert!(w.record(&awake(1, true, false, 0), t0), "the first report");
        assert!(!w.record(&awake(1, true, false, 0), t0));
        w.arm(1, true);
        assert!(w.record(&awake(1, true, false, 0), t0));
        assert!(!w.record(&awake(1, true, false, 0), t0), "disarmed by the output it woke for");
        w.arm(1, true);
        assert!(!w.record(&awake(1, false, false, 0), t0), "a report without output keeps the arm");
        assert!(w.record(&awake(1, true, false, 0), t0));
        assert!(w.record(&awake(1, false, true, 7), t0), "the terminal changed hands");
        w.arm(2, true);
        assert!(w.latest(2).is_none(), "arming a pane that never reported adds nothing");
    }

    #[test]
    fn the_table_is_bounded_and_drops_the_pane_heard_from_longest_ago() {
        let mut w = Wakes::default();
        let t0 = Instant::now();
        for sid in 0..PANES_MAX as u64 {
            w.record(&awake(sid, true, false, 0), t0 + Duration::from_millis(sid + 1));
        }
        w.record(&awake(0, true, false, 0), t0 + Duration::from_secs(10));
        w.record(&awake(9999, true, false, 0), t0 + Duration::from_secs(11));
        assert_eq!(w.panes.len(), PANES_MAX);
        assert!(w.latest(1).is_none(), "the one heard from longest ago went");
        assert!(w.latest(0).is_some() && w.latest(9999).is_some());
    }
}

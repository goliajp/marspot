//! What the sessions say happened in their panes (`PaneAwake`), kept
//! for the supervisor: when each pane last printed, when its terminal
//! last changed hands and to which process group.  The reader of the
//! core's frames records each report and knocks the pump, so the pass
//! after it can read it.
//!
//! Only kept so far: the pane state machine and the plugins still look
//! for themselves on the supervisor's clock.

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
}

#[derive(Default)]
pub(crate) struct Wakes {
    panes: Vec<(u64, PaneWake)>,
}

impl Wakes {
    /// Whether the report says the terminal changed hands.
    fn record(&mut self, a: &PaneAwake, now: Instant) -> bool {
        let i = match self.panes.iter().position(|(sid, _)| *sid == a.session_id) {
            Some(i) => i,
            None => {
                if self.panes.len() >= PANES_MAX
                    && let Some(oldest) = (0..self.panes.len()).min_by_key(|&i| self.panes[i].1.heard_at)
                {
                    self.panes.swap_remove(oldest);
                }
                let blank = PaneWake { output_at: None, foreground_at: None, fg_pgid: 0, shell_in_front: false, heard_at: now };
                self.panes.push((a.session_id, blank));
                self.panes.len() - 1
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
        a.foreground
    }

    fn latest(&self, sid: u64) -> Option<PaneWake> {
        self.panes.iter().find(|(s, _)| *s == sid).map(|(_, w)| *w)
    }
}

static WAKES: Mutex<Wakes> = Mutex::new(Wakes { panes: Vec::new() });
static HEARD: AtomicU64 = AtomicU64::new(0);

/// Keep `a`; from the reader thread.
pub(crate) fn record(a: &PaneAwake) {
    let handed = WAKES.lock().unwrap_or_else(|p| p.into_inner()).record(a, Instant::now());
    HEARD.fetch_add(1, Ordering::Release);
    if handed {
        marspot::lx_debug!(
            "shell.pane_awake.foreground",
            "a pane's terminal changed hands",
            sid = a.session_id,
            fg_pgid = a.fg_pgid,
            shell_in_front = a.shell_in_front as u32
        );
    }
}

/// What pane `sid` last reported.
// read by the supervisor once it waits on these reports instead of
// polling for what they say
#[allow(dead_code)]
pub(crate) fn latest(sid: u64) -> Option<PaneWake> {
    WAKES.lock().unwrap_or_else(|p| p.into_inner()).latest(sid)
}

/// How many reports have been kept: a pass compares it with the count
/// it saw last to tell whether any pane has reported since.
// as `latest`
#[allow(dead_code)]
pub(crate) fn heard() -> u64 {
    HEARD.load(Ordering::Acquire)
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
        assert!(!w.record(&awake(1, true, false, 50), t1));
        let got = w.latest(1).unwrap();
        assert_eq!((got.output_at, got.foreground_at, got.fg_pgid), (Some(t1), Some(t0), 50));
        assert!(w.latest(2).is_none());
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

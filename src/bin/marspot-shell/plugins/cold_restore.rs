//! What every agent's cold restore shares: when a pane whose session
//! process started afresh is owed its agent back, when a pane's row of
//! what it last ran goes, and the pace restores start at.
//!
//! After the machine restarts, every pane's session process comes back
//! with its screen and a new shell.  Each agent plugin keeps, per pane,
//! what it last ran there and under which shell; a pane whose shell is
//! new, idle, young and untouched gets that agent resumed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use crate::plugins::pidtree;

/// How long a pane's own shell must be without its agent before its row
/// goes.  A switch or a reclaim takes the agent down for seconds, and a
/// machine shutting down kills the agent a moment before the shell: the
/// row has to outlive both.
pub(crate) const GONE_GRACE: Duration = Duration::from_secs(30);

/// How long after a pane's new shell started its agent is put back.
/// The restores of a restarted machine are started `STAGGER` apart, at
/// most `CAP` of them, so the last starts about two minutes after the
/// panes came back; past five minutes the pane is the person's, who may
/// come back to it at any time and must not find a line typed under them.
pub(crate) const WINDOW: Duration = Duration::from_secs(5 * 60);

/// Between two restores, of any agent: each one reads a whole history.
pub(crate) const STAGGER: Duration = Duration::from_secs(4);
/// Restores one shell process starts, at most, of all agents together.
pub(crate) const CAP: u32 = 32;

/// When the process `id` started.
pub(crate) fn started(id: pidtree::ProcId) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_micros(id.start_us)
}

/// Whether a pane whose shell is `shell` may still have a line typed
/// into it at `now`: the shell is younger than `WINDOW`, and the pane
/// has had no input since it started.
pub(crate) fn restorable(shell: pidtree::ProcId, now: SystemTime, last_input: Option<SystemTime>) -> bool {
    let from = started(shell);
    now.duration_since(from).unwrap_or_default() < WINDOW && last_input.is_none_or(|t| t < from)
}

/// Whether pane `sid` is still open.  A close marks the session closed
/// before it signals it and removes its directory once it has exited; a
/// session process that ended with the machine keeps both.  Its entry
/// alone says nothing: the core removes it while it starts a dead pane's
/// session process again, which writes it back.
pub(crate) fn pane_open(sid: u64) -> bool {
    use marspot_term::session_registry::{is_closed, session_dir};
    session_dir(sid).exists() && !is_closed(sid)
}

/// When pane `sid` last had input; the control hub's record.
pub(crate) fn hub_last_input(sid: u64) -> Option<SystemTime> {
    crate::pane_control::hub()?.last_input(sid)
}

/// A pane's row: which pane, and the shell its agent last ran under.
pub(crate) trait ColdRow {
    fn sid(&self) -> u64;
    fn shell(&self) -> pidtree::ProcId;
}

/// What a scan says about panes with no agent bound now.
pub(crate) struct Unbound<'a> {
    pub(crate) bound: &'a dyn Fn(u64) -> bool,
    /// The pane is still open; see `pane_open`.
    pub(crate) listed: &'a dyn Fn(u64) -> bool,
    /// The pane's shell, when its session process runs.
    pub(crate) shell: &'a dyn Fn(u64) -> Option<pidtree::ProcId>,
    pub(crate) last_input: &'a dyn Fn(u64) -> Option<SystemTime>,
    pub(crate) now: Instant,
    pub(crate) wall: SystemTime,
}

/// Drop the rows of closed panes, of panes whose own shell has been
/// without the agent for `GONE_GRACE`, and of new shells past `WINDOW`
/// or typed into; the sids dropped.
pub(crate) fn forget<R: ColdRow>(rows: &mut Vec<R>, gone_since: &mut HashMap<u64, Instant>, u: &Unbound) -> Vec<u64> {
    let mut dropped = Vec::new();
    rows.retain(|r| {
        let sid = r.sid();
        if (u.bound)(sid) {
            gone_since.remove(&sid);
            return true;
        }
        let keep = match (u.shell)(sid) {
            _ if !(u.listed)(sid) => false,
            // the session process is not back yet
            None => true,
            Some(sh) if sh == r.shell() => {
                let since = *gone_since.entry(sid).or_insert(u.now);
                u.now.duration_since(since) < GONE_GRACE
            }
            Some(sh) => restorable(sh, u.wall, (u.last_input)(sid)),
        };
        if !keep {
            dropped.push(sid);
        }
        keep
    });
    gone_since.retain(|sid, _| rows.iter().any(|r| r.sid() == *sid));
    dropped
}

/// The pane as the kernel has it now.
pub(crate) struct PaneNow {
    pub(crate) shell: pidtree::ProcId,
    /// The shell holds its terminal and no agent runs anywhere in it.
    pub(crate) idle_shell: bool,
    pub(crate) last_input: Option<SystemTime>,
}

/// Whether `pane` may be typed into at `now`: idle, young, untouched.
pub(crate) fn owed_shell(p: &PaneNow, now: SystemTime) -> bool {
    p.idle_shell && restorable(p.shell, now, p.last_input)
}

/// Whether row `r` is owed a restore into `pane` at `now`: a new shell,
/// idle, young and untouched.
pub(crate) fn owed<R: ColdRow>(r: &R, pane: Option<&PaneNow>, now: SystemTime) -> bool {
    pane.is_some_and(|p| p.shell != r.shell() && owed_shell(p, now))
}

#[derive(Default)]
struct PaceState {
    last_start: Option<Instant>,
    started: u32,
    capped_said: bool,
}

/// The pace restores start at: one every `STAGGER`, `CAP` in all.  One
/// per shell process, shared by every agent's plugin.
#[derive(Clone)]
pub(crate) struct Pace(Arc<Mutex<PaceState>>);

/// Why a restore may not start now.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Held {
    Stagger,
    /// The cap is reached; `first` the first time this is said.
    Capped { first: bool },
}

impl Default for Pace {
    /// The shell process's own.
    fn default() -> Pace {
        static SHARED: OnceLock<Pace> = OnceLock::new();
        SHARED.get_or_init(Pace::own).clone()
    }
}

impl Pace {
    /// A pace of its own, not the shell process's.
    pub(crate) fn own() -> Pace {
        Pace(Arc::default())
    }

    pub(crate) fn may_start(&self) -> Result<(), Held> {
        let mut p = self.0.lock().unwrap();
        if p.last_start.is_some_and(|t| t.elapsed() < STAGGER) {
            return Err(Held::Stagger);
        }
        if p.started >= CAP {
            let first = !p.capped_said;
            p.capped_said = true;
            return Err(Held::Capped { first });
        }
        Ok(())
    }

    pub(crate) fn note_start(&self) {
        let mut p = self.0.lock().unwrap();
        p.started += 1;
        p.last_start = Some(Instant::now());
    }

    /// Set when the last restore started, as a test needs it.
    #[cfg(test)]
    pub(crate) fn set_last_start(&self, at: Option<Instant>) {
        self.0.lock().unwrap().last_start = at;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_are_paced_and_capped_across_agents() {
        let pace = Pace::own();
        let other = pace.clone();
        assert_eq!(pace.may_start(), Ok(()));
        pace.note_start();
        assert_eq!(other.may_start(), Err(Held::Stagger), "a second agent's plugin waits too");
        for _ in 1..CAP {
            other.set_last_start(None);
            other.note_start();
        }
        other.set_last_start(None);
        assert_eq!(pace.may_start(), Err(Held::Capped { first: true }));
        assert_eq!(other.may_start(), Err(Held::Capped { first: false }));
    }
}

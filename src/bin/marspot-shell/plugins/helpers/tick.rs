//! The plugin's tick, on the shell's main loop: it reads only what the
//! shell already sampled (per-pane state, the observer's last walk) and
//! queues work; nothing here touches a file or waits.  What comes due
//! on a clock is the worker's (`timers`).

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use marspot::pane_state::{BusyKind, PaneStatus};

use super::table::Table;
use super::turn::{self, Turn, turn_reported};
use super::work::Job;
use super::{Shared, records};
use crate::observe::Observation;
use crate::plugins::{PluginHost, TickDue};

/// A helper whose pane is back at its shell this long after its task
/// went in has exited, not yet started.
pub(super) const START_GRACE_MS: u64 = 60_000;
/// How long a parent's shell may be in front before a state that is
/// not `Empty` counts as kept: the sampled state lags the terminal by a
/// pass or two after an agent leaves (its plugin still reports it).
pub(super) const SETTLE_MS: u64 = 10_000;

/// How long after a parent was parked it is looked at once more: past
/// the half minute of silence a pane that printed (its agent's last
/// words, `kill`'s) takes to count as empty.
pub(super) const RECHECK_MS: u64 = 35_000;

/// A parent pane with its own shell in front and a state not `Empty`.
#[derive(Debug)]
struct AtShell {
    /// Since when (ms after the epoch).
    since: u64,
    /// The shell's jobs then, as [`jobs_of`].
    jobs: u64,
    parked: bool,
    /// When it is looked at again although nothing changed.
    recheck: Option<u64>,
}

/// Parents whose shell has been in front for [`SETTLE_MS`] with a state
/// that is not `Empty` (an agent stopped with ^Z, background jobs, an
/// agent parked by idle reclaim, a contradiction, nothing known):
/// reclaim acts only on `Empty`, so no tick watches them until their
/// shell's jobs change, or they are due one more look.
#[derive(Debug, Default)]
pub(super) struct Parked {
    map: HashMap<u64, AtShell>,
}

impl Parked {
    pub fn contains(&self, parent: u64) -> bool {
        self.map.get(&parent).is_some_and(|p| p.parked)
    }

    /// `parent` is at its shell with a state not `Empty`; `printing`:
    /// because it printed a moment ago, which ends with no event.
    fn seen(&mut self, parent: u64, jobs: u64, printing: bool, now_ms: u64) {
        let new = || AtShell { since: now_ms, jobs, parked: false, recheck: Some(now_ms + SETTLE_MS + RECHECK_MS) };
        let p = self.map.entry(parent).or_insert_with(new);
        if p.jobs != jobs {
            *p = new();
        }
        p.parked = now_ms.saturating_sub(p.since) >= SETTLE_MS;
        if p.recheck.is_none_or(|at| at <= now_ms) {
            p.recheck = printing.then_some(now_ms + RECHECK_MS);
        }
    }
}

/// The jobs of `parent`'s shell (its own children), as one number: a
/// parked parent's state changes with them.  Not all that runs under it:
/// a build in the background starts and ends processes all the time.
fn jobs_of(obs: &Observation, parent: u64) -> u64 {
    let Some(x) = obs.session(parent) else { return 0 };
    let Some(shell) = x.shell else { return 0 };
    let one = |pid: i32, start_us: u64| {
        let mut h = DefaultHasher::new();
        (pid, start_us).hash(&mut h);
        h.finish()
    };
    let jobs = x.descendants.iter().filter(|p| p.row.ppid == shell.pid);
    jobs.fold(0, |sum, p| sum.wrapping_add(one(p.row.pid, p.row.start_us)))
}

pub(super) fn tick(s: &Arc<Shared>, host: &dyn PluginHost, in_turn: &mut HashMap<u64, Turn>, parked: &mut Parked) {
    let t = s.table();
    let now_ms = records::now_ms();
    in_turn.retain(|sid, _| t.map.contains_key(sid));
    let obs = crate::observe::shared().latest();
    for h in t.map.values() {
        if obs.session(h.sid).is_none() && now_ms.saturating_sub(h.started_ms) > START_GRACE_MS {
            // the worker looks again from a fresh walk before closing
            s.queue_closing(h.sid, Job::Gone { sid: h.sid });
        } else if turn_reported(h) && h.delivered_ms.is_some() {
            turn::turn(s, host, &t, h, now_ms, in_turn);
        }
    }
    reclaim(s, host, &t, now_ms, &obs, parked);
    let again = s.lock().undelivered.due(&obs);
    for (sid, was) in again {
        if !s.queue(Job::Redeliver { sid }) {
            // seen as unchanged, so the next tick tries again
            s.lock().undelivered.rollback(sid, was);
            marspot::lx_warn!("helpers.queue_full", &format!("sid={sid}: redelivery not queued"));
        }
    }
}

/// When the next tick is wanted for helpers whose session is missing
/// from `obs` and which nobody closes yet: nothing else may ever bring
/// one, as a pane that went changes no more.
pub(super) fn gone_due(t: &Table, closing: &HashSet<u64>, obs: &Observation, now_ms: u64) -> TickDue {
    let missing = t.map.values().filter(|h| !closing.contains(&h.sid) && obs.session(h.sid).is_none());
    match missing.map(|h| h.started_ms + START_GRACE_MS + 1).min() {
        Some(at) if at <= now_ms => TickDue::Cadence,
        Some(at) => TickDue::At(Instant::now() + Duration::from_millis(at - now_ms)),
        None => TickDue::Event,
    }
}

/// The sooner of two.
pub(super) fn sooner(a: TickDue, b: TickDue) -> TickDue {
    match (a, b) {
        (TickDue::Cadence, _) | (_, TickDue::Cadence) => TickDue::Cadence,
        (TickDue::At(x), TickDue::At(y)) => TickDue::At(x.min(y)),
        (TickDue::Event, x) | (x, TickDue::Event) => x,
    }
}

/// The parent's own shell holds its terminal in `obs`.
fn shell_in_front(obs: &Observation, parent: u64) -> bool {
    obs.session(parent).is_some_and(|x| x.shell.is_some() && x.front_pgid == 0)
}

/// Note when a parent of helpers an agent started is first seen with
/// no agent (its pane empty: a parked agent is not gone), and forget it
/// when one is back; the worker closes them ten minutes on.  Parents at
/// their shell with a state that is not `Empty` go into `parked`.
pub(super) fn reclaim(s: &Shared, host: &dyn PluginHost, t: &Table, now_ms: u64, obs: &Observation, parked: &mut Parked) {
    let parents: HashSet<u64> = t.map.values().filter(|h| h.started.agent_caller()).map(|h| h.started.parent).collect();
    parked.map.retain(|p, _| parents.contains(p) && shell_in_front(obs, *p));
    let mut st = s.lock();
    st.no_agent.retain(|p, _| parents.contains(p));
    let mut noted = false;
    for parent in parents {
        let status = host.pane_status(parent).ok().flatten().map(|v| v.status);
        if status == Some(PaneStatus::Empty) {
            parked.map.remove(&parent);
            if let std::collections::hash_map::Entry::Vacant(e) = st.no_agent.entry(parent) {
                e.insert(now_ms);
                noted = true;
            }
        } else {
            st.no_agent.remove(&parent);
            if shell_in_front(obs, parent) {
                let printing = status == Some(PaneStatus::Busy(BusyKind::Output));
                parked.seen(parent, jobs_of(obs, parent), printing, now_ms);
            }
        }
    }
    drop(st);
    if noted {
        s.wake();
    }
}

/// When the next tick is wanted: each second while a claude or kimi
/// helper runs (its turns are in the per-pane state the shell samples);
/// while a parent's shell is in front, not yet noted and not parked
/// (its state may still be on its way to `Empty`); once when a parked
/// parent's shell is no longer in front or its jobs changed, and once
/// at its recheck; once when a notice waits on a parent whose program
/// changed (`retry`).
pub(super) fn due(t: &Table, noted: &HashSet<u64>, parked: &Parked, obs: &Observation, retry: bool, now_ms: u64) -> TickDue {
    let converging = |p: u64| !noted.contains(&p) && shell_in_front(obs, p) && !parked.contains(p);
    let mut held = parked.map.iter().filter(|(_, x)| x.parked);
    let watching = retry
        || t.map.values().any(turn_reported)
        || t.map.values().any(|h| h.started.agent_caller() && converging(h.started.parent))
        || held.any(|(p, x)| !shell_in_front(obs, *p) || jobs_of(obs, *p) != x.jobs);
    if watching {
        return TickDue::Cadence;
    }
    match parked.map.values().filter(|x| x.parked).filter_map(|x| x.recheck).min() {
        Some(at) => TickDue::At(Instant::now() + Duration::from_millis(at.saturating_sub(now_ms))),
        None => TickDue::Event,
    }
}

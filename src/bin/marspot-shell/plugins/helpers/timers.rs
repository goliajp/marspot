//! What comes due on a clock: a helper's budget, the notices the rate
//! held back, a close to try again, a notice waiting on a parent's
//! inbox, a parent pane that has had no agent for ten minutes.  The
//! worker sleeps until the soonest of these and no longer; the shell's
//! main loop may be resting then, so none of it waits for a tick.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use super::notify::Stop;
use super::table::Table;
use super::work::Job;
use super::{NO_AGENT_FOR, Shared, records};

/// When the next of them comes due (ms after the epoch); None when
/// nothing is waiting on a clock.  Helpers being closed are left out:
/// their close is under way, or comes back through `retry`.
pub(super) fn next_ms(t: &Table, closing: &HashSet<u64>, retry: &HashMap<u64, (u64, u32)>, no_agent: &HashMap<u64, u64>, now_ms: u64) -> Option<u64> {
    let helpers = t
        .map
        .values()
        .filter(|h| !closing.contains(&h.sid))
        .flat_map(|h| [h.deadline_ms(), h.window_opens_ms(now_ms).filter(|_| h.suppressed > 0)])
        .flatten();
    let parents = no_agent.values().map(|since| since + NO_AGENT_FOR.as_millis() as u64);
    helpers.chain(parents).chain(retry.values().map(|(at, _)| *at)).min()
}

/// How long the worker may sleep before something is due.
pub(super) fn sleep_for(s: &Shared) -> Option<Duration> {
    let now = records::now_ms();
    let st = s.lock();
    let next = next_ms(&s.table(), &st.closing, &st.close_retry, &st.no_agent, now);
    let next = next.into_iter().chain(st.undelivered.next_retry_ms()).min();
    drop(st);
    next.map(|ms| Duration::from_millis(ms.saturating_sub(now)))
}

/// The jobs due at `now_ms` for the worker of `epoch`, each queued
/// once; none once that start is over.  `no_agent`: whether a parent
/// pane still has no agent ([`no_agent_now`] outside tests).
pub(super) fn due_jobs(s: &Arc<Shared>, epoch: u64, now_ms: u64, no_agent: impl Fn(u64) -> bool) -> Vec<Job> {
    let mut jobs = Vec::new();
    let mut st = s.lock();
    if s.epoch() != epoch {
        return jobs;
    }
    let t = s.table();
    for h in t.map.values() {
        if h.deadline_ms().is_some_and(|d| d <= now_ms) {
            if st.closing.insert(h.sid) {
                marspot::lx_info!("helpers.budget_due", &format!("sid={}", h.sid));
                jobs.push(Job::Stopped { sid: h.sid, stop: Stop::Budget, by_screen: false });
            }
        } else if h.suppressed > 0 && h.may_notify(now_ms) && !st.closing.contains(&h.sid) && !st.undelivered.holds(h.sid) {
            // one undelivered goes first: another would start its wait over
            jobs.push(Job::Release { sid: h.sid });
        }
    }
    let again: Vec<u64> = st.close_retry.iter().filter(|(_, (at, _))| *at <= now_ms).map(|(sid, _)| *sid).collect();
    for sid in again {
        // stays in `close_retry` with its count; the close moves its time on
        if let Some(r) = st.close_retry.get_mut(&sid) {
            r.0 = u64::MAX;
        }
        jobs.push(Job::Close { sid, tell: None });
    }
    jobs.extend(st.undelivered.retries_due(now_ms).into_iter().map(|sid| Job::Redeliver { sid }));
    let expired: Vec<u64> = st.no_agent.iter().filter(|(_, since)| now_ms >= **since + NO_AGENT_FOR.as_millis() as u64).map(|(p, _)| *p).collect();
    for p in &expired {
        st.no_agent.remove(p);
    }
    drop(st);
    for parent in expired {
        if !no_agent(parent) {
            continue;
        }
        let mut st = s.lock();
        if s.epoch() != epoch {
            break;
        }
        for h in t.under(parent).filter(|h| h.started.agent_caller()) {
            if st.closing.insert(h.sid) {
                marspot::lx_event!("HELPER_RECLAIMED", "the parent pane had no agent for 10 minutes", sid = h.sid, parent = parent);
                jobs.push(Job::Close { sid: h.sid, tell: None });
            }
        }
    }
    jobs
}

/// The parent's own shell holds its terminal: no agent is in front.
/// Checked again when the ten minutes are up, from a fresh look.
pub(super) fn no_agent_now(parent: u64) -> bool {
    let obs = crate::observe::shared().fresh(crate::observe::INTERVAL);
    obs.session(parent).is_some_and(|x| x.shell.is_some() && x.front_pgid == 0)
}

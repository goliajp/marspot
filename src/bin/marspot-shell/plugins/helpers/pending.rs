//! Notices that did not reach their parent's inbox: the parent's agent
//! was gone, or between two processes, when the notice was sent.  Each
//! is tried again when the program in front of its parent pane changes
//! (an agent started there has an inbox), at most [`TRIES`] times; the
//! facts the notice stands for are written only once it is delivered,
//! so a helper's new result stays new until then.  A notice with no
//! retry left before a new program (a parent at a shell has no inbox)
//! or none left at all, or one refused by a parent at its shell prompt,
//! writes its stop at once: the helper shows as stopped, and only its
//! result waits for the delivery.
//!
//! An agent in front of the parent that has not registered its inbox
//! yet is on its way: that failure spends no try, and the notice goes
//! again every [`SETTLE_STEP_MS`] until [`SETTLE_FOR_MS`] after it was
//! first refused so.
//!
//! The program in front may stay the same while its inbox comes back,
//! so a notice that failed for any other reason, or past that wait, is
//! also tried after each of [`SLOW_MS`], at no cost either.
//!
//! Whatever waits, a notice is given up [`GIVE_UP_MS`] after it was
//! first refused, or at once when its parent pane is gone (`give_up`).

use std::collections::HashMap;

use super::table::Event;
use crate::observe::Observation;

/// Deliveries of one notice, the first included.
pub(super) const TRIES: u32 = 5;
/// Notices kept at once; a helper has at most one.
pub(super) const CAP: usize = 32;
const SETTLE_STEP_MS: u64 = 2_000;
const SETTLE_FOR_MS: u64 = 30_000;
/// The waits before the slow retries with one program in front.
pub(super) const SLOW_MS: [u64; 3] = [60_000, 300_000, 900_000];
/// How long after its first refusal a notice is tried at all: past the
/// slow retries, the wait for a new program in front ends here.
pub(super) const GIVE_UP_MS: u64 = 30 * 60_000;

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Notice {
    pub parent: u64,
    pub body: String,
    /// Written when it is delivered, while the helper is in the table.
    pub events: Vec<Event>,
    pub tries: u32,
    /// The parent's foreground process group when it last failed.
    pub front: Option<i32>,
    /// When to try again with no change in front (ms after the epoch).
    pub retry_ms: Option<u64>,
    /// The end of that wait.
    pub settle_until: Option<u64>,
    /// Slow retries set with this program in front.
    pub slow: usize,
    /// When it was first refused (ms after the epoch).
    pub first_failed_ms: Option<u64>,
}

/// Whether `err` from a delivery means the pane's agent has no inbox
/// yet (or is not in the pane table yet), rather than a refusal.
pub(super) fn not_ready(err: &str) -> bool {
    err.contains("has no inbox") || err.contains(marspot_peer::resolve::NO_PANE_NAMED)
}

impl Notice {
    /// It failed at `now_ms` with the parent's front now `front`;
    /// `registering`: an agent is in front of the parent and has no
    /// inbox yet.  Such a failure is tried again soon and costs no try,
    /// until the wait is over; from then on, and for any other failure,
    /// the slow retries follow, which cost none.
    pub fn failed(&mut self, registering: bool, front: Option<i32>, now_ms: u64) {
        self.first_failed_ms.get_or_insert(now_ms);
        if front != self.front {
            // another program since, seen by a retry rather than the tick
            self.slow = 0;
        }
        self.front = front;
        self.retry_ms = None;
        if registering && self.slow == 0 {
            let until = *self.settle_until.get_or_insert(now_ms + SETTLE_FOR_MS);
            if now_ms < until {
                self.tries = self.tries.saturating_sub(1);
                self.retry_ms = Some((now_ms + SETTLE_STEP_MS).min(until));
                return;
            }
        }
        self.settle_until = None;
        if self.slow > 0 {
            self.tries = self.tries.saturating_sub(1);
        }
        if let Some(wait) = SLOW_MS.get(self.slow) {
            self.retry_ms = Some(now_ms + wait);
            self.slow += 1;
        }
    }

    /// When it is given up; None before it was refused.
    pub fn give_up_ms(&self) -> Option<u64> {
        self.first_failed_ms.map(|t| t + GIVE_UP_MS)
    }

    /// Whether nothing will try it again with this program in front.
    pub fn out_of_retries(&self) -> bool {
        self.retry_ms.is_none() || self.tries >= TRIES
    }

    /// The facts of the stop it tells of, taken out; what is left is
    /// written when it is delivered.  They are written at the time they
    /// are taken, which counts as a notice that went (`noticed_at`): it
    /// takes a place in the rate's window, and the strip says the parent
    /// was told, though nothing reached it.
    pub fn take_stop(&mut self) -> Vec<Event> {
        let (result, stop) = std::mem::take(&mut self.events).into_iter().partition(|e| matches!(e, Event::ResultDelivered { .. }));
        self.events = result;
        stop
    }
}

/// One undelivered notice per helper; a newer one replaces it.
#[derive(Debug, Default)]
pub(super) struct Undelivered {
    map: HashMap<u64, Notice>,
}

pub(super) fn front_of(obs: &Observation, parent: u64) -> Option<i32> {
    obs.session(parent).map(|x| x.front_pgid)
}

impl Undelivered {
    /// Keep `n` for helper `sid` unless it has been tried enough; false
    /// when it is dropped.
    #[cfg(test)]
    pub fn keep(&mut self, sid: u64, n: Notice) -> bool {
        self.keep_evicting(sid, n).0
    }

    /// [`Self::keep`], with the notice put out to make room for it, of
    /// the helper it is for: past [`CAP`], the most tried one goes.
    pub fn keep_evicting(&mut self, sid: u64, n: Notice) -> (bool, Option<(u64, Notice)>) {
        if n.tries >= TRIES {
            self.map.remove(&sid);
            return (false, None);
        }
        let mut evicted = None;
        if self.map.len() >= CAP && !self.map.contains_key(&sid) {
            let most_tried = self.map.iter().max_by_key(|(_, n)| n.tries).map(|(k, _)| *k);
            evicted = most_tried.and_then(|k| Some((k, self.map.remove(&k)?)));
        }
        self.map.insert(sid, n);
        (true, evicted)
    }

    pub fn take(&mut self, sid: u64) -> Option<Notice> {
        self.map.remove(&sid)
    }

    pub fn get(&self, sid: u64) -> Option<&Notice> {
        self.map.get(&sid)
    }

    /// Whether a notice for `sid` is kept.
    pub fn holds(&self, sid: u64) -> bool {
        self.map.contains_key(&sid)
    }

    /// A worker that ended may have taken retries it never made, or had
    /// a redelivery `due` handed out in its queue: every notice is due
    /// at `now_ms`.
    pub fn rearm(&mut self, now_ms: u64) {
        for n in self.map.values_mut() {
            n.retry_ms = Some(now_ms);
        }
    }

    /// A newer notice for `sid` is being sent: the old one goes.
    pub fn superseded(&mut self, sid: u64) {
        self.map.remove(&sid);
    }

    /// Whether a parent of a kept notice has a new program in front.
    pub fn waiting_on(&self, obs: &Observation) -> bool {
        self.map.values().any(|n| front_of(obs, n.parent) != n.front)
    }

    /// The helpers whose parent changed since their notice failed, with
    /// the front each had; each is taken to be in flight until it fails
    /// again, and a new program starts a new wait for its inbox.
    pub fn due(&mut self, obs: &Observation) -> Vec<(u64, Option<i32>)> {
        let mut out = Vec::new();
        for (sid, n) in self.map.iter_mut() {
            let now = front_of(obs, n.parent);
            if now != n.front {
                out.push((*sid, std::mem::replace(&mut n.front, now)));
                n.retry_ms = None;
                n.settle_until = None;
                n.slow = 0;
            }
        }
        out
    }

    /// A redelivery `due` handed out did not go: the change is seen
    /// again.
    pub fn rollback(&mut self, sid: u64, front: Option<i32>) {
        if let Some(n) = self.map.get_mut(&sid) {
            n.front = front;
        }
    }

    /// The soonest retry of a kept notice.
    pub fn next_retry_ms(&self) -> Option<u64> {
        self.map.values().filter_map(|n| n.retry_ms).min()
    }

    /// The soonest time a kept notice is given up.
    pub fn next_give_up_ms(&self) -> Option<u64> {
        self.map.values().filter_map(Notice::give_up_ms).min()
    }

    /// The notices whose retry is due at `now_ms`, each handed out once,
    /// and those to give up, tried one last time.
    pub fn retries_due(&mut self, now_ms: u64) -> Vec<u64> {
        let mut out = Vec::new();
        for (sid, n) in self.map.iter_mut() {
            if n.retry_ms.is_some_and(|at| at <= now_ms) || n.give_up_ms().is_some_and(|at| at <= now_ms) {
                n.retry_ms = None;
                out.push(*sid);
            }
        }
        out
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

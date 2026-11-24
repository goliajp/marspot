//! The way to the worker: the queue of the latest start, and what waits
//! on it.

use std::sync::atomic::Ordering;
use std::sync::mpsc::SyncSender;

use super::work::Job;
use super::{Shared, records};

impl Shared {
    /// Queue `job` for the worker; false when it did not go (the worker
    /// is not running, or its queue is full).
    pub(super) fn queue(&self, job: Job) -> bool {
        self.jobs.lock().unwrap_or_else(|p| p.into_inner()).as_ref().is_some_and(|tx| tx.try_send(job).is_ok())
    }

    /// Queue `job` from a thread of its own, waiting while the queue is
    /// full: the thread has no next tick to queue it again on.
    pub(super) fn queue_wait(&self, job: Job) {
        let (sid, name) = (job.sid(), job.name());
        // sent outside the lock, which the main loop takes
        let tx = self.jobs.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if !tx.is_some_and(|tx| tx.send(job).is_ok()) {
            marspot::lx_warn!("helpers.not_queued", &format!("sid={sid} {name}: the worker is not running"));
        }
    }

    /// Queue to `tx` from now on, as after a fresh start: closes queued
    /// to an earlier worker are lost with its queue, so none counts as
    /// under way, and what was waiting to be tried again is due now.
    /// Parents seen with no agent before are looked at anew.  The epoch
    /// of the start, moved on with the state held: what the earlier
    /// worker took before is put back here, and it takes nothing after.
    pub(super) fn arm(&self, tx: SyncSender<Job>) -> u64 {
        let mut st = self.lock();
        let epoch = self.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        st.closing.clear();
        st.no_agent.clear();
        st.turn_told.clear();
        let now = records::now_ms();
        st.undelivered.rearm(now);
        let retrying: Vec<u64> = st.close_retry.keys().copied().collect();
        for r in st.close_retry.values_mut() {
            r.0 = r.0.min(now);
        }
        st.closing.extend(retrying);
        drop(st);
        *self.jobs.lock().unwrap_or_else(|p| p.into_inner()) = Some(tx);
        self.wake_queued.store(false, Ordering::Release);
        self.contexts_queued.store(false, Ordering::Release);
        self.parked.store(false, Ordering::Release);
        epoch
    }

    /// The epoch now.
    pub(super) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// Whether the start of `epoch` still runs: the app was neither put
    /// away nor started again since.
    pub(super) fn runs(&self, epoch: u64) -> bool {
        self.epoch() == epoch && !self.parked.load(Ordering::Acquire)
    }

    /// Have the worker work out its sleep again.  A flag, not a slot
    /// per call: one queued wake does for any number of callers.
    pub(super) fn wake(&self) {
        if !self.wake_queued.swap(true, Ordering::AcqRel) && !self.queue(Job::Wake) {
            self.wake_queued.store(false, Ordering::Release);
        }
    }

    /// Queue `job`, which closes helper `sid`, unless it is being
    /// closed; a job that did not go leaves it free to be queued again.
    pub(super) fn queue_closing(&self, sid: u64, job: Job) {
        if !self.lock().closing.insert(sid) {
            return;
        }
        let name = job.name();
        if !self.queue(job) {
            self.lock().closing.remove(&sid);
            marspot::lx_warn!("helpers.not_queued", &format!("sid={sid} {name}"));
        }
    }
}

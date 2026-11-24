//! Waiting for the home a session leaves to let go of it.
//!
//! Codex keeps no lock across homes: while the old daemon still has
//! the session loaded, the new home can load it too, and the two write
//! the same rollout.  The old daemon unloads it about a minute after
//! the last client lets go, and offers no call to make it sooner; so
//! the switch waits, and gives up rather than resume over it.  The TUI
//! is a client too, so the wait starts when the switch op has ended it,
//! not at the verdict: the op may wait in the pane's queue first.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use marspot::pidtree::ProcId;
use marspot::ui::strings::{t, Msg};
use marspot_term::{lx_info, lx_warn};

use super::worker::Worker;
use crate::plugins::pty_op::Job;

/// Longest a switch waits for the old home once the TUI is gone;
/// measured unloads take 50 to 62 seconds.
pub(crate) const RELEASE_WAIT: Duration = Duration::from_secs(90);
/// How often the old home is asked meanwhile.
const RELEASE_EVERY: Duration = Duration::from_secs(2);

/// Finished by the worker once the old home's daemon no longer has the
/// session loaded, or with an error when it still has at the deadline.
#[derive(Clone)]
pub(crate) struct Released(pub Arc<Job>);

impl PartialEq for Released {
    fn eq(&self, o: &Self) -> bool {
        Arc::ptr_eq(&self.0, &o.0)
    }
}

impl Eq for Released {}

impl std::fmt::Debug for Released {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Released")
    }
}

pub(super) struct Release {
    pub home: PathBuf,
    pub thread: String,
    pub job: Arc<Job>,
    /// The switch op is submitted; the mark lasts until it ends.
    pub submitted: bool,
    /// Set together once the TUI is gone: the deadline, and when the
    /// old home is next asked.
    pub until: Option<Instant>,
    pub next: Option<Instant>,
    /// The last time asking failed, why.
    pub failed: Option<String>,
}

#[cfg(test)]
static TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Tests that touch the moving statics, or the published threads, take
/// turns: the test harness runs them on several threads.
#[cfg(test)]
pub(crate) fn test_turn() -> std::sync::MutexGuard<'static, ()> {
    TESTS.lock().unwrap_or_else(|e| e.into_inner())
}

impl Worker {
    /// Start watching `home` for letting go of `thread`; the job is
    /// finished when it has, or failed at the deadline.  No clock runs
    /// until the switch op says the TUI is gone.
    pub(super) fn watch_release(&mut self, sid: u64, home: &Path, thread: &str) -> Arc<Job> {
        let job = Job::noted(t(Msg::BadgeCodexWaitingRelease));
        let r = Release {
            home: home.to_path_buf(),
            thread: thread.to_string(),
            job: Arc::clone(&job),
            submitted: false,
            until: None,
            next: None,
            failed: None,
        };
        if let Some(old) = self.moves.releases.insert(sid, r) {
            old.job.finish(Err("another switch began".into()));
        }
        job
    }

    /// The release of `sid`'s move by `codex`, if that is the move marked.
    fn release_of(&mut self, sid: u64, codex: ProcId) -> Option<&mut Release> {
        let ours = self.moves.marked.get(&sid).is_some_and(|m| m.codex == codex);
        self.moves.releases.get_mut(&sid).filter(|_| ours)
    }

    pub(super) fn release_submitted(&mut self, sid: u64, codex: ProcId) {
        if let Some(r) = self.release_of(sid, codex) {
            r.submitted = true;
        }
    }

    pub(super) fn release_clock(&mut self, sid: u64, codex: ProcId) {
        let now = Instant::now();
        if let Some(r) = self.release_of(sid, codex) {
            r.until = Some(now + RELEASE_WAIT);
            r.next = Some(now + RELEASE_EVERY);
            lx_info!("peerbus.move.waiting", "the TUI is gone; waiting for the old home", sid = sid);
        }
    }

    pub(super) fn releases_due(&mut self, now: Instant) {
        let due: Vec<u64> = self.moves.releases.iter().filter(|(_, r)| r.next.is_some_and(|n| n <= now)).map(|(s, _)| *s).collect();
        for sid in due {
            let (home, thread) = {
                let r = &self.moves.releases[&sid];
                (r.home.clone(), r.thread.clone())
            };
            let answer = self.loaded_ids(&home);
            let r = self.moves.releases.get_mut(&sid).expect("listed above");
            match answer {
                Ok(ids) if !ids.contains(&thread) => {
                    r.job.finish(Ok(None));
                    self.moves.releases.remove(&sid);
                    lx_info!("peerbus.move.released", "the old home let go of the session", sid = sid);
                    continue;
                }
                Ok(_) => {}
                Err(e) => r.failed = Some(e.to_string()),
            }
            if r.until.is_some_and(|u| now >= u) {
                let why = r.failed.clone().unwrap_or_else(|| "still loaded".into());
                r.job.finish(Err(format!("the old home still has the session after {RELEASE_WAIT:?}: {why}")));
                self.moves.releases.remove(&sid);
                lx_warn!("peerbus.move.not_released", &why, sid = sid, thread = thread);
            } else {
                r.next = Some(now + RELEASE_EVERY);
            }
        }
    }
}

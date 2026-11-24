//! Waiting for the home a session leaves to let go of it.
//!
//! Codex keeps no lock across homes: while the old daemon still has
//! the session loaded, the new home can load it too, and the two write
//! the same rollout.  The old daemon unloads it about a minute after
//! the last client lets go, and offers no call to make it sooner; so
//! the switch waits, and gives up rather than resume over it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use marspot::ui::strings::{t, Msg};
use marspot_term::{lx_info, lx_warn};

use super::worker::Worker;
use crate::plugins::pty_op::Job;

/// Longest a switch waits for the old home; measured unloads take 50 to
/// 62 seconds.
pub(crate) const RELEASE_WAIT: Duration = Duration::from_secs(90);
/// How often the old home is asked meanwhile.
const RELEASE_EVERY: Duration = Duration::from_secs(2);

pub(super) struct Release {
    pub home: PathBuf,
    pub thread: String,
    pub job: Arc<Job>,
    pub until: Instant,
    pub next: Instant,
    /// The last time asking failed, why.
    pub failed: Option<String>,
}

impl Worker {
    /// Start watching `home` for letting go of `thread`; the job is
    /// finished when it has, or failed at the deadline.
    pub(super) fn watch_release(&mut self, sid: u64, home: &Path, thread: &str) -> Arc<Job> {
        let job = Job::noted(t(Msg::BadgeCodexWaitingRelease));
        let now = Instant::now();
        let r = Release {
            home: home.to_path_buf(),
            thread: thread.to_string(),
            job: Arc::clone(&job),
            until: now + RELEASE_WAIT,
            next: now + RELEASE_EVERY,
            failed: None,
        };
        if let Some(old) = self.moves.releases.insert(sid, r) {
            old.job.finish(Err("another switch began".into()));
        }
        job
    }

    pub(super) fn releases_due(&mut self, now: Instant) {
        let due: Vec<u64> = self.moves.releases.iter().filter(|(_, r)| r.next <= now).map(|(s, _)| *s).collect();
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
            if now >= r.until {
                let why = r.failed.clone().unwrap_or_else(|| "still loaded".into());
                r.job.finish(Err(format!("the old home still has the session after {RELEASE_WAIT:?}: {why}")));
                self.moves.releases.remove(&sid);
                lx_warn!("peerbus.move.not_released", &why, sid = sid, thread = thread);
            } else {
                r.next = now + RELEASE_EVERY;
            }
        }
    }
}

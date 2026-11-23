//! The helpers' result files: a change is a fact, told with the next
//! stop.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::table::{Event, Helper};
use super::{Shared, notify};

impl Shared {
    /// Watch `sid`'s result file; a change is a fact, told with the next
    /// stop.  Kept only while `sid` is still a helper: a stop that came
    /// while the watcher started has already dropped what it had.
    pub(super) fn watch_result(self: &Arc<Self>, sid: u64) {
        let path = self.dir_of(sid).join("result.md");
        let (me, seen) = (Arc::clone(self), path.clone());
        let watcher = marspot_term::fs_watch::Watcher::spawn(std::slice::from_ref(&path), move || {
            if seen.exists() && me.table().map.contains_key(&sid) {
                me.record(sid, Event::ResultChanged);
            }
        });
        match watcher {
            Ok(w) => {
                let mut st = self.lock();
                if st.table.map.contains_key(&sid) && !self.parked.load(Ordering::Acquire) {
                    st.watchers.insert(sid, w);
                }
            }
            Err(e) => marspot::lx_warn!("helpers.result_watch_failed", &format!("sid={sid}: {e}")),
        }
    }

    /// `h`'s result file may have been written while nothing watched it:
    /// text no notice carried is new.  Looked at once the watch is on.
    pub(super) fn missed_result(&self, h: &Helper) {
        // delivered by a build that kept no digest: nothing to compare with
        let unknown = h.result_sha.is_empty() && h.results_delivered > 0;
        if h.result_new || unknown {
            return;
        }
        let text = notify::read_result(&self.dir_of(h.sid).join("result.md"));
        if text.is_some_and(|r| notify::sha(&r) != h.result_sha) {
            self.record(h.sid, Event::ResultChanged);
        }
    }
}

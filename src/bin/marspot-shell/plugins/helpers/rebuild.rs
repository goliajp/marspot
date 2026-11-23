//! The table at the shell's start: from the log and the sessions still
//! running.

use std::collections::HashSet;
use std::sync::Arc;

use super::Shared;
use super::table::Event;
use super::work::Job;

/// The shadow panes running now.
pub(super) fn live_shadows() -> HashSet<u64> {
    let obs = crate::observe::shared().fresh(std::time::Duration::ZERO);
    obs.live().filter(|x| x.entry.shadow_of != 0).map(|x| x.entry.id).collect()
}

/// Rebuild the table from the log and the sessions still running; close
/// helpers whose task never went in; drop directories of helpers that
/// are gone.  How many carried over.  `live`: the shadow panes running,
/// looked up once the log is read, so that a helper in it that is not
/// among them is gone.  The worker may close one before this is done:
/// it counts as carried over all the same.
pub(super) fn rebuild(s: &Arc<Shared>, live: impl FnOnce() -> HashSet<u64>) -> usize {
    let sids: Vec<u64> = {
        // a fact written between the read and the swap would be lost
        let _w = s.writing.lock().unwrap_or_else(|p| p.into_inner());
        let replayed = super::records::replay(&s.log);
        let sids = replayed.map.keys().copied().collect();
        let mut st = s.lock();
        st.table = replayed;
        *s.published.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(st.table.clone());
        sids
    };
    let live = live();
    let carried = sids.iter().filter(|sid| live.contains(sid)).count();
    for sid in sids.into_iter().filter(|sid| !live.contains(sid)) {
        s.record(sid, Event::Stopped);
    }
    let dir = s.log.parent().map_or_else(super::records::dir, std::path::PathBuf::from);
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let stale = e.file_name().to_str().and_then(|n| n.parse::<u64>().ok()).is_some_and(|sid| !live.contains(&sid));
            if stale {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    let t = s.table();
    for h in t.map.values() {
        if h.delivered_ms.is_none() && !h.preparing {
            s.queue_closing(h.sid, Job::Close { sid: h.sid, tell: Some("was started, but its task had not gone in when marspot restarted; it was closed.") });
            continue;
        }
        s.watch_result(h.sid);
        super::settle::resume(s, h.sid, h.started.agent);
    }
    carried
}

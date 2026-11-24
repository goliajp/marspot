//! The table at the shell's start: from the log and the sessions still
//! running.

use std::cell::OnceCell;
use std::collections::HashSet;
use std::sync::Arc;

use super::Shared;
use super::table::Event;
use super::work::Job;
use crate::observe::Observation;

/// The shadow panes running now.
pub(super) fn live_shadows() -> HashSet<u64> {
    let obs = crate::observe::shared().fresh(std::time::Duration::ZERO);
    obs.live().filter(|x| x.entry.shadow_of != 0).map(|x| x.entry.id).collect()
}

/// Start the worker and rebuild the table; how many helpers carried
/// over.
pub(super) fn begin(s: &Arc<Shared>, live: impl FnOnce() -> HashSet<u64>) -> std::io::Result<usize> {
    super::work::start(Arc::clone(s))?;
    let n = rebuild(s, live);
    s.lock().unnamed = super::pair::unnamed_at_start(&s.table(), super::records::now_ms());
    // the worker worked out its sleep before there was a table
    s.wake();
    Ok(n)
}

/// Rebuild the table from the log and the sessions still running; close
/// helpers whose task never went in; drop directories of helpers that
/// are gone, unless the log cannot be read.  How many carried over.  `live`: the shadow panes running,
/// looked up once the log is read, so that a helper in it that is not
/// among them is gone.  The worker may close one before this is done:
/// it counts as carried over all the same.
pub(super) fn rebuild(s: &Arc<Shared>, live: impl FnOnce() -> HashSet<u64>) -> usize {
    let known = reload(s, &|| crate::observe::shared().fresh(crate::observe::INTERVAL));
    let sids: Vec<u64> = s.table().map.keys().copied().collect();
    let live = live();
    let carried = sids.iter().filter(|sid| live.contains(sid)).count();
    for sid in sids.into_iter().filter(|sid| !live.contains(sid)) {
        // gone while no shell ran: nobody stopped it
        s.record(sid, Event::Stopped { exited: true });
    }
    if known {
        s.sweep_dirs_where(|sid| !live.contains(&sid));
    }
    let t = s.table();
    super::container::sweep(s, live);
    for h in t.map.values() {
        if h.preparing {
            s.watch_result(h.sid);
            super::container::prepare(s, h.sid, h.started.agent);
            continue;
        }
        if h.delivered_ms.is_none() {
            s.queue_closing(h.sid, Job::Close { sid: h.sid, tell: Some("was started, but its task had not gone in when marspot restarted; it was closed.") });
            continue;
        }
        s.watch_result(h.sid);
        s.missed_result(h);
        super::settle::resume(s, h.sid, h.started.agent);
    }
    carried
}

/// Take the table and who stopped from the log; false when it cannot be
/// read, with the table as it was.  `look` waits for a walk: it is asked
/// only for a helper whose top pane the log does not say.
pub(super) fn reload(s: &Shared, look: &dyn Fn() -> Arc<Observation>) -> bool {
    // a fact written between the read and the swap would be lost
    let _w = s.writing.lock().unwrap_or_else(|p| p.into_inner());
    let obs = OnceCell::new();
    let replayed = super::records::replay(&s.log, &|sid| super::strip::above(obs.get_or_init(look), sid));
    let mut st = s.lock();
    let known = match replayed {
        Ok(r) => {
            st.table = r.table;
            st.batches = r.batches;
            st.stopped.extend(r.stopped);
            true
        }
        Err(e) => {
            marspot::lx_warn!("helpers.log_unreadable", &format!("{}: {e}", s.log.display()));
            false
        }
    };
    st.began_ms = super::records::now_ms();
    st.poked = super::turn::poked_at_start(&st.table, st.began_ms);
    *s.published.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(st.table.clone());
    known
}

//! Closing a helper's pane for the worker, and trying again later when
//! the pane is still there.

use std::sync::Arc;
use std::time::{Duration, Instant};

use marspot_peer::control::ErrorCode;

use super::table::Event;
use super::{Shared, records};
use crate::pane_control::helpers_port::with_port;

/// The first wait before a failed close is tried again; it doubles up
/// to [`CLOSE_RETRY_MAX`].
const CLOSE_RETRY: Duration = Duration::from_secs(60);
const CLOSE_RETRY_MAX: Duration = Duration::from_secs(600);

/// Close helper `sid`'s pane and forget it; true when it is closed (or
/// already gone).  A core that is restarting says when to ask again; a
/// pane still there when that runs out, or when the close fails, stays
/// a helper and is tried again later.
pub(super) fn close(s: &Arc<Shared>, sid: u64) -> bool {
    let epoch = s.epoch();
    let until = Instant::now() + Duration::from_secs(60);
    let failed = loop {
        match with_port(None, |p| p.close(sid)) {
            Some(Ok(())) => break None,
            Some(Err(r)) if r.code == ErrorCode::CoreRestarting && Instant::now() < until && s.runs(epoch) => {
                std::thread::sleep(Duration::from_millis(r.retry_after_ms.unwrap_or(1_000)));
            }
            Some(Err(r)) => break Some(format!("{}: {}", r.code.as_str(), r.message)),
            None => break Some("the control socket is not running".to_string()),
        }
    };
    if let Some(why) = failed
        && s.may_be_alive(sid, epoch)
    {
        retry_close_later(s, sid, &why);
        return false;
    }
    s.record(sid, Event::Stopped { exited: false });
    s.sweep_dirs();
    true
}

fn retry_close_later(s: &Shared, sid: u64, why: &str) {
    let mut st = s.lock();
    let tries = st.close_retry.get(&sid).map_or(0, |r| r.1) + 1;
    let wait = (CLOSE_RETRY * 2u32.saturating_pow(tries.min(8) - 1)).min(CLOSE_RETRY_MAX);
    st.close_retry.insert(sid, (records::now_ms() + wait.as_millis() as u64, tries));
    st.closing.insert(sid);
    drop(st);
    // the worker of a later start may be the one to try it
    s.wake();
    marspot::lx_warn!("helpers.close_failed", &format!("sid={sid} try={tries}: {why}; again in {}s", wait.as_secs()));
}

impl Shared {
    /// Whether helper `sid`, whose close failed, may still run, as asked
    /// by work begun in `epoch`.  Looked at before the epoch is checked:
    /// the observer goes after the park, and shows no session from then
    /// on, so a look that finds none counts only while the start runs.
    pub(super) fn may_be_alive(&self, sid: u64, epoch: u64) -> bool {
        crate::observe::shared().fresh(crate::observe::INTERVAL).session(sid).is_some() || !self.runs(epoch)
    }
}

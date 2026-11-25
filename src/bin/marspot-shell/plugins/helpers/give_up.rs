//! Giving up a notice that cannot be delivered: its parent pane is gone,
//! or it has waited [`GIVE_UP_MS`] since it was first refused.  What it
//! told of shows as told; its result stays in the helper's result file.

use std::sync::Arc;

use super::Shared;
use super::pending::{GIVE_UP_MS, Notice};
use crate::observe::Observation;

/// Why `n`, refused at `now_ms` with its parent as `obs` shows it, is
/// given up; None while it may still go.
pub(super) fn reason(n: &Notice, obs: &Observation, now_ms: u64) -> Option<String> {
    // an observation with no sessions at all is the one a put-away
    // observer serves, and says nothing of the parent
    let gone = match obs.session(n.parent) {
        Some(x) => !x.live,
        None => !obs.sessions.is_empty(),
    };
    if gone {
        return Some("its parent pane is gone".into());
    }
    let waited = n.give_up_ms().filter(|at| *at <= now_ms)?;
    Some(format!("not delivered in the {} minutes since its first refusal at {}", GIVE_UP_MS / 60_000, waited - GIVE_UP_MS))
}

/// Drop `n` of helper `sid`, last refused with `e`, saying so once: it
/// is not kept, so nothing tries it again.
pub(super) fn give_up(s: &Shared, sid: u64, mut n: Notice, why: String, e: &str) {
    s.lock().undelivered.superseded(sid);
    record_stop(s, sid, &mut n);
    marspot::lx_warn!("helpers.notice_given_up", &format!("sid={sid} parent={}: {why}, after {} tries; last: {e}", n.parent, n.tries));
    s.strip_changed();
}

/// Write the stop notice `n` of helper `sid` tells of, while it is a
/// helper; what is left in `n` is its result.
pub(super) fn record_stop(s: &Shared, sid: u64, n: &mut Notice) {
    if s.table().map.contains_key(&sid) {
        for ev in n.take_stop() {
            s.record(sid, ev);
        }
    }
}

/// The panes a notice is sent by; a test on this thread may set them.
pub(super) fn look() -> Arc<Observation> {
    #[cfg(test)]
    if let Some(obs) = LOOK.with(|l| l.borrow().clone()) {
        return obs;
    }
    crate::observe::shared().fresh(crate::observe::INTERVAL)
}

#[cfg(test)]
thread_local! {
    pub(super) static LOOK: std::cell::RefCell<Option<Arc<Observation>>> = const { std::cell::RefCell::new(None) };
}

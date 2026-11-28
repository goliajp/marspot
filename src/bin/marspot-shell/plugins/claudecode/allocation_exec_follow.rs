//! Each order followed from the main thread's start to the pane coming
//! back on its target.
//!
//! The main thread reports each switch it starts (`started`) with what
//! it read of the pane at that moment: whether a turn was running and
//! whether work ran under the agent.  That state, not the one the order
//! was decided on, is what the unsafe-switch rollback reads.  An order
//! not started within `ORDER_S` was declined: a session told of it is
//! told to carry on and the plan pins the pane.

use std::collections::HashMap;
use std::sync::Mutex;

use marspot_agent::sched::level2::exec_gate::{Done, RESUME_S};

use super::super::allocation_inputs::Snapshot;
use super::super::allocation_more::Hold;
use super::{CAP, DAY_S, Exec};

/// An order the main thread has not started in this long was declined.
const ORDER_S: i64 = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Started {
    sid: u64,
    at: i64,
    turn_running: bool,
    live: bool,
}

static STARTED: Mutex<Vec<Started>> = Mutex::new(Vec::new());

/// The main thread started the switch of `sid` at `at`; `turn_running`
/// and `live` as it read the pane then.
pub(in crate::plugins::claudecode) fn started(sid: u64, at: i64, turn_running: bool, live: bool) {
    let mut v = STARTED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if v.len() < CAP {
        v.push(Started { sid, at, turn_running, live });
    }
}

fn take_started() -> Vec<Started> {
    std::mem::take(&mut *STARTED.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
}

impl Exec {
    /// Orders the main thread started, landed or declined.
    pub(super) fn follow_pending(&mut self, snap: &Snapshot, holds: &HashMap<u64, Hold>, log: &mut Vec<(&'static str, String)>) {
        let now = snap.now;
        let reports = take_started();
        let max = self.max();
        for (o, r) in std::mem::take(&mut self.landing) {
            let landed = snap.panes.iter().any(|p| p.sid == o.sid && p.profile == o.to);
            if landed || now - r.at > RESUME_S {
                self.land(o.sid, &r, landed.then_some(now), o.noticed, max);
            } else {
                self.landing.push((o, r));
            }
        }
        for x in std::mem::take(&mut self.pending) {
            let bound = snap.panes.iter().find(|p| p.sid == x.order.sid).map(|p| p.profile);
            match reports.iter().find(|r| r.sid == x.order.sid) {
                Some(r) => {
                    let landed = bound == Some(x.order.to);
                    self.answered(x.order.sid);
                    if landed || now - r.at > RESUME_S {
                        self.land(x.order.sid, r, landed.then_some(now), x.order.noticed, max);
                    } else if self.landing.len() < CAP {
                        self.landing.push((x.order, *r));
                    }
                }
                None if now - x.at > ORDER_S => {
                    if x.order.noticed {
                        self.tell_carry_on(x.order.sid, x.order.to, holds, snap, "declined", log);
                    }
                    self.cancelled.push(x.order.sid);
                    log.push(("sched.switch.declined", format!("sid={} to=P{}", x.order.sid, x.order.to)));
                }
                None => self.pending.push(x),
            }
        }
        self.done.retain(|d| now - d.at < 7 * DAY_S);
        self.notices.retain(|n| now - n.0 < DAY_S);
    }

    fn land(&mut self, sid: u64, r: &Started, resumed_at: Option<i64>, noticed: bool, max: marspot_agent::sched::level2::ladder::Step) {
        self.ladder.switched(r.at, max);
        if self.done.len() >= CAP {
            self.done.pop_front();
        }
        self.done.push_back(Done { sid, at: r.at, resumed_at, turn_running: r.turn_running, live_unnoticed: r.live && !noticed });
    }
}

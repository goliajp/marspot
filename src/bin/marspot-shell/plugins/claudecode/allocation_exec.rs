//! The planner's executor and the ladder that lets it act, on the scan
//! worker.
//!
//! Each scan it watches every committed switch whose window is open at
//! its pane's turn boundaries and asks the stopping rule
//! (`sched::level2::exec`) what to do.  Before step B it does nothing.
//! At B (the dry run) each answer is written once as a `would_switch`,
//! `would_notify` or `would_cancel` fact with what decided it, read back
//! from the event log and decided again from that record alone; the
//! ladder counts the rehearsed switches whose record replays to the
//! decision written in it.  A would-be notice is taken as sent while
//! rehearsing, so the switch after it is rehearsed too; none of these
//! is carried into a live step.  From C it acts: a switch is handed to
//! the main thread as an order (at C only for panes nobody is at, at
//! most four a day), a notice or a word to carry on goes to the
//! session's inbox, and every switch and notice is checked against the
//! five rollback conditions.  At E, under strict protection, the waits
//! the plan would impose are rehearsed, and one that would hold an
//! attended pane is recorded.
//!
//! Nothing advances, and nothing is ordered, unless switching is set to
//! live; and nothing goes above `ladder::LADDER_MAX`.
//!
//! The ladder is kept in `sched/gate.txt` and logged as `sched.gate` at
//! start and whenever it changes; each advance is `sched.gate.advance`
//! and each rollback `sched.exec.rollback`.  A file that cannot be read
//! holds the ladder as if rolled back.  A person releases a rollback by
//! writing `sched.gate_reset = <unix seconds>` in the settings file: a
//! stamp newer than the last one applied resets the ladder at its step.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use marspot_agent::sched::level2::exec::{self, Boundary, Watched};
use marspot_agent::sched::level2::exec_gate::Done;
use marspot_agent::sched::level2::ladder::{Change, LADDER_MAX, Ladder, Step};
use marspot_agent::sched::level2::{Kind, Limits};

use super::allocation_adapter::Allocation;
use super::allocation_inputs::{PaneIn, Snapshot};
use super::allocation_more::{Hold, USER_S};
use super::allocation_route;

#[path = "allocation_exec_act.rs"]
mod act;
#[path = "allocation_exec_follow.rs"]
mod follow;
#[path = "allocation_exec_say.rs"]
pub(super) mod say;

pub(in crate::plugins::claudecode) use follow::started;

/// Nobody wrote to a pane out of focus for this long: nobody is at it.
const UNATTENDED_S: i64 = 3600;
/// A placed pane bound this soon after a live placement on its account
/// is taken to be that placement.
const BIND_S: i64 = 120;
pub(super) const CAP: usize = 64;
const DAY_S: i64 = 86_400;

/// One committed switch whose window the last pass left open.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Watch {
    pub sid: u64,
    pub from: u8,
    pub to: u8,
    pub w: Watched,
}

/// A switch for the main thread to make; `noticed`: the session was
/// told of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::plugins::claudecode) struct Order {
    pub sid: u64,
    pub from: u8,
    pub to: u8,
    pub kind: Kind,
    pub noticed: bool,
}

#[derive(Clone, Copy, Debug)]
struct Noticed {
    to: u8,
    at: i64,
    /// Sent to the session, not only rehearsed.
    sent: bool,
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    order: Order,
    at: i64,
}

/// What one scan hands the executor besides the panes.
pub(super) struct Scan<'a> {
    pub watches: &'a [Watch],
    pub lim: &'a Limits,
    /// Refusals the plan did not foresee in the last day, and the panes
    /// refused since the last scan.
    pub unforeseen: u32,
    pub refused: &'a [u64],
    /// The other-model panes strict protection would make wait.
    pub waits: &'a [u64],
    /// Switching is set to live.
    pub setting_live: bool,
    /// The settings' `sched.gate_reset` stamp.
    pub reset: Option<i64>,
}

pub(super) struct Exec {
    pub ladder: Ladder,
    path: PathBuf,
    saved: String,
    /// Lines for the first scan to log.
    start: Vec<(&'static str, String)>,
    setting_live: bool,
    /// The highest step the ladder may enter: `LADDER_MAX`.
    top: Step,
    rest_from: HashMap<u64, i64>,
    noticed: HashMap<u64, Noticed>,
    /// Dry-run records written: pane, target, decision.
    written: VecDeque<(u64, u8, String)>,
    done: VecDeque<Done>,
    /// Notices sent: when, whether answered, the pane.
    notices: VecDeque<(i64, bool, u64)>,
    pending: Vec<Pending>,
    /// Orders started, until the pane comes back on its target.
    landing: Vec<(Order, follow::Started)>,
    /// Panes whose switch was called off this scan: the plan pins them.
    pub cancelled: Vec<u64>,
}

impl Exec {
    /// The ladder as kept under `state_root`, logged; a missing file
    /// starts it in shadow, an unreadable one holds it as rolled back,
    /// and one above this build's highest step is held at it.
    pub(super) fn load(state_root: &Path, now: i64) -> Exec {
        let path = state_root.join("sched").join("gate.txt");
        let text = std::fs::read_to_string(&path).ok();
        let mut start = Vec::new();
        let mut ladder = match text.as_deref().map(Ladder::decode) {
            None => Ladder::default(),
            Some(Some(l)) => l,
            Some(None) => {
                start.push(("sched.gate.unreadable", path.display().to_string()));
                Ladder::unreadable(now)
            }
        };
        if ladder.cap(LADDER_MAX) {
            start.push(("sched.gate.capped", format!("max={}", LADDER_MAX.word())));
        }
        start.insert(0, ("sched.gate", ladder.words()));
        let saved = text.unwrap_or_default();
        let (noticed, rest_from) = (HashMap::new(), HashMap::new());
        Exec { ladder, path, saved, start, setting_live: false, top: LADDER_MAX, rest_from, noticed, written: VecDeque::new(), done: VecDeque::new(), notices: VecDeque::new(), pending: Vec::new(), landing: Vec::new(), cancelled: Vec::new() }
    }

    fn changed(&mut self, c: Option<Change>, log: &mut Vec<(&'static str, String)>) {
        let Some(c) = c else { return };
        match c.why {
            None => log.push(("sched.gate.advance", format!("step={} from={} {}", c.to.word(), c.from.word(), self.ladder.words()))),
            Some(w) => log.push(("sched.exec.rollback", format!("reason={} from={} to={} {}", w.word(), c.from.word(), c.to.word(), self.ladder.words()))),
        }
    }

    /// The highest step this scan may advance to: none above the current
    /// one unless switching is live.
    fn max(&self) -> Step {
        if self.setting_live { self.top } else { self.ladder.step.min(self.top) }
    }

    /// Placements and refusals since the last scan, counted by the ladder.
    fn count_routes(&mut self, a: &Allocation, snap: &Snapshot, refused: &[u64], log: &mut Vec<(&'static str, String)>) {
        for bad in allocation_route::take_noted() {
            let c = self.ladder.routed(bad, snap.now, self.max());
            self.changed(c, log);
        }
        allocation_route::set_live(self.setting_live && self.ladder.routes_live());
        let placed = allocation_route::live_placed(snap.now);
        for sid in refused {
            let Some(p) = snap.panes.iter().find(|p| p.sid == *sid) else { continue };
            let bound = a.last.get(sid).and_then(|l| l.bound_at);
            let hit = placed.iter().find(|(at, to)| *to == p.profile && bound.is_some_and(|b| b >= *at && b - at <= BIND_S));
            if let Some((at, _)) = hit {
                let c = self.ladder.placed_refused(*at, snap.now);
                self.changed(c, log);
            }
        }
    }

    /// A person's reset stamp newer than the last one applied.
    fn take_reset(&mut self, stamp: Option<i64>, now: i64, log: &mut Vec<(&'static str, String)>) {
        let Some(stamp) = stamp.filter(|s| *s > self.ladder.reset_at) else { return };
        let was = self.ladder.words();
        self.ladder.reset(now, stamp);
        log.push(("sched.gate.reset", format!("stamp={stamp} was {was}")));
    }

    /// One scan: the orders for the main thread.
    pub(super) fn step(&mut self, a: &mut Allocation, snap: &Snapshot, holds: &HashMap<u64, Hold>, s: &Scan, log: &mut Vec<(&'static str, String)>) -> Vec<Order> {
        let now = snap.now;
        let before = self.ladder.step;
        self.setting_live = s.setting_live;
        log.append(&mut self.start);
        self.cancelled.clear();
        self.take_reset(s.reset, now, log);
        self.count_routes(a, snap, s.refused, log);
        self.follow_pending(snap, holds, log);
        let c = self.ladder.tick(s.unforeseen, now, self.max());
        self.changed(c, log);
        if self.ladder.step == Step::C && before < Step::C {
            // what was rehearsed was never said: no live switch follows it
            self.noticed.retain(|_, n| n.sent);
        }
        for p in &snap.panes {
            if p.resting {
                self.rest_from.entry(p.sid).or_insert(now);
            } else {
                self.rest_from.remove(&p.sid);
            }
        }
        self.rest_from.retain(|sid, _| snap.panes.iter().any(|p| p.sid == *sid));
        let orders = self.watch_all(a, snap, holds, s, log);
        self.rehearse_waits(a, snap, holds, s, log);
        self.answer_gone(s.watches, holds, snap, log);
        let notices: Vec<(i64, bool)> = self.notices.iter().map(|n| (n.0, n.1)).collect();
        let stay_s = (s.lim.dwell_h * 3600.0) as i64;
        let c = self.ladder.check(self.done.make_contiguous(), stay_s, &notices, s.unforeseen, now);
        if c.is_some() {
            self.answer_all(holds, snap, "rolled_back", log);
        }
        self.changed(c, log);
        self.save(log);
        orders
    }

    fn watch_all(&mut self, a: &mut Allocation, snap: &Snapshot, holds: &HashMap<u64, Hold>, s: &Scan, log: &mut Vec<(&'static str, String)>) -> Vec<Order> {
        let (dry, acting) = (self.ladder.dry_run(), self.live());
        let mut orders = Vec::new();
        if !dry && !acting {
            return orders;
        }
        for w in s.watches.iter().filter(|w| w.w.open_at <= snap.now) {
            let Some(p) = snap.panes.iter().find(|p| p.sid == w.sid && p.profile == w.from) else { continue };
            if self.pending.iter().any(|x| x.order.sid == w.sid) || self.landing.iter().any(|x| x.0.sid == w.sid) {
                continue;
            }
            let hold = holds.get(&w.sid).copied().unwrap_or_default();
            let (b, attended) = self.boundary(a, p, hold, snap.now);
            let mut watched = w.w;
            // only a notice the session received counts once live
            watched.noticed_at = self.noticed.get(&w.sid).filter(|n| n.to == w.to && (dry || n.sent)).map(|n| n.at);
            let would = exec::decide(&watched, &b, s.lim);
            if dry {
                self.rehearse(a, p, w, &watched, &b, would, s.lim, log);
            } else if let Some(o) = self.act(a, p, w, &b, attended, hold, would, log) {
                orders.push(o);
            }
        }
        orders
    }

    /// The executor acts: switching is live and the ladder at C or above.
    pub(super) fn live(&self) -> bool {
        self.setting_live && self.ladder.executes().is_some()
    }

    fn boundary(&self, a: &Allocation, p: &PaneIn, hold: Hold, now: i64) -> (Boundary, bool) {
        let child = hold.child.unwrap_or(true);
        let open = p.transcript.as_deref().map(|t| a.open_of(t, child, now)).unwrap_or_default();
        let wrote = |s: i64| open.prompt_at.is_some_and(|at| now - at < s);
        let b = Boundary {
            at: now,
            turn_running: !p.resting,
            live: open.tasks > 0 || child,
            user: hold.focus || wrote(USER_S),
            quiet_s: self.rest_from.get(&p.sid).map_or(0, |at| now - at),
        };
        (b, hold.focus || wrote(UNATTENDED_S))
    }

    fn save(&mut self, log: &mut Vec<(&'static str, String)>) {
        let text = self.ladder.encode();
        if text == self.saved {
            return;
        }
        log.push(("sched.gate", self.ladder.words()));
        if let Err(e) = say::write_atomic(&self.path, &text) {
            log.push(("sched.gate.save_failed", format!("{e}")));
        }
        self.saved = text;
    }
}

#[cfg(test)]
#[path = "allocation_exec_tests.rs"]
mod tests;

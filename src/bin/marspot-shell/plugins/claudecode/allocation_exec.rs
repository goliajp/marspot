//! The planner's executor and the ladder that lets it act, on the scan
//! worker.
//!
//! Each scan it watches every committed switch whose window is open at
//! its pane's turn boundaries and asks the stopping rule
//! (`sched::level2::exec`) what to do.  Before step B it does nothing.
//! At B (the dry run) each answer is written once as a `would_switch`,
//! `would_notify` or `would_cancel` fact with what decided it, made
//! again from the fact alone, and counted by the ladder when both agree;
//! a would-be notice is taken as sent, so the switch after it is
//! rehearsed too.  From C it acts: a switch is handed to the main thread
//! as an order (at C only for panes nobody is at, at most four a day), a
//! notice or a word to carry on goes to the session's inbox, and every
//! switch and notice is checked against the five rollback conditions.
//!
//! The ladder is kept in `sched/gate.txt` and logged as `sched.gate` at
//! start and whenever it changes; each advance is `sched.gate.advance`
//! and each rollback `sched.exec.rollback`.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use marspot_agent::sched::level2::{Input, Kind, Limits, Plan};
use marspot_agent::sched::level2::exec::{self, Boundary, Watched, Would};
use marspot_agent::sched::level2::exec_gate::{Done, RESUME_S, UNATTENDED_PER_DAY};
use marspot_agent::sched::level2::ladder::{Change, Ladder, Scope};

use super::allocation_adapter::Allocation;
use super::allocation_inputs::{PaneIn, Snapshot};
use super::allocation_more::{Hold, USER_S};
use super::allocation_route;

#[path = "allocation_exec_say.rs"]
pub(super) mod say;

/// Nobody wrote to a pane out of focus for this long: nobody is at it.
const UNATTENDED_S: i64 = 3600;
/// An order the main thread has not started in this long was declined.
const ORDER_S: i64 = 60;
/// A placed pane bound this soon after a live placement on its account
/// is taken to be that placement.
const BIND_S: i64 = 120;
const CAP: usize = 64;
const DAY_S: i64 = 86_400;

/// One committed switch whose window the last pass left open.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Watch {
    pub sid: u64,
    pub from: u8,
    pub to: u8,
    pub w: Watched,
}

/// A switch for the main thread to make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::plugins::claudecode) struct Order {
    pub sid: u64,
    pub from: u8,
    pub to: u8,
    pub kind: Kind,
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
    turn_running: bool,
    live_unnoticed: bool,
    started: Option<i64>,
}

pub(super) struct Exec {
    pub ladder: Ladder,
    path: PathBuf,
    saved: String,
    /// Lines for the first scan to log.
    start: Vec<(&'static str, String)>,
    rest_from: HashMap<u64, i64>,
    noticed: HashMap<u64, Noticed>,
    /// Dry-run records written: pane, target, decision.
    written: VecDeque<(u64, u8, String)>,
    done: VecDeque<Done>,
    /// Notices sent: when, whether answered, the pane.
    notices: VecDeque<(i64, bool, u64)>,
    pending: Vec<Pending>,
    /// Panes whose switch was called off this scan: the plan pins them.
    pub cancelled: Vec<u64>,
}

impl Exec {
    /// The ladder as kept under `state_root`, logged; a missing or
    /// unreadable file starts it in shadow, and an unreadable one says so.
    pub(super) fn load(state_root: &Path) -> Exec {
        let path = state_root.join("sched").join("gate.txt");
        let text = std::fs::read_to_string(&path).ok();
        let ladder = text.as_deref().and_then(Ladder::decode).unwrap_or_default();
        let mut start = vec![("sched.gate", ladder.words())];
        if text.is_some_and(|t| Ladder::decode(&t).is_none()) {
            start.push(("sched.gate.unreadable", path.display().to_string()));
        }
        let saved = ladder.encode();
        Exec { ladder, path, saved, start, rest_from: HashMap::new(), noticed: HashMap::new(), written: VecDeque::new(), done: VecDeque::new(), notices: VecDeque::new(), pending: Vec::new(), cancelled: Vec::new() }
    }

    fn changed(&mut self, c: Option<Change>, log: &mut Vec<(&'static str, String)>) {
        let Some(c) = c else { return };
        match c.why {
            None => log.push(("sched.gate.advance", format!("step={} from={} {}", c.to.word(), c.from.word(), self.ladder.words()))),
            Some(w) => log.push(("sched.exec.rollback", format!("reason={} from={} to={} {}", w.word(), c.from.word(), c.to.word(), self.ladder.words()))),
        }
    }

    /// Placements and refusals since the last scan, counted by the ladder.
    fn count_routes(&mut self, a: &Allocation, snap: &Snapshot, refused: &[u64], log: &mut Vec<(&'static str, String)>) {
        for bad in allocation_route::take_noted() {
            let c = self.ladder.routed(bad, snap.now);
            self.changed(c, log);
        }
        allocation_route::set_live(self.ladder.routes_live());
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

    /// One scan: the orders for the main thread.
    #[allow(clippy::too_many_arguments)] // one scan's inputs, each read once
    pub(super) fn step(&mut self, a: &mut Allocation, snap: &Snapshot, holds: &HashMap<u64, Hold>, watches: &[Watch], lim: &Limits, unforeseen: u32, refused: &[u64], log: &mut Vec<(&'static str, String)>) -> Vec<Order> {
        let now = snap.now;
        log.append(&mut self.start);
        self.cancelled.clear();
        self.count_routes(a, snap, refused, log);
        self.follow_pending(snap);
        let c = self.ladder.tick(unforeseen, now);
        self.changed(c, log);
        for p in &snap.panes {
            if p.resting {
                self.rest_from.entry(p.sid).or_insert(now);
            } else {
                self.rest_from.remove(&p.sid);
            }
        }
        self.rest_from.retain(|sid, _| snap.panes.iter().any(|p| p.sid == *sid));
        let mut orders = Vec::new();
        if self.ladder.dry_run() || self.ladder.executes().is_some() {
            for w in watches.iter().filter(|w| w.w.open_at <= now) {
                let Some(p) = snap.panes.iter().find(|p| p.sid == w.sid && p.profile == w.from) else { continue };
                if self.pending.iter().any(|x| x.order.sid == w.sid) {
                    continue;
                }
                let hold = holds.get(&w.sid).copied().unwrap_or_default();
                let (b, attended) = self.boundary(a, p, hold, now);
                let mut watched = w.w;
                watched.noticed_at = self.noticed.get(&w.sid).filter(|n| n.to == w.to).map(|n| n.at);
                let would = exec::decide(&watched, &b, lim);
                if self.ladder.dry_run() {
                    self.rehearse(a, p, w, &watched, &b, would, lim, log);
                } else if let Some(o) = self.act(a, p, w, &b, attended, hold, would, log) {
                    orders.push(o);
                }
            }
        }
        self.answer_gone(watches, holds, snap, log);
        let notices: Vec<(i64, bool)> = self.notices.iter().map(|n| (n.0, n.1)).collect();
        let stay_s = (lim.dwell_h * 3600.0) as i64;
        let c = self.ladder.check(self.done.make_contiguous(), stay_s, &notices, unforeseen, now);
        self.changed(c, log);
        self.save(log);
        orders
    }

    /// Whether today's rule should leave lapsing quota to the executor.
    pub(super) fn live(&self) -> bool {
        self.ladder.executes().is_some()
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

    #[allow(clippy::too_many_arguments)] // one decision and what it was made from
    fn rehearse(&mut self, a: &mut Allocation, p: &PaneIn, w: &Watch, watched: &Watched, b: &Boundary, would: Would, lim: &Limits, log: &mut Vec<(&'static str, String)>) {
        let (kind, notice) = match would {
            Would::Wait => return,
            Would::Switch(k) => ("would_switch", k.notice()),
            Would::Notify(_) => ("would_notify", true),
            Would::Cancel => ("would_cancel", false),
        };
        let word = would.word();
        if self.written.iter().any(|(sid, to, wd)| *sid == w.sid && *to == w.to && *wd == word) {
            return;
        }
        if self.written.len() >= CAP {
            self.written.pop_front();
        }
        self.written.push_back((w.sid, w.to, word.clone()));
        let detail = format!("to=P{} {} {}", w.to, word, exec::words(watched, b));
        let same = exec::parse(&detail).is_some_and(|(w2, b2)| exec::decide(&w2, &b2, lim) == would);
        a.event(b.at, w.sid, kind, Some(p.profile), p.class, detail, log);
        if !matches!(would, Would::Cancel) {
            self.ladder.dry_record(same, notice);
        }
        match would {
            Would::Notify(_) => {
                self.noticed.insert(w.sid, Noticed { to: w.to, at: b.at, sent: false });
            }
            Would::Switch(_) | Would::Cancel => {
                self.noticed.remove(&w.sid);
            }
            Would::Wait => {}
        }
    }

    #[allow(clippy::too_many_arguments)] // one decision and what it acts on
    fn act(&mut self, a: &mut Allocation, p: &PaneIn, w: &Watch, b: &Boundary, attended: bool, hold: Hold, would: Would, log: &mut Vec<(&'static str, String)>) -> Option<Order> {
        let now = b.at;
        if self.ladder.executes() == Some(Scope::Unattended) {
            let today = self.done.iter().filter(|d| now - d.at < DAY_S).count() + self.pending.len();
            if attended || today >= UNATTENDED_PER_DAY as usize {
                return None;
            }
        }
        match would {
            Would::Wait => None,
            Would::Switch(kind) => {
                let noticed = self.noticed.remove(&w.sid).is_some_and(|n| n.sent);
                self.answered(w.sid);
                let order = Order { sid: w.sid, from: w.from, to: w.to, kind };
                if self.pending.len() < CAP {
                    self.pending.push(Pending { order, at: now, turn_running: b.turn_running, live_unnoticed: b.live && !noticed, started: None });
                }
                a.event(now, w.sid, "switch", Some(p.profile), p.class, format!("to=P{} kind={} started", w.to, kind.word()), log);
                log.push(("sched.switch.order", format!("sid={} P{} → P{} kind={}", w.sid, w.from, w.to, kind.word())));
                Some(order)
            }
            Would::Notify(kind) => {
                say::notice(w.sid, hold.claude_pid, w.from, w.to, kind);
                self.noticed.insert(w.sid, Noticed { to: w.to, at: now, sent: true });
                if self.notices.len() >= CAP {
                    self.notices.pop_front();
                }
                self.notices.push_back((now, false, w.sid));
                a.event(now, w.sid, "told", Some(p.profile), p.class, format!("to=P{} kind={}", w.to, kind.word()), log);
                None
            }
            Would::Cancel => {
                say::carry_on(w.sid, hold.claude_pid, w.from);
                self.noticed.remove(&w.sid);
                self.answered(w.sid);
                self.cancelled.push(w.sid);
                a.event(now, w.sid, "switch", Some(p.profile), p.class, format!("to=P{} cancelled why=background_work", w.to), log);
                None
            }
        }
    }

    fn answered(&mut self, sid: u64) {
        for n in self.notices.iter_mut().filter(|n| n.2 == sid) {
            n.1 = true;
        }
    }

    /// A session told of a switch the plan no longer holds is told to
    /// carry on.
    fn answer_gone(&mut self, watches: &[Watch], holds: &HashMap<u64, Hold>, snap: &Snapshot, log: &mut Vec<(&'static str, String)>) {
        let gone: Vec<(u64, Noticed)> = self.noticed.iter().filter(|(sid, n)| !watches.iter().any(|w| w.sid == **sid && w.to == n.to)).map(|(s, n)| (*s, *n)).collect();
        for (sid, n) in gone {
            self.noticed.remove(&sid);
            if !n.sent {
                continue;
            }
            if let (Some(h), Some(p)) = (holds.get(&sid), snap.panes.iter().find(|p| p.sid == sid)) {
                say::carry_on(sid, h.claude_pid, p.profile);
                log.push(("sched.switch.cancelled", format!("sid={sid} to=P{} why=no_longer_planned", n.to)));
            }
            self.answered(sid);
        }
    }

    /// Orders the main thread started, landed or declined.
    fn follow_pending(&mut self, snap: &Snapshot) {
        let now = snap.now;
        let mut keep = Vec::new();
        for mut x in std::mem::take(&mut self.pending) {
            let bound = snap.panes.iter().find(|p| p.sid == x.order.sid).map(|p| p.profile);
            if x.started.is_none() && bound != Some(x.order.from) {
                x.started = Some(now);
            }
            let landed = bound == Some(x.order.to);
            match x.started {
                Some(at) if landed || now - at > RESUME_S => {
                    self.ladder.switched(now);
                    if self.done.len() >= CAP {
                        self.done.pop_front();
                    }
                    self.done.push_back(Done { sid: x.order.sid, at, resumed_at: landed.then_some(now), turn_running: x.turn_running, live_unnoticed: x.live_unnoticed });
                }
                Some(_) => keep.push(x),
                None if now - x.at > ORDER_S => {}
                None => keep.push(x),
            }
        }
        self.pending = keep;
        self.done.retain(|d| now - d.at < 7 * DAY_S);
        self.notices.retain(|n| now - n.0 < DAY_S);
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

/// The committed first switches of `plan` that the executor watches, in
/// absolute time, with what each pane's input says of it.
pub(super) fn watches(plan: &Plan, input: &Input, now: i64) -> Vec<Watch> {
    let at = |h: f64| now + (h * 3600.0) as i64;
    plan.switches
        .iter()
        .filter(|s| s.order == 0)
        .filter_map(|s| {
            let p = input.panes.iter().find(|p| p.sid == s.sid && p.on == s.from)?;
            let w = Watched { open_at: at(s.t_open), dead_at: at(s.t_dead), exempt: s.exempt, clean_per_h: p.clean_per_h, long_background: p.long_background, lost_work: p.lost_work, gain: s.gain, noticed_at: None };
            Some(Watch { sid: s.sid, from: s.from, to: s.to, w })
        })
        .take(CAP)
        .collect()
}

#[cfg(test)]
#[path = "allocation_exec_tests.rs"]
mod tests;

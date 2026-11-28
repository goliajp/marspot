//! What the executor does with one decision: rehearse it in the dry
//! run, or act on it once live; and the word to carry on to every
//! session told of a switch that is not coming.

use std::collections::HashMap;

use marspot_agent::sched::level2::exec::{self, Boundary, Watched, Would};
use marspot_agent::sched::level2::exec_gate::UNATTENDED_PER_DAY;
use marspot_agent::sched::level2::ladder::Scope;
use marspot_agent::sched::level2::{FableGuard, Limits};

use super::super::allocation_adapter::Allocation;
use super::super::allocation_inputs::{PaneIn, Snapshot};
use super::super::allocation_more::Hold;
use super::{CAP, DAY_S, Exec, Noticed, Order, Pending, Scan, Watch, say};

impl Exec {
    /// Writes the record once, reads it back from the event log and
    /// decides again from it alone.  Only a rehearsed switch counts
    /// toward `C`; a record of any kind that does not replay starts the
    /// count again.
    #[allow(clippy::too_many_arguments)] // one decision and what it was made from
    pub(super) fn rehearse(&mut self, a: &mut Allocation, p: &PaneIn, w: &Watch, watched: &Watched, b: &Boundary, would: Would, lim: &Limits, log: &mut Vec<(&'static str, String)>) {
        let kind = match would {
            Would::Wait => return,
            Would::Switch(_) => "would_switch",
            Would::Notify(_) => "would_notify",
            Would::Cancel => "would_cancel",
        };
        let word = would.word();
        if self.written.iter().any(|(sid, to, wd)| *sid == w.sid && *to == w.to && *wd == word) {
            return;
        }
        if self.written.len() >= CAP {
            self.written.pop_front();
        }
        self.written.push_back((w.sid, w.to, word));
        let head = format!("to=P{} ", w.to);
        a.event(b.at, w.sid, kind, Some(p.profile), p.class, format!("{head}{}", exec::record(watched, b, lim, would)), log);
        let kept = a.events.read_since(b.at, b.at).into_iter().rev().find(|e| e.sid == w.sid && e.kind == kind && e.detail.starts_with(&head));
        let same = kept.and_then(|e| exec::replay(&e.detail)) == Some(true);
        if !same {
            log.push(("sched.exec.replay_mismatch", format!("sid={} {kind}", w.sid)));
        }
        match would {
            Would::Switch(k) => self.ladder.dry_record(same, k.notice()),
            _ if !same => self.ladder.dry_record(false, false),
            _ => {}
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
    pub(super) fn act(&mut self, a: &mut Allocation, p: &PaneIn, w: &Watch, b: &Boundary, attended: bool, hold: Hold, would: Would, log: &mut Vec<(&'static str, String)>) -> Option<Order> {
        let now = b.at;
        if self.ladder.executes() == Some(Scope::Unattended) {
            let today = self.done.iter().filter(|d| now - d.at < DAY_S).count() + self.pending.len() + self.landing.len();
            if attended || today >= UNATTENDED_PER_DAY as usize {
                // a session already told is told the switch is not coming
                if self.noticed.get(&w.sid).is_some_and(|n| n.sent) {
                    let why = if attended { "attended" } else { "day_cap" };
                    self.call_off(a, p, w.to, hold.claude_pid, why, now, log);
                }
                return None;
            }
        }
        match would {
            Would::Wait => None,
            Would::Switch(kind) => {
                let noticed = self.noticed.remove(&w.sid).is_some_and(|n| n.sent);
                let order = Order { sid: w.sid, from: w.from, to: w.to, kind, noticed };
                if self.pending.len() >= CAP {
                    return None;
                }
                self.pending.push(Pending { order, at: now });
                a.event(now, w.sid, "switch", Some(p.profile), p.class, format!("to=P{} kind={} ordered", w.to, kind.word()), log);
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
                self.call_off(a, p, w.to, hold.claude_pid, "background_work", now, log);
                None
            }
        }
    }

    /// A switch the session may have been told of is called off: it is
    /// told to carry on, its notice is answered and the plan pins it.
    #[allow(clippy::too_many_arguments)] // the pane, the switch and why
    pub(super) fn call_off(&mut self, a: &mut Allocation, p: &PaneIn, to: u8, claude_pid: i32, why: &str, now: i64, log: &mut Vec<(&'static str, String)>) {
        let told = self.noticed.remove(&p.sid).is_some_and(|n| n.sent);
        if told {
            say::carry_on(p.sid, claude_pid, p.profile);
        }
        self.answered(p.sid);
        self.cancelled.push(p.sid);
        a.event(now, p.sid, "switch", Some(p.profile), p.class, format!("to=P{to} cancelled why={why} told={}", u8::from(told)), log);
    }

    pub(super) fn answered(&mut self, sid: u64) {
        for n in self.notices.iter_mut().filter(|n| n.2 == sid) {
            n.1 = true;
        }
    }

    /// A session told of a switch the plan no longer holds is told to
    /// carry on.
    pub(super) fn answer_gone(&mut self, watches: &[Watch], holds: &HashMap<u64, Hold>, snap: &Snapshot, log: &mut Vec<(&'static str, String)>) {
        let gone: Vec<(u64, Noticed)> = self.noticed.iter().filter(|(sid, n)| !watches.iter().any(|w| w.sid == **sid && w.to == n.to)).map(|(s, n)| (*s, *n)).collect();
        for (sid, n) in gone {
            self.noticed.remove(&sid);
            if n.sent {
                self.tell_carry_on(sid, n.to, holds, snap, "no_longer_planned", log);
            }
        }
    }

    /// Every session told of a switch is told to carry on: the executor
    /// stopped acting.
    pub(super) fn answer_all(&mut self, holds: &HashMap<u64, Hold>, snap: &Snapshot, why: &str, log: &mut Vec<(&'static str, String)>) {
        let told: Vec<(u64, u8)> = self.noticed.iter().filter(|(_, n)| n.sent).map(|(s, n)| (*s, n.to)).collect();
        self.noticed.clear();
        for (sid, to) in told {
            self.tell_carry_on(sid, to, holds, snap, why, log);
        }
    }

    pub(super) fn tell_carry_on(&mut self, sid: u64, to: u8, holds: &HashMap<u64, Hold>, snap: &Snapshot, why: &str, log: &mut Vec<(&'static str, String)>) {
        if let (Some(h), Some(p)) = (holds.get(&sid), snap.panes.iter().find(|p| p.sid == sid)) {
            say::carry_on(sid, h.claude_pid, p.profile);
            log.push(("sched.switch.cancelled", format!("sid={sid} to=P{to} why={why} told=1")));
        }
        self.answered(sid);
    }

    /// At E under strict protection: each wait the plan would impose,
    /// written once as a `would_wait` fact; one that would hold an
    /// attended pane is recorded by the ladder.
    pub(super) fn rehearse_waits(&mut self, a: &mut Allocation, snap: &Snapshot, holds: &HashMap<u64, Hold>, s: &Scan, log: &mut Vec<(&'static str, String)>) {
        if !self.ladder.protects() || s.lim.fable != FableGuard::Strict {
            return;
        }
        for sid in s.waits {
            let Some(p) = snap.panes.iter().find(|p| p.sid == *sid) else { continue };
            if self.written.iter().any(|(x, to, wd)| x == sid && *to == p.profile && wd == "wait") {
                continue;
            }
            if self.written.len() >= CAP {
                self.written.pop_front();
            }
            self.written.push_back((*sid, p.profile, "wait".into()));
            let (_, attended) = self.boundary(a, p, holds.get(sid).copied().unwrap_or_default(), snap.now);
            a.event(snap.now, *sid, "would_wait", Some(p.profile), p.class, format!("attended={}", u8::from(attended)), log);
            if attended {
                self.ladder.waited_attended();
                log.push(("sched.exec.wait_attended", format!("sid={sid} P{}", p.profile)));
            }
        }
    }
}

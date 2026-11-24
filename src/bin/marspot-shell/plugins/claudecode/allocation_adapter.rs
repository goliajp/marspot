//! The shared allocation, run over Claude's panes once per scan, for
//! the record only: it moves nothing.
//!
//! Each scan it brings the per-pane facts up to date, writes a line to
//! `alloc_events` for each that changed and none for the rest, and
//! works out the plan and where each pane would go; the log gets
//! `alloc.plan` when that answer changes.  When the shell starts, what
//! it last knew of each pane is read back from every kept day of
//! events, and the speeds from the last fifteen hours of them.
//!
//! A pane's use is written as the tokens each model used, never as
//! weeks: the price of a token is a derivation, worked out again on
//! every read.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use super::allocation_events::{DAY, Event, EventLog, KEEP_DAYS};
use super::allocation_inputs::{PaneIn, Snapshot};
use super::allocation_prices::{Tokens, class_of, weeks};
use super::allocation_usage::Readers;
use crate::plugins::allocation::Class;
use crate::plugins::allocation::notice::Told;
use crate::plugins::allocation::speed::{PaneUse, Slope, WINDOW_S};

/// A pane's use is written at most this often, summed per model.
pub(super) const USED_EVERY_S: i64 = 300;

/// Models remembered per pane between two writes.
const MODELS_CAP: usize = 16;

/// What was last written for one pane.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Last {
    pub profile: Option<u8>,
    pub bound_at: Option<i64>,
    /// The account it was on before, and when it left it.
    pub left: Option<(u8, i64)>,
    pub resting: Option<bool>,
    pub refused_at: Option<i64>,
    pub marked_at: Option<i64>,
    pub would: Option<u8>,
}

/// Use read but not yet written: since when, and the tokens per model.
type Pending = (i64, Vec<(String, Tokens)>);

pub(super) struct Allocation {
    pub(super) events: EventLog,
    pub(super) last: HashMap<u64, Last>,
    pub(super) used: HashMap<u64, PaneUse>,
    pending: HashMap<u64, Pending>,
    readers: Readers,
    /// Per profile: the week's and Fable's readings.
    pub(super) slopes: HashMap<u8, (Slope, Slope)>,
    pub(super) told: Told,
    /// Plan-following would-moves of the last hour.
    pub(super) band_moves: VecDeque<i64>,
    pub(super) said: HashMap<&'static str, String>,
    rebuilt: bool,
}

/// Events in `dir` under the claudecode plugin's state directory.
pub(super) fn events_dir(state_root: &Path) -> PathBuf {
    state_root.join("plugins").join("claudecode").join("alloc_events")
}

pub(super) fn class_word(c: Class) -> &'static str {
    match c {
        Class::Plain => "plain",
        Class::Restricted => "fable",
    }
}

impl Allocation {
    pub(super) fn new(dir: PathBuf) -> Self {
        Allocation {
            events: EventLog::new(dir),
            last: HashMap::new(),
            used: HashMap::new(),
            pending: HashMap::new(),
            readers: Readers::default(),
            slopes: HashMap::new(),
            told: Told::default(),
            band_moves: VecDeque::new(),
            said: HashMap::new(),
            rebuilt: false,
        }
    }

    /// One scan: facts first, then the plan.  Log lines out, `(tag, line)`.
    pub(super) fn observe(&mut self, snap: &Snapshot) -> Vec<(&'static str, String)> {
        let mut log = Vec::new();
        if !self.rebuilt {
            self.rebuild(snap.now);
        }
        self.forget_gone(snap, &mut log);
        let mut read = HashSet::new();
        for p in &snap.panes {
            self.facts(p, snap.now, &mut log);
            self.read_use(p, snap.now, &mut read, &mut log);
        }
        self.readers.retain(&read);
        for a in &snap.accounts {
            let s = self.slopes.entry(a.profile).or_default();
            s.0.note(a.measured_at, a.week_used, 90 * 60);
            if let Some(f) = a.fable_used {
                s.1.note(a.measured_at, f, 6 * 3600);
            }
        }
        let profiles: Vec<u8> = snap.accounts.iter().map(|a| a.profile).collect();
        self.slopes.retain(|p, _| profiles.contains(p));
        super::allocation_decide::decide(self, snap, &mut log);
        log
    }

    fn write(&mut self, ev: Event, log: &mut Vec<(&'static str, String)>) {
        if let Err(e) = self.events.append(&ev) {
            log.push(("alloc.events_failed", format!("{e}")));
        }
    }

    pub(super) fn event(&mut self, at: i64, sid: u64, kind: &str, profile: Option<u8>, class: Class, detail: String, log: &mut Vec<(&'static str, String)>) {
        let ev = Event { at, sid, kind: kind.to_string(), profile, class: class_word(class).to_string(), detail };
        self.write(ev, log);
    }

    fn forget_gone(&mut self, snap: &Snapshot, log: &mut Vec<(&'static str, String)>) {
        let gone: Vec<u64> = self.last.keys().copied().filter(|sid| !snap.live.contains(sid)).collect();
        for sid in gone {
            if let Some(l) = self.last.remove(&sid)
                && l.profile.is_some()
            {
                self.event(snap.now, sid, "bound", None, Class::Plain, "pane gone".into(), log);
            }
            self.used.remove(&sid);
            self.pending.remove(&sid);
        }
        self.told.retain(|sid| snap.live.contains(&sid));
    }

    fn facts(&mut self, p: &PaneIn, now: i64, log: &mut Vec<(&'static str, String)>) {
        let last = self.last.get(&p.sid).cloned().unwrap_or_default();
        let mut next = last.clone();
        if last.profile != Some(p.profile) {
            let from = last.profile.map_or_else(String::new, |f| format!("from P{f}"));
            self.event(now, p.sid, "bound", Some(p.profile), p.class, from, log);
            next.left = last.profile.map(|f| (f, now));
            next.profile = Some(p.profile);
            next.bound_at = Some(now);
            self.used.entry(p.sid).or_default().watched(now);
        }
        if last.resting != Some(p.resting) {
            let kind = if p.resting { "rest" } else { "busy" };
            self.event(now, p.sid, kind, Some(p.profile), p.class, String::new(), log);
            next.resting = Some(p.resting);
        }
        if p.refused_at.is_some_and(|at| last.refused_at.is_none_or(|was| at > was)) {
            self.event(now, p.sid, "refused", Some(p.profile), p.class, format!("at={}", p.refused_at.unwrap_or(0)), log);
            next.refused_at = p.refused_at;
        }
        if p.marked_at.is_some_and(|at| last.marked_at.is_none_or(|was| at > was)) {
            self.event(now, p.sid, "marked", Some(p.profile), p.class, format!("at={}", p.marked_at.unwrap_or(0)), log);
            next.marked_at = p.marked_at;
        }
        self.last.insert(p.sid, next);
    }

    fn read_use(&mut self, p: &PaneIn, now: i64, read: &mut HashSet<PathBuf>, log: &mut Vec<(&'static str, String)>) {
        let spent = match p.transcript.as_deref() {
            Some(t) => {
                read.insert(t.to_path_buf());
                self.readers.read(t, now)
            }
            None => Vec::new(),
        };
        let pend = self.pending.entry(p.sid).or_insert((now, Vec::new()));
        for s in spent {
            match pend.1.iter().position(|(m, _)| *m == s.model) {
                Some(k) => pend.1[k].1.add(&s.tokens),
                None if pend.1.len() < MODELS_CAP => pend.1.push((s.model, s.tokens)),
                None => {}
            }
        }
        if now - pend.0 < USED_EVERY_S {
            return;
        }
        let (_, models) = std::mem::replace(pend, (now, Vec::new()));
        for (model, t) in models {
            let class = class_of(&model);
            self.event(now, p.sid, "used", Some(p.profile), class, format!("model={model} {}", t.words()), log);
            self.used.entry(p.sid).or_default().add(now, class, weeks(&model, &t));
        }
        if let Some(u) = self.used.get_mut(&p.sid) {
            u.prune(now);
        }
    }

    /// What every kept day of events says each pane was last, and what
    /// the last fifteen hours say it used, as if the shell had been
    /// running all along.
    pub(super) fn rebuild(&mut self, now: i64) {
        self.rebuilt = true;
        for e in self.events.read_since(now - KEEP_DAYS * DAY, now) {
            if e.kind == "bound" && e.profile.is_none() {
                self.last.remove(&e.sid);
                self.used.remove(&e.sid);
                continue;
            }
            let l = self.last.entry(e.sid).or_default();
            match e.kind.as_str() {
                "bound" => {
                    if let Some(was) = l.profile {
                        l.left = Some((was, e.at));
                    }
                    l.profile = e.profile;
                    l.bound_at = Some(e.at);
                }
                "rest" | "busy" => l.resting = Some(e.kind == "rest"),
                "refused" => l.refused_at = at_of(&e.detail),
                "marked" => l.marked_at = at_of(&e.detail),
                "would_move" => l.would = e.detail.split(' ').find_map(|w| w.strip_prefix("to=P")).and_then(|p| p.parse().ok()),
                "used" if e.at > now - WINDOW_S => {
                    let model = e.detail.split(' ').find_map(|w| w.strip_prefix("model="));
                    if let (Some(m), Some(t)) = (model, Tokens::parse(&e.detail)) {
                        self.used.entry(e.sid).or_default().add(e.at, class_of(m), weeks(m, &t));
                    }
                }
                _ => {}
            }
        }
        for (sid, l) in &self.last {
            if let Some(at) = l.bound_at {
                self.used.entry(*sid).or_default().watched(at);
            }
        }
        for u in self.used.values_mut() {
            u.prune(now);
        }
    }
}

fn at_of(detail: &str) -> Option<i64> {
    detail.strip_prefix("at=")?.parse().ok()
}

#[cfg(test)]
#[path = "allocation_adapter_tests.rs"]
mod tests;

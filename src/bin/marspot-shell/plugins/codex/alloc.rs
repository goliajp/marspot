//! The shared allocation over the codex panes, for the record only.
//!
//! Each pass brings what is known of each pane up to date from its
//! rollout's tail and the peerbus binding, writes a line to the codex
//! `alloc_events` for each fact that changed (`bound`, `rest`, `busy`,
//! `refused`, `used`, `told`), works out where each pane would go and
//! writes `would_move` when that changes, with every input the decision
//! read.  It moves nothing: no pty op, no `CODEX_HOME` change, no
//! resume.  A busy pane on an account that will not last two hours is
//! handed back to be told so through its inbox.
//!
//! A refusal counts only when it happened after the pane's codex
//! started: what the rollout says of an earlier process is history.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use super::alloc_rebuild::Refusals;
use super::alloc_rollout::{CxTokens, Note, Tails};
use crate::plugins::allocation::Class;
use crate::plugins::allocation::notice::Told;
use crate::plugins::allocation::speed::PaneUse;
use crate::plugins::claudecode::allocation_events::{DAY, Event, EventLog};
use crate::plugins::peerbus::TurnEnds;

/// A pane's use is written at most this often.
pub(super) const USED_EVERY_S: i64 = 300;

/// Two reports of a refusal this close together are the same refusal
/// when they do not both carry the turn's id.
pub(super) const SAME_REFUSAL_S: i64 = 60;

pub(super) const WEEK_S: i64 = 7 * DAY;

/// One codex account as the feed reads it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct AccountIn {
    pub profile: u8,
    pub week_used: f64,
    pub week_renews: i64,
    /// Spark's own cap: used and when it renews, when the feed reads one.
    pub spark: Option<(f64, i64)>,
    /// The feed does not say it refuses.
    pub open: bool,
    pub measured_at: i64,
    /// What its stored resets bring back before they end.
    pub supply: Vec<marspot_agent::sched::reset::Supply>,
}

/// One codex pane.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PaneIn {
    pub sid: u64,
    pub profile: u8,
    /// When its codex process started, unix seconds.
    pub started_at: i64,
    pub rollout: Option<PathBuf>,
    /// The daemon says its thread is idle.
    pub idle: bool,
    pub helper: bool,
    /// It runs a model with a cap of its own (Spark).
    pub restricted: bool,
    /// How the peerbus heard its turns end.
    pub peer: TurnEnds,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Snapshot {
    pub now: i64,
    /// When the feed was written; `None` without one.
    pub feed_at: Option<i64>,
    pub accounts: Vec<AccountIn>,
    pub panes: Vec<PaneIn>,
}

/// A busy pane to tell that its account will not last.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct HeadsUp {
    pub sid: u64,
    pub profile: u8,
    /// Hours its account lasts at the speeds on it.
    pub hours: f64,
}

/// What was last written for one pane.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Last {
    pub profile: Option<u8>,
    pub bound_at: Option<i64>,
    pub left: Option<(u8, i64)>,
    pub resting: Option<bool>,
    /// The last turn that ended without a usage refusal.
    pub done_at: Option<i64>,
    pub would: Option<u8>,
    pub rest_moves: VecDeque<i64>,
}

pub(super) struct CodexAlloc {
    pub(super) events: EventLog,
    pub(super) rebuilt: bool,
    tails: Tails,
    pub(super) last: HashMap<u64, Last>,
    pub(super) used: HashMap<u64, PaneUse>,
    pending: HashMap<u64, (i64, CxTokens)>,
    /// Per account, the newest reading a rollout gave: when, week used, renewal.
    pub(super) limits: HashMap<u8, (i64, f64, i64)>,
    pub(super) band_moves: VecDeque<i64>,
    pub(super) stranded: Told,
    pub(super) said: Option<String>,
    /// Per pane and account, the week (its renewal) it was told about.
    /// Kept apart from `last` so a pane gone for a scan, or a restart,
    /// does not forget it; dropped once that week has renewed.
    pub(super) told: HashMap<(u64, u8), i64>,
    /// Per pane, kept like `told`: a pane back after a scan away reads
    /// its tail again and must know the refusals in it.
    pub(super) refusals: HashMap<u64, Refusals>,
    /// What the last decision was made from; an unchanged one is not
    /// decided again.
    pub(super) decided: Option<u64>,
    pub(super) sched: crate::agentsvc::sched::Shadow,
}

pub(super) type Log = Vec<(&'static str, String)>;

/// The codex events directory under the shell's state.
pub(super) fn events_dir(state_root: &Path) -> PathBuf {
    state_root.join("plugins").join("codex").join("alloc_events")
}

pub(super) fn class_word(c: Class) -> &'static str {
    match c {
        Class::Plain => "plain",
        Class::Restricted => "spark",
    }
}

pub(super) fn class_of_word(w: &str) -> Class {
    if w == "spark" { Class::Restricted } else { Class::Plain }
}

pub(super) fn class_of(p: &PaneIn) -> Class {
    if p.restricted { Class::Restricted } else { Class::Plain }
}

impl CodexAlloc {
    pub(super) fn new(dir: PathBuf) -> Self {
        CodexAlloc {
            events: EventLog::for_agent(dir, "codex"),
            rebuilt: false,
            tails: Tails::default(),
            last: HashMap::new(),
            used: HashMap::new(),
            pending: HashMap::new(),
            limits: HashMap::new(),
            band_moves: VecDeque::new(),
            stranded: Told::default(),
            said: None,
            told: HashMap::new(),
            refusals: HashMap::new(),
            decided: None,
            sched: Default::default(),
        }
    }

    /// One pass: facts first, then what the core would do.
    pub(super) fn observe(&mut self, snap: &Snapshot) -> (Log, Vec<HeadsUp>) {
        let mut log = Vec::new();
        if !self.rebuilt {
            self.rebuild(snap.now);
        }
        self.forget_gone(snap, &mut log);
        crate::agentsvc::reset::idle::publish("codex", snap.panes.iter().map(|p| (p.profile, p.idle)));
        let mut keep = HashSet::new();
        for p in &snap.panes {
            self.facts(p, snap.now, &mut log);
            if let Some(path) = &p.rollout {
                keep.insert(path.clone());
                self.read_rollout(p, path, snap.now, &mut log);
            }
        }
        self.tails.retain(&keep);
        self.told.retain(|_, week| *week > snap.now);
        self.refusals.retain(|_, r| r.at >= snap.now - WEEK_S);
        let tell = super::alloc_decide::decide(self, snap, &mut log);
        (log, tell)
    }

    pub(super) fn event(&mut self, at: i64, sid: u64, kind: &str, profile: Option<u8>, class: Class, detail: String, log: &mut Log) {
        let ev = Event { at, sid, kind: kind.to_string(), profile, class: class_word(class).to_string(), detail };
        if let Err(e) = self.events.append(&ev) {
            log.push(("codex.alloc.events_failed", format!("{e}")));
        }
    }

    fn forget_gone(&mut self, snap: &Snapshot, log: &mut Log) {
        let live: HashSet<u64> = snap.panes.iter().map(|p| p.sid).collect();
        let gone: Vec<u64> = self.last.keys().copied().filter(|sid| !live.contains(sid)).collect();
        for sid in gone {
            if self.last.remove(&sid).is_some_and(|l| l.profile.is_some()) {
                self.event(snap.now, sid, "bound", None, Class::Plain, "codex gone".into(), log);
            }
            self.used.remove(&sid);
            self.pending.remove(&sid);
        }
        self.stranded.retain(|sid| live.contains(&sid));
    }

    fn facts(&mut self, p: &PaneIn, now: i64, log: &mut Log) {
        let last = self.last.get(&p.sid).cloned().unwrap_or_default();
        let mut next = last.clone();
        let class = class_of(p);
        if last.profile != Some(p.profile) {
            let from = last.profile.map_or_else(String::new, |f| format!("from P{f}"));
            self.event(now, p.sid, "bound", Some(p.profile), class, from, log);
            next.left = last.profile.map(|f| (f, now));
            next.profile = Some(p.profile);
            next.bound_at = Some(now);
            next.would = None;
            self.used.entry(p.sid).or_default().watched(now);
        }
        if last.resting != Some(p.idle) {
            let kind = if p.idle { "rest" } else { "busy" };
            self.event(now, p.sid, kind, Some(p.profile), class, String::new(), log);
            next.resting = Some(p.idle);
        }
        if let Some(d) = p.peer.done_at.filter(|d| *d >= p.started_at) {
            next.done_at = Some(next.done_at.map_or(d, |was| was.max(d)));
        }
        self.last.insert(p.sid, next);
        if let Some((at, turn)) = p.peer.refused.as_ref().filter(|(at, _)| *at >= p.started_at) {
            self.refusal(p, *at, turn.as_deref(), "peerbus", now, log);
        }
    }

    /// One refusal, written once however many reports of it arrive: by
    /// its turn's id when this report and the last refusal both carry
    /// one, else by time; one older than the last is an old one.
    fn refusal(&mut self, p: &PaneIn, at: i64, turn: Option<&str>, how: &str, now: i64, log: &mut Log) {
        let seen = self.refusals.get(&p.sid).is_some_and(|r| {
            at < r.at
                || match turn {
                    Some(t) if r.by_id => r.turns.iter().any(|x| x == t),
                    _ => (at - r.at).abs() <= SAME_REFUSAL_S,
                }
        });
        if seen {
            return;
        }
        self.note_refusal(p.sid, at, turn);
        let id = turn.map_or_else(String::new, |t| format!(" turn={t}"));
        self.event(now, p.sid, "refused", Some(p.profile), class_of(p), format!("at={at} how={how}{id}"), log);
        log.push(("codex.alloc.refused", format!("sid={} P{} at={at} how={how}{id}", p.sid, p.profile)));
    }

    fn read_rollout(&mut self, p: &PaneIn, path: &Path, now: i64, log: &mut Log) {
        let (notes, first) = self.tails.read(path);
        let class = class_of(p);
        for n in notes {
            match n {
                Note::Refused { at, agent, turn } if at >= p.started_at => self.refusal(p, at, turn.as_deref(), if agent { "agent" } else { "turn" }, now, log),
                Note::Done { at } if at >= p.started_at => {
                    let l = self.last.entry(p.sid).or_default();
                    l.done_at = Some(l.done_at.map_or(at, |d| d.max(at)));
                }
                // an earlier codex's reading may be another account's
                Note::Limits { at, week: Some((used, renews)) } if at >= p.started_at => {
                    if self.limits.get(&p.profile).is_none_or(|(was, _, _)| at > *was) {
                        self.limits.insert(p.profile, (at, used, renews));
                    }
                }
                // what a first look finds was used before this pane was watched
                Note::Used { at, tokens } if !first && at >= p.started_at => {
                    self.pending.entry(p.sid).or_insert((now, CxTokens::default())).1.add(&tokens);
                }
                _ => {}
            }
        }
        let pend = self.pending.entry(p.sid).or_insert((now, CxTokens::default()));
        if now - pend.0 < USED_EVERY_S {
            return;
        }
        let (_, t) = std::mem::replace(pend, (now, CxTokens::default()));
        if t != CxTokens::default() {
            self.event(now, p.sid, "used", Some(p.profile), class, t.words(), log);
            self.used.entry(p.sid).or_default().add(now, class, t.weeks());
        }
        if let Some(u) = self.used.get_mut(&p.sid) {
            u.prune(now);
        }
    }
}

//! The shared allocation over the gemini panes: rule (a) for a pane
//! whose account refused its model.
//!
//! gemini writes no usage figures, so an account's window is a day per
//! model and the one reading of it is a refusal, with the time it ends.
//! An account no refusal is known against has room, as a kimi account
//! without a feed row does.  Each home is one account: an API key's
//! quota is its Google Cloud project's, and which project a key belongs
//! to cannot be read locally, so two homes with keys of one project
//! count twice.
//!
//! Each pass writes `refused` to the gemini `alloc_events` once per
//! refusal (per account, model and renewal), and for a pane that must
//! move and has gone quiet, where the core sends it: `would_move` when
//! the target changes, with every input, and `via=resume` when the
//! target home holds the session, `via=handoff` when it does not.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::quota::{Refusal, Scope, pacific_midnight_after};
use crate::plugins::allocation::choose::{HELPER_LAST_H, MUST_LAST_H, Mover, loads_of};
use crate::plugins::allocation::replay::{Inputs, Kind};
use crate::plugins::allocation::{Class, Quant, RHO0};
use crate::plugins::claudecode::allocation_events::{DAY, Event, EventLog};

/// Refusals remembered; far more than accounts times models.
const REFUSALS_CAP: usize = 256;

/// Two readings of one refusal give renewals this close.
const SAME_RENEWAL_S: i64 = 120;

/// One gemini pane, as the tick saw it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PaneIn {
    pub sid: u64,
    /// The home's profile number; 0 for the user's own home.
    pub profile: u8,
    /// When its gemini started, unix seconds.
    pub started_at: i64,
    /// The model its session is set to, when known.
    pub model: Option<String>,
    /// The latest refusal its session file records.
    pub refused: Option<Refusal>,
    /// The quota dialog on its screen.
    pub on_screen: Option<Refusal>,
    /// Its session file has not grown for a while: no turn is running.
    pub quiet: bool,
    /// Why it is not to be moved now though quiet: a tool running, an
    /// approval asked, text typed, or the user at a gemini still working.
    pub held: Option<&'static str>,
    pub helper: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Snapshot {
    pub now: i64,
    /// The homes a pane can go to.
    pub homes: Vec<u8>,
    pub panes: Vec<PaneIn>,
}

/// Where a pane would go, and how.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Move {
    pub sid: u64,
    pub from: u8,
    pub to: u8,
    /// The target home holds the session: resume it there.
    pub resume: bool,
    pub inputs: Inputs,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Refused {
    pub at: i64,
    pub resets_at: i64,
}

pub(super) type Log = Vec<(&'static str, String)>;

pub(super) struct GeminiAlloc {
    events: EventLog,
    dir: PathBuf,
    rebuilt: bool,
    pub(super) refused: HashMap<(u8, Scope), Refused>,
    would: HashMap<u64, u8>,
    stuck: HashSet<u64>,
}

pub(super) fn events_dir(state_root: &Path) -> PathBuf {
    state_root.join("plugins").join("gemini").join("alloc_events")
}

/// Is account `id` refused for a pane on `model` at `now`?
pub(super) fn shut(refused: &HashMap<(u8, Scope), Refused>, id: u8, model: Option<&str>, now: i64) -> Option<i64> {
    refused.iter().filter(|((a, s), r)| *a == id && r.resets_at > now && s.covers(model)).map(|(_, r)| r.resets_at).max()
}

/// The accounts as the core sees them for a pane on `model`: each
/// unrefused one full, renewing at the next Pacific midnight; a refused
/// one closed until its refusal ends.
pub(super) fn quants(refused: &HashMap<(u8, Scope), Refused>, ids: &[u8], model: Option<&str>, now: i64) -> Vec<Quant> {
    let hours = |t: i64| ((t - now) as f64 / 3600.0).max(0.5);
    let day = hours(pacific_midnight_after(now));
    ids.iter()
        .map(|&id| {
            let shut = shut(refused, id, model, now);
            let t = shut.map_or(day, hours);
            Quant { id, r: 1.0, f: 1.0, t, tf: t, open: shut.is_none(), stale: false }
        })
        .collect()
}

impl GeminiAlloc {
    pub(super) fn new(dir: PathBuf) -> Self {
        GeminiAlloc { events: EventLog::for_agent(dir.clone(), "gemini"), dir, rebuilt: false, refused: HashMap::new(), would: HashMap::new(), stuck: HashSet::new() }
    }

    /// The refusals kept in the last two days' rows, so a restart neither
    /// writes them again nor forgets them.
    fn rebuild(&mut self, now: i64) {
        self.rebuilt = true;
        for ev in EventLog::for_agent(self.dir.clone(), "gemini").read_since(now - 2 * DAY, now) {
            let (Some(profile), "refused") = (ev.profile, ev.kind.as_str()) else { continue };
            let get = |k: &str| ev.detail.split(' ').find_map(|w| w.strip_prefix(k)?.strip_prefix('='));
            let (Some(at), Some(scope), Some(resets)) = (get("at").and_then(|v| v.parse().ok()), get("model"), get("resets").and_then(|v| v.parse::<i64>().ok())) else { continue };
            if resets > now {
                self.refused.insert((profile, Scope::from_word(scope)), Refused { at, resets_at: resets });
            }
        }
    }

    fn event(&mut self, at: i64, sid: u64, kind: &str, profile: u8, detail: String, log: &mut Log) {
        let ev = Event { at, sid, kind: kind.to_string(), profile: Some(profile), class: "plain".into(), detail };
        if let Err(e) = self.events.append(&ev) {
            log.push(("gemini.alloc.events_failed", format!("{e}")));
        }
    }

    /// Take in one refusal a pane met; true when it is new.
    fn note(&mut self, p: &PaneIn, r: &Refusal, now: i64, log: &mut Log) -> bool {
        if r.at < p.started_at || r.resets_at <= now {
            return false;
        }
        let key = (p.profile, r.scope.clone());
        if self.refused.get(&key).is_some_and(|k| k.resets_at >= r.resets_at - SAME_RENEWAL_S) {
            return false;
        }
        if self.refused.len() >= REFUSALS_CAP {
            self.refused.retain(|_, k| k.resets_at > now);
            if self.refused.len() >= REFUSALS_CAP {
                self.refused.clear();
            }
        }
        self.refused.insert(key, Refused { at: r.at, resets_at: r.resets_at });
        let detail = format!("at={} model={} how={} resets={}", r.at, r.scope.word(), r.how.word(), r.resets_at);
        log.push(("gemini.alloc.refused", format!("sid={} P{} {detail}", p.sid, p.profile)));
        self.event(now, p.sid, "refused", p.profile, detail, log);
        true
    }

    /// One pass: refusals first, then where each pane that must move
    /// would go.  `holds(pane, to)`: the home `to` holds the pane's session.
    pub(super) fn observe(&mut self, snap: &Snapshot, holds: impl Fn(&PaneIn, u8) -> bool) -> (Log, Vec<Move>) {
        let mut log = Vec::new();
        let now = snap.now;
        if !self.rebuilt {
            self.rebuild(now);
        }
        for p in &snap.panes {
            for r in [&p.refused, &p.on_screen].into_iter().flatten() {
                self.note(p, r, now, &mut log);
            }
        }
        self.refused.retain(|_, k| k.resets_at > now);
        let live: HashSet<u64> = snap.panes.iter().map(|p| p.sid).collect();
        self.would.retain(|sid, _| live.contains(sid));
        self.stuck.retain(|sid| live.contains(sid));
        let mut moves = Vec::new();
        for (i, p) in snap.panes.iter().enumerate() {
            if shut(&self.refused, p.profile, p.model.as_deref(), now).is_none() {
                self.would.remove(&p.sid);
                self.stuck.remove(&p.sid);
                continue;
            }
            if !p.quiet || p.held.is_some() {
                continue;
            }
            let inputs = self.inputs(snap, i);
            match inputs.decide(&inputs.plan()) {
                Some((to, why)) => {
                    self.stuck.remove(&p.sid);
                    let resume = holds(p, to);
                    if self.would.insert(p.sid, to) != Some(to) {
                        let via = if resume { "resume" } else { "handoff" };
                        self.event(now, p.sid, "would_move", p.profile, format!("to=P{to} why={} via={via} {}", why.word(), inputs.encode()), &mut log);
                    }
                    moves.push(Move { sid: p.sid, from: p.profile, to, resume, inputs });
                }
                None if self.stuck.insert(p.sid) => {
                    let first = snap.homes.iter().filter_map(|&h| shut(&self.refused, h, p.model.as_deref(), now)).min();
                    log.push(("gemini.alloc.notice", format!("sid={}: every gemini account refuses its model; the first back at {first:?}", p.sid)));
                }
                None => {}
            }
        }
        (log, moves)
    }

    /// What the decision about pane `i` reads: every home and the pane's
    /// own, the other panes at the default speed.
    fn inputs(&self, snap: &Snapshot, i: usize) -> Inputs {
        let p = &snap.panes[i];
        let mut ids = snap.homes.clone();
        if !ids.contains(&p.profile) {
            ids.push(p.profile);
        }
        ids.sort_unstable();
        let accounts = quants(&self.refused, &ids, p.model.as_deref(), snap.now);
        let table: Vec<(Option<u8>, Class, f64)> = snap.panes.iter().map(|x| (Some(x.profile), Class::Plain, RHO0)).collect();
        let loads = loads_of(&accounts, &table, Some(i));
        let horizon_h = if p.helper { HELPER_LAST_H } else { MUST_LAST_H };
        let b_n = RHO0 * snap.panes.len() as f64;
        Inputs { accounts, b_f: 0.0, b_n, kappa: None, loads, mover: Mover { class: Class::Plain, rate: RHO0 }, here: p.profile, kind: Kind::Out { horizon_h } }
    }

    /// A move made: one line, with what chose it.
    pub(super) fn moved(&mut self, now: i64, m: &Move, log: &mut Log) {
        let via = if m.resume { "resume" } else { "handoff" };
        self.event(now, m.sid, "moved", m.from, format!("to=P{} why=out via={via} {}", m.to, m.inputs.encode()), log);
    }

    /// What the helper rule reads of the refusals.
    pub(super) fn refusals(&self) -> Vec<(u8, Scope, i64)> {
        self.refused.iter().map(|((id, s), r)| (*id, s.clone(), r.resets_at)).collect()
    }
}

#[cfg(test)]
#[path = "alloc_tests.rs"]
mod tests;

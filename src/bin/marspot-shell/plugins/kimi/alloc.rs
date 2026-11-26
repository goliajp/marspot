//! The shared allocation over the kimi panes.
//!
//! Each pass hands the core every kimi account as the feed and the
//! refusals have it (the monthly window and paid use laid over, see
//! `extra`) and every pane at the default speed: kimi's records give no
//! use per pane that has been checked against an account's readings.
//! Kimi has no model with a window of its own, so nothing is reserved.
//! What the core decides for a pane is written to `alloc_events` as
//! `would_move` when its target changes, with every input, in the format
//! Claude's and Codex's lines have; with `kimi.allocation = live` the
//! plugin acts on it and writes `moved` instead.  A move is a new
//! session with a handoff, never a resume in another home.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use crate::plugins::allocation::choose::{HELPER_LAST_H, MUST_LAST_H, Mover, loads_of};
use crate::plugins::allocation::replay::{Inputs, Kind, Why};
use crate::plugins::allocation::rest::{MOVES_PER_WEEK, is_keeper};
use crate::plugins::allocation::{Class, Quant, RHO0};
use crate::plugins::claudecode::allocation_events::{Event, EventLog};
use crate::plugins::claudecode::quota::Room;

use super::extra::Extras;

mod quants;
pub(super) use quants::quants;

const WEEK_S: i64 = 7 * 86_400;
/// A feed older than this stops plan-following moves.
const STALE_FEED_S: i64 = 15 * 60;
/// Decisions are made again at least this often with unchanged inputs.
const DECIDE_EVERY_S: i64 = 60;
/// A pane moved live is left alone this long, while its new kimi starts.
const LANDING_S: i64 = 180;
/// The most plan-following moves remembered (the hourly limit is four).
const BAND_CAP: usize = 64;

/// One kimi pane as the pass sees it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PaneIn {
    pub sid: u64,
    pub profile: u8,
    /// Its kimi waits for the user.
    pub idle: bool,
    pub helper: bool,
    /// Its account cannot answer it: refused, or spent as the feed says.
    pub out: bool,
}

pub(super) struct Snap<'a> {
    pub now: i64,
    /// When the feed was written; `None` without one.
    pub feed_at: Option<i64>,
    /// Every account, with refusals, the month and paid use laid over.
    pub rooms: &'a [(u8, Room)],
    pub extras: &'a Extras,
    pub panes: Vec<PaneIn>,
}

/// What the core decides for one pane, with what decided it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Decision {
    pub sid: u64,
    pub to: u8,
    pub why: Why,
    pub inputs: Inputs,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Last {
    pub profile: Option<u8>,
    pub bound_at: Option<i64>,
    pub left: Option<(u8, i64)>,
    pub would: Option<u8>,
    pub rest_moves: VecDeque<i64>,
    pub moved_at: Option<i64>,
}

pub(super) struct KimiAlloc {
    events: EventLog,
    /// Acts (`moved` rows count) rather than writing what it would do.
    live: bool,
    rebuilt: bool,
    pub(super) last: HashMap<u64, Last>,
    band_moves: VecDeque<i64>,
    decided: Option<(u64, bool)>,
    decisions: Vec<Decision>,
}

/// The kimi events directory under the shell's state.
pub(super) fn events_dir(state_root: &Path) -> PathBuf {
    state_root.join("plugins").join("kimi").join("alloc_events")
}

impl KimiAlloc {
    pub(super) fn new(dir: PathBuf, live: bool) -> Self {
        KimiAlloc { events: EventLog::for_agent(dir, "kimi"), live, rebuilt: false, last: HashMap::new(), band_moves: VecDeque::new(), decided: None, decisions: Vec::new() }
    }

    pub(super) fn live(&self) -> bool {
        self.live
    }

    /// One pass: the facts that changed, then what the core decides for
    /// each pane.  In a dry run a change of target is written as
    /// `would_move`; live, the caller acts and calls `moved`.
    pub(super) fn observe(&mut self, snap: &Snap) -> Vec<Decision> {
        let live = self.live;
        if !self.rebuilt {
            self.rebuild(snap.now);
        }
        self.facts(snap);
        let Some(feed_at) = snap.feed_at else { return Vec::new() };
        let key = key_of(snap, live);
        if self.decided == Some(key) {
            return self.decisions.clone();
        }
        self.decided = Some(key);
        let accounts = quants(snap.rooms, snap.extras, snap.now);
        self.band_moves.retain(|t| snap.now - t < 3600);
        let mut out = Vec::new();
        for i in 0..snap.panes.len() {
            let p = &snap.panes[i];
            if self.last.get(&p.sid).and_then(|l| l.moved_at).is_some_and(|at| snap.now - at < LANDING_S) {
                continue;
            }
            let Some(inputs) = self.inputs(snap, &accounts, i, snap.now - feed_at > STALE_FEED_S) else { continue };
            let Some((to, why)) = inputs.decide(&inputs.plan()) else { continue };
            if !live {
                self.would(snap.now, p, to, why, &inputs);
            }
            out.push(Decision { sid: p.sid, to, why, inputs });
        }
        self.decisions = out.clone();
        out
    }

    fn inputs(&self, snap: &Snap, accounts: &[Quant], i: usize, stale_feed: bool) -> Option<Inputs> {
        let p = &snap.panes[i];
        let table: Vec<(Option<u8>, Class, f64)> = snap.panes.iter().map(|x| (Some(x.profile), Class::Plain, RHO0)).collect();
        let b_n = RHO0 * snap.panes.len() as f64;
        let mover = Mover { class: Class::Plain, rate: RHO0, tokens: None, fable: None };
        let base = |loads, kind| Inputs { accounts: accounts.to_vec(), b_f: 0.0, b_n, kappa: None, loads, mover, here: p.profile, kind };
        if p.out {
            let horizon_h = if p.helper { HELPER_LAST_H } else { MUST_LAST_H };
            return Some(base(loads_of(accounts, &table, Some(i)), Kind::Out { horizon_h }));
        }
        // a helper moves only when it has to, by the rule for helpers
        if !p.idle || p.helper || !accounts.iter().any(|q| q.id == p.profile) {
            return None;
        }
        let hours = |at: i64| (snap.now - at) as f64 / 3600.0;
        let last = self.last.get(&p.sid);
        let same = snap.panes.iter().filter(|y| y.sid != p.sid && y.profile == p.profile).map(|y| (y.sid, RHO0));
        let kind = Kind::Rest {
            keeper: is_keeper((p.sid, RHO0), same),
            since_move_h: last.and_then(|l| l.bound_at).map(hours),
            left: last.and_then(|l| l.left).map(|(id, at)| (id, hours(at))).into_iter().collect(),
            band_last_hour: self.band_moves.len(),
            demand: b_n,
            stale_feed,
            speed_known: false,
            keeps_restricted: false,
            moves_week: last.map_or(0, |l| l.rest_moves.iter().filter(|at| snap.now - **at < WEEK_S).count()),
        };
        Some(base(loads_of(accounts, &table, None), kind))
    }

    /// A `would_move` when the pane's target becomes a new account,
    /// counted against the limits once.  A pass with no decision leaves
    /// the last target as it was, so a return to it writes nothing, as
    /// the Codex adapter does; a new binding forgets it.
    fn would(&mut self, now: i64, p: &PaneIn, to: u8, why: Why, inputs: &Inputs) {
        let l = self.last.entry(p.sid).or_default();
        if l.would == Some(to) {
            return;
        }
        l.would = Some(to);
        self.event(now, p.sid, "would_move", Some(p.profile), format!("to=P{to} why={} {}", why.word(), inputs.encode()));
        self.count_move(now, p.sid, why);
    }

    /// The caller moved pane `sid` live, as `d` decided.
    pub(super) fn moved(&mut self, now: i64, from: u8, d: &Decision) {
        self.event(now, d.sid, "moved", Some(from), format!("to=P{} why={} {}", d.to, d.why.word(), d.inputs.encode()));
        self.count_move(now, d.sid, d.why);
        self.last.entry(d.sid).or_default().moved_at = Some(now);
        self.decided = None;
    }

    /// A refusal the plugin has just taken in.
    pub(super) fn refused(&mut self, now: i64, sid: u64, profile: u8, at: i64) {
        self.event(now, sid, "refused", Some(profile), format!("at={at}"));
    }

    fn count_move(&mut self, at: i64, sid: u64, why: Why) {
        if matches!(why, Why::Plan | Why::Rescue | Why::UOut) {
            if self.band_moves.len() >= BAND_CAP {
                self.band_moves.pop_front();
            }
            self.band_moves.push_back(at);
        }
        if why.resting() {
            let l = self.last.entry(sid).or_default();
            while l.rest_moves.front().is_some_and(|t| at - t >= WEEK_S) || l.rest_moves.len() >= MOVES_PER_WEEK {
                l.rest_moves.pop_front();
            }
            l.rest_moves.push_back(at);
        }
    }

    fn event(&mut self, at: i64, sid: u64, kind: &str, profile: Option<u8>, detail: String) {
        let ev = Event { at, sid, kind: kind.to_string(), profile, class: "plain".into(), detail };
        if let Err(e) = self.events.append(&ev) {
            marspot::lx_warn!("kimi.alloc.events_failed", &format!("{kind} sid={sid}: {e}"));
        }
    }

    fn facts(&mut self, snap: &Snap) {
        let gone: Vec<u64> = self.last.keys().copied().filter(|sid| !snap.panes.iter().any(|p| p.sid == *sid)).collect();
        for sid in gone {
            if self.last.remove(&sid).is_some_and(|l| l.profile.is_some()) {
                self.event(snap.now, sid, "bound", None, "kimi gone".into());
            }
        }
        for p in &snap.panes {
            let l = self.last.entry(p.sid).or_default();
            if l.profile == Some(p.profile) {
                continue;
            }
            let from = l.profile.map_or_else(String::new, |f| format!("from P{f}"));
            l.left = l.profile.map(|f| (f, snap.now));
            l.profile = Some(p.profile);
            l.bound_at = Some(snap.now);
            l.would = None;
            self.event(snap.now, p.sid, "bound", Some(p.profile), from);
        }
    }

    /// What the kept week of events says each pane was bound to and
    /// how often it moved: `moved` rows count live, `would_move` rows in
    /// a dry run, as Claude's caps do.
    fn rebuild(&mut self, now: i64) {
        self.rebuilt = true;
        let counted = if self.live { "moved" } else { "would_move" };
        for e in self.events.read_since(now - WEEK_S, now) {
            let word = |k: &str| e.detail.split(' ').find_map(|w| w.strip_prefix(k)?.strip_prefix('='));
            match e.kind.as_str() {
                "bound" => {
                    let l = self.last.entry(e.sid).or_default();
                    l.left = l.profile.filter(|_| e.profile.is_some()).map(|f| (f, e.at));
                    l.profile = e.profile;
                    l.bound_at = Some(e.at);
                    l.would = None;
                }
                k if k == counted => {
                    let to = word("to").and_then(|t| t.strip_prefix('P')?.parse().ok());
                    if let Some(why) = word("why").and_then(Why::from_word) {
                        self.count_move(e.at, e.sid, why);
                    }
                    if k == "would_move" {
                        self.last.entry(e.sid).or_default().would = to;
                    }
                }
                _ => {}
            }
        }
        self.last.retain(|_, l| l.profile.is_some());
        self.band_moves.retain(|t| now - t < 3600);
    }
}

/// Everything a decision reads, hashed, with the time to the minute.
fn key_of(snap: &Snap, live: bool) -> (u64, bool) {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (snap.now / DECIDE_EVERY_S, snap.feed_at).hash(&mut h);
    format!("{:?}", snap.rooms).hash(&mut h);
    let mut extras: Vec<_> = snap.extras.iter().map(|(n, e)| format!("{n}{e:?}")).collect();
    extras.sort();
    extras.hash(&mut h);
    for p in &snap.panes {
        (p.sid, p.profile, p.idle, p.helper, p.out).hash(&mut h);
    }
    (h.finish(), live)
}

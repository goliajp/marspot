//! What a restart reads back of the codex panes, what is remembered of
//! refusals and heads-ups, and the limits the would-moves count against.

use std::collections::VecDeque;

use super::alloc::{CodexAlloc, WEEK_S, class_of_word};
use super::alloc_rollout::CxTokens;
use crate::plugins::allocation::replay::Why;
use crate::plugins::allocation::rest::MOVES_PER_WEEK;
use crate::plugins::allocation::speed::WINDOW_S;
use crate::plugins::claudecode::allocation_events::{DAY, KEEP_DAYS};

/// A pane's last refusal: when, whether it carried its turn's id, and
/// the ids of the last refused turns, newest last.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Refusals {
    pub at: i64,
    pub by_id: bool,
    pub turns: VecDeque<String>,
}

/// The refused turns remembered per pane, to know a report seen again.
const TURNS_KEPT: usize = 16;

/// Panes and accounts told about at once; far more than are open.
const TOLD_CAP: usize = 1024;

/// Panes whose refusals are remembered; likewise far more than are open.
const REFUSALS_CAP: usize = 1024;

impl CodexAlloc {
    /// Note that pane `sid` was told about account `profile`'s week.
    pub(super) fn note_told(&mut self, sid: u64, profile: u8, week: i64) {
        if self.told.len() >= TOLD_CAP
            && let Some(k) = self.told.iter().min_by_key(|(_, w)| **w).map(|(k, _)| *k)
        {
            self.told.remove(&k);
        }
        self.told.insert((sid, profile), week);
    }

    /// Note pane `sid`'s refusal at `at`, of turn `turn` when known.
    pub(super) fn note_refusal(&mut self, sid: u64, at: i64, turn: Option<&str>) {
        if !self.refusals.contains_key(&sid)
            && self.refusals.len() >= REFUSALS_CAP
            && let Some(k) = self.refusals.iter().min_by_key(|(_, r)| r.at).map(|(k, _)| *k)
        {
            self.refusals.remove(&k);
        }
        let r = self.refusals.entry(sid).or_default();
        r.at = at;
        r.by_id = turn.is_some();
        if let Some(t) = turn {
            if r.turns.len() >= TURNS_KEPT {
                r.turns.pop_front();
            }
            r.turns.push_back(t.to_string());
        }
    }

    /// One would-move counted against the limits it falls under.
    pub(super) fn count_move(&mut self, at: i64, sid: u64, why: Why) {
        if matches!(why, Why::Plan | Why::Rescue | Why::UOut) {
            self.band_moves.push_back(at);
        }
        if why.resting()
            && let Some(l) = self.last.get_mut(&sid)
        {
            while l.rest_moves.front().is_some_and(|t| at - t >= WEEK_S) || l.rest_moves.len() >= MOVES_PER_WEEK {
                l.rest_moves.pop_front();
            }
            l.rest_moves.push_back(at);
        }
    }

    /// What every kept day of events says each pane was last, and what
    /// the last fifteen hours say it used.
    pub(super) fn rebuild(&mut self, now: i64) {
        self.rebuilt = true;
        let events = self.events.read_since(now - KEEP_DAYS * DAY, now);
        for e in &events {
            if e.kind == "bound" && e.profile.is_none() {
                self.last.remove(&e.sid);
                self.used.remove(&e.sid);
                continue;
            }
            let word = |k: &str| e.detail.split(' ').find_map(|w| w.strip_prefix(k)?.strip_prefix('='));
            let l = self.last.entry(e.sid).or_default();
            match e.kind.as_str() {
                "bound" => {
                    l.left = l.profile.map(|was| (was, e.at));
                    l.profile = e.profile;
                    l.bound_at = Some(e.at);
                    l.would = None;
                }
                "rest" | "busy" => l.resting = Some(e.kind == "rest"),
                "refused" => {
                    if let Some(at) = word("at").and_then(|a| a.parse().ok()) {
                        self.note_refusal(e.sid, at, word("turn"));
                    }
                }
                "told" => {
                    if let Some((p, week)) = e.profile.zip(word("week").and_then(|w| w.parse().ok())) {
                        self.note_told(e.sid, p, week);
                    }
                }
                "would_move" => {
                    l.would = word("to").and_then(|t| t.strip_prefix('P')?.parse().ok());
                    let why = word("why").and_then(Why::from_word);
                    if let Some(why) = why.filter(|_| now - e.at < WEEK_S) {
                        self.count_move(e.at, e.sid, why);
                    }
                }
                "used" if e.at > now - WINDOW_S => {
                    if let Some(t) = CxTokens::parse(&e.detail) {
                        self.used.entry(e.sid).or_default().add(e.at, class_of_word(&e.class), t.weeks());
                    }
                }
                _ => {}
            }
        }
        self.band_moves.retain(|t| now - t < 3600);
        self.told.retain(|_, week| *week > now);
        self.refusals.retain(|_, r| r.at >= now - WEEK_S);
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

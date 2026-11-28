//! What a switch of account costs a session, measured on this machine.
//!
//! Each pane keeps its conversation's last few replies.  When its
//! account changes, the replies after the change are compared with the
//! ones before (`sched::switch_cost::sample`) and the result is written
//! as a `switch_cost` fact; the fit over every kept day of those facts
//! prices the next switch for the context the pane has
//! (`sched::switch_cost::weeks`).

use std::collections::{HashMap, VecDeque};

use marspot_agent::sched::switch_cost::{self, AFTER, BASE, Fit, Sample, Turn, sample};

use super::super::allocation_prices::Tokens;
use super::super::allocation_usage::Spent;
use crate::plugins::allocation::Class;
use crate::plugins::allocation::waste::SWITCH_TOKENS;

/// A cold reply this soon before the change was noticed came after it.
const LATE_S: i64 = 120;
/// Panes watched at most.
const PANES_CAP: usize = 256;

#[derive(Debug, Default)]
struct Watch {
    turns: VecDeque<Turn>,
    /// The account changed then, and the replies after it are awaited.
    switched: Option<i64>,
}

#[derive(Debug, Default)]
pub(in crate::plugins::claudecode) struct SwitchCosts {
    panes: HashMap<u64, Watch>,
    pub(in crate::plugins::claudecode) fit: Fit,
}

fn turn_of(at: i64, t: &Tokens) -> Turn {
    Turn { at, input: t.input, write_5m: t.cache_5m, write_1h: t.cache_1h, read: t.cache_read, output: t.output }
}

impl SwitchCosts {
    /// Pane `sid` was put on another account, noticed at `now`.
    pub(in crate::plugins::claudecode) fn bound(&mut self, sid: u64, now: i64) {
        let Some(w) = self.panes.get_mut(&sid) else { return };
        let at = match w.turns.back() {
            Some(t) if t.cold() && now - t.at <= LATE_S => t.at - 1,
            _ => now,
        };
        w.switched = Some(at);
    }

    /// One reply read of pane `sid`; the sample it completes, if any.
    pub(in crate::plugins::claudecode) fn spent(&mut self, sid: u64, s: &Spent) -> Option<Sample> {
        if !s.main {
            return None;
        }
        if !self.panes.contains_key(&sid) && self.panes.len() >= PANES_CAP {
            return None;
        }
        let w = self.panes.entry(sid).or_default();
        let t = turn_of(s.at, &s.whole);
        match w.turns.back_mut() {
            Some(last) if s.repeat => *last = Turn { at: last.at, ..t },
            _ => {
                if w.turns.len() == BASE + AFTER {
                    w.turns.pop_front();
                }
                w.turns.push_back(t);
            }
        }
        let at = w.switched?;
        let after = w.turns.iter().filter(|t| t.at > at).count();
        if after < AFTER {
            return None;
        }
        w.switched = None;
        let turns: Vec<Turn> = w.turns.iter().copied().collect();
        let got = sample(&turns, at, None)?;
        self.fit.add(&got);
        Some(got)
    }

    pub(in crate::plugins::claudecode) fn retain(&mut self, live: impl Fn(u64) -> bool) {
        self.panes.retain(|sid, _| live(*sid));
    }
}

/// What switching a pane of `class` with `tokens` of context costs,
/// weeks; the typical context where it is not known.
pub(in crate::plugins::claudecode) fn switch_weeks(fit: &Fit, class: Class, tokens: Option<u64>) -> f64 {
    switch_cost::weeks(fit.measured(), class, tokens.unwrap_or(SWITCH_TOKENS as u64))
}

#[cfg(test)]
#[path = "allocation_switch_cost_tests.rs"]
mod tests;

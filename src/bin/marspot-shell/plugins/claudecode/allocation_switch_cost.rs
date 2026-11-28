//! What a switch of account costs a session, measured on this machine.
//!
//! Each pane keeps its conversation's last few replies.  When its
//! account changes, the replies after the change are compared with the
//! ones before (`sched::switch_cost::sample`) and the result is written
//! as a `switch_cost` fact; the fit over every kept day of those facts
//! prices the next switch.  A switch is priced for the context the pane
//! has: its rewrite share of that context at the cache-write price
//! instead of the cache-read price a normal reply pays, and the
//! thinking said again at the output price.

use std::collections::{HashMap, VecDeque};

use marspot_agent::sched::switch_cost::{AFTER, BASE, Fit, Sample, Shares, Turn, sample};

use super::super::allocation_prices::{Tokens, weeks};
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
pub(in crate::plugins::claudecode) fn switch_weeks(sh: Shares, class: Class, tokens: Option<u64>) -> f64 {
    let ctx = tokens.map_or(SWITCH_TOKENS, |t| t as f64);
    let model = if class == Class::Restricted { "fable" } else { "opus" };
    let rewrite = sh.rewrite * ctx;
    let one_hour = (rewrite * sh.one_hour) as u64;
    let after = Tokens { cache_1h: one_hour, cache_5m: rewrite as u64 - one_hour, output: (sh.regen * ctx) as u64, ..Tokens::default() };
    let normal = Tokens { cache_read: rewrite as u64, ..Tokens::default() };
    (weeks(model, &after) - weeks(model, &normal)).max(0.0)
}

#[cfg(test)]
#[path = "allocation_switch_cost_tests.rs"]
mod tests;

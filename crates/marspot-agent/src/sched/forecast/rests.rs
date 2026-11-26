//! Clean rests, background-work lengths and helper lifetimes, from
//! the `boundary` facts and the helper log.

use super::band::quantile;
use super::facts::{Facts, PaneFacts};
use super::{DAY_H, HELPER_FLOOR_S, HOUR};

/// Where a pane's rate or length came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The pane's own facts: it has a day of `boundary` rows.
    Pane,
    /// The fleet's, before the pane has a day of them.
    Fleet,
    /// No `boundary` fact at all: the pane is taken never to rest
    /// cleanly, and no background work is known.
    Nothing,
}

/// The span a pane's `boundary` rows cover, seconds: from its first
/// row to its last fact.
fn span_s(p: &PaneFacts) -> i64 {
    p.boundaries.front().map_or(0, |b| p.last_at.max(b.at) - b.at)
}

fn own(p: &PaneFacts) -> bool {
    span_s(p) >= DAY_H * HOUR
}

/// Clean rests per hour over the fleet's `boundary` spans; `None` without
/// a boundary fact or with no time covered.
pub fn fleet_clean_rate(f: &Facts) -> Option<f64> {
    let (clean, span) = f.panes.values().fold((0usize, 0i64), |(c, s), p| (c + clean_count(p), s + span_s(p)));
    (span > 0).then(|| clean as f64 * HOUR as f64 / span as f64)
}

fn clean_count(p: &PaneFacts) -> usize {
    p.boundaries.iter().filter(|b| b.clean).count()
}

/// Pane `p`'s clean rests an hour, and where it came from.
pub fn clean_rate(p: &PaneFacts, fleet: Option<f64>) -> (f64, Source) {
    if own(p) {
        return (clean_count(p) as f64 * HOUR as f64 / span_s(p) as f64, Source::Pane);
    }
    fleet.map_or((0.0, Source::Nothing), |r| (r, Source::Fleet))
}

fn ages(p: &PaneFacts) -> impl Iterator<Item = f64> + '_ {
    p.boundaries.iter().filter(|b| b.tasks > 0).filter_map(|b| b.oldest_s).map(f64::from)
}

fn p80(mut v: Vec<f64>) -> Option<u32> {
    v.sort_by(f64::total_cmp);
    (!v.is_empty()).then(|| quantile(&v, 0.8).round() as u32)
}

/// The p80 age of the oldest live background task at the fleet's
/// boundaries where one was live, seconds.
pub fn fleet_background(f: &Facts) -> Option<u32> {
    p80(f.panes.values().flat_map(ages).collect())
}

/// Pane `p`'s background-work length: its own p80 once it has a day of
/// `boundary` rows, the fleet's before.
pub fn background(p: &PaneFacts, fleet: Option<u32>) -> (Option<u32>, Source) {
    if own(p) {
        return (p80(ages(p).collect()), Source::Pane);
    }
    (fleet, if fleet.is_some() { Source::Fleet } else { Source::Nothing })
}

/// A helper's expected remaining life by agent: the p75 of its finished
/// helpers, at least ten minutes.
#[derive(Clone, Debug, PartialEq)]
pub struct HelperLife {
    pub agent: String,
    pub finished: usize,
    pub p75_s: u32,
}

pub fn helper_lives(f: &Facts) -> Vec<HelperLife> {
    f.helpers
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(agent, v)| {
            let mut s: Vec<f64> = v.iter().copied().map(f64::from).collect();
            s.sort_by(f64::total_cmp);
            let p75 = (quantile(&s, 0.75).round() as u32).max(HELPER_FLOOR_S);
            HelperLife { agent: agent.clone(), finished: v.len(), p75_s: p75 }
        })
        .collect()
}

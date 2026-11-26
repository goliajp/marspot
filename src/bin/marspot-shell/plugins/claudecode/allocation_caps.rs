//! The limits on moves nothing forced -- the hourly band and each
//! pane's week -- and what counts against them.
//!
//! In a dry run a would-move counts: it is what a live run would have
//! done, and a replay of the dry run depends on the count.  Live, only
//! a move that was made counts, a `moved` line the main thread wrote;
//! would-moves that were never made do not.  When the mode changes the
//! counts are made again from the kept events, so a run switched to live
//! does not start with its limits used by moves that never happened.

use super::{Allocation, WEEK_S};
use super::super::allocation_events::{Event, Tail};
use crate::plugins::allocation::replay::Why;

/// The span the hourly band counts over.
const HOUR_S: i64 = 3600;

/// The span the daily budget counts over.
const DAY_S: i64 = 24 * HOUR_S;

/// The reason a `would_move` or `moved` line gives.
fn why_of(detail: &str) -> Option<Why> {
    detail.split(' ').find_map(|w| w.strip_prefix("why=")).and_then(Why::from_word)
}

impl Allocation {
    /// One move counted against the limits it falls under.
    pub(in super::super) fn count_move(&mut self, at: i64, sid: u64, why: Why) {
        if matches!(why, Why::Plan | Why::Rescue | Why::UOut | Why::Waste) {
            self.band_moves.push_back(at);
        }
        if why == Why::Waste {
            self.day_moves.push_back(at);
        }
        if why.resting()
            && let Some(l) = self.last.get_mut(&sid)
        {
            l.rest_moved(at);
        }
    }

    /// The counts from `events`, oldest first: would-moves in a dry
    /// run, moves made when live.
    pub(in super::super) fn count_from(&mut self, events: &[Event], now: i64) {
        let kind = if self.live { "moved" } else { "would_move" };
        for e in events.iter().filter(|e| e.kind == kind && now - e.at < WEEK_S) {
            let Some(why) = why_of(&e.detail) else { continue };
            self.count_move(e.at, e.sid, why);
        }
        self.band_moves.retain(|t| now - t < HOUR_S);
        self.day_moves.retain(|t| now - t < DAY_S);
    }

    /// Set the mode for the scans that follow; on a change, count
    /// again from the kept events.
    pub(in super::super) fn set_live(&mut self, live: bool, now: i64) {
        if live == self.live {
            return;
        }
        self.live = live;
        if !self.rebuilt {
            return;
        }
        self.band_moves.clear();
        self.day_moves.clear();
        for l in self.last.values_mut() {
            l.rest_moves.clear();
        }
        let events = self.events.read_since(now - WEEK_S, now);
        self.count_from(&events, now);
        // what the recount read is not read again as new
        self.tail = Tail::default();
        self.events.read_new(&mut self.tail, now);
    }

    /// The moves the main thread made since the last scan, counted when live.
    pub(in super::super) fn count_made(&mut self, now: i64) {
        let fresh = self.events.read_new(&mut self.tail, now);
        if self.live {
            let made: Vec<Event> = fresh.into_iter().filter(|e| e.kind == "moved").collect();
            self.count_from(&made, now);
        }
    }
}

#[cfg(test)]
#[path = "allocation_caps_tests.rs"]
mod tests;

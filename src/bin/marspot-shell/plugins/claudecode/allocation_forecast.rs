//! The planner's forecast over Claude's panes, in shadow: built from the
//! facts at the first scan of each hour and logged as `sched.forecast`
//! when it changes.  Nothing decides by it yet.
//!
//! What it is built from is folded as the facts come in (`alloc_events`
//! read from where the last scan stopped; at start every kept day), so
//! a scan reads only the bytes appended since.  The hourly roll-up and
//! the helper log are read again once an hour, the helper log only when
//! it grew.

use std::path::{Path, PathBuf};

use marspot_agent::sched::forecast::{self, Boundary, Facts, HOUR, keep_from};

use super::allocation_adapter::events_dir;
use super::allocation_events::{Event, EventLog, Tail};
use super::allocation_hours;
use super::allocation_prices::{Tokens, weeks};
use crate::plugins::allocation::Class;

pub(super) struct Forecasting {
    facts: Facts,
    tail: Tail,
    started: bool,
    hours_dir: PathBuf,
    helpers_log: PathBuf,
    helpers_len: Option<u64>,
    /// The hour the forecast was last built in.
    built: Option<i64>,
    said: String,
    last: Option<forecast::Forecast>,
}

impl Forecasting {
    pub(super) fn new(state_root: &Path) -> Self {
        let hours_dir = events_dir(state_root).with_file_name("alloc_hours");
        let helpers_log = state_root.join("helpers").join("events.log");
        Forecasting { facts: Facts::default(), tail: Tail::default(), started: false, hours_dir, helpers_log, helpers_len: None, built: None, said: String::new(), last: None }
    }

    /// One scan: fold the new facts; at the first scan of an hour build
    /// the forecast for the live panes `live`.  The line when it changed.
    pub(super) fn scan(&mut self, events: &EventLog, live: &[u64], now: i64) -> Option<String> {
        if !self.started {
            self.started = true;
            for e in events.read_since(keep_from(now), now) {
                fold(&mut self.facts, &e);
            }
            // the first read only finds where the files end
            events.read_new(&mut self.tail, now);
        }
        for e in events.read_new(&mut self.tail, now) {
            fold(&mut self.facts, &e);
        }
        let hour = now.div_euclid(HOUR);
        if self.built == Some(hour) {
            return None;
        }
        self.built = Some(hour);
        self.facts.prune(now);
        self.facts.set_hours(allocation_hours::read_rows(&self.hours_dir));
        self.read_helpers();
        let built = forecast::build(&self.facts, now, live);
        let line = built.line();
        self.last = Some(built);
        (line != self.said).then(|| {
            self.said = line.clone();
            line
        })
    }

    /// The forecast last built.
    pub(super) fn last(&self) -> Option<&forecast::Forecast> {
        self.last.as_ref()
    }

    fn read_helpers(&mut self) {
        let len = std::fs::metadata(&self.helpers_log).map(|m| m.len()).ok();
        if len == self.helpers_len {
            return;
        }
        self.helpers_len = len;
        let text = std::fs::read_to_string(&self.helpers_log).unwrap_or_default();
        let lives = helper_lives(&text);
        self.facts.set_helpers(lives.iter().map(|(a, s)| (a.as_str(), *s)));
    }
}

/// One fact into the store: use priced as today, turn ends, and every
/// other fact of a pane as a sign it was there.
fn fold(f: &mut Facts, e: &Event) {
    if e.sid == 0 {
        return;
    }
    match e.kind.as_str() {
        "used" => {
            let model = e.detail.split(' ').find_map(|w| w.strip_prefix("model="));
            if let (Some(m), Some(t)) = (model, Tokens::parse(&e.detail)) {
                let class = if e.class == "fable" { Class::Restricted } else { Class::Plain };
                f.used(e.at, e.sid, class, weeks(m, &t));
            }
        }
        "boundary" => {
            let word = |k: &str| e.detail.split(' ').find_map(|w| w.strip_prefix(k));
            let b = Boundary {
                at: e.at,
                clean: word("holds=") == Some("-"),
                tasks: word("tasks=").and_then(|v| v.parse().ok()).unwrap_or(0),
                oldest_s: word("oldest_s=").and_then(|v| v.parse().ok()),
            };
            f.boundary(e.sid, b);
        }
        _ => f.seen(e.at, e.sid),
    }
}

/// Finished helpers in the helper log, `(agent, seconds)` in the order
/// they stopped: from `started` (or the `carried` line a rotation
/// starts with) to `stopped`.
pub(super) fn helper_lives(text: &str) -> Vec<(String, u32)> {
    let mut started: std::collections::HashMap<&str, (u64, &str)> = std::collections::HashMap::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let mut f = line.split('\t');
        let (Some(_), Some(at), Some(sid), Some(kind)) = (f.next(), f.next(), f.next(), f.next()) else { continue };
        let Ok(at) = at.parse::<u64>() else { continue };
        let arg = |k: &str| line.split('\t').skip(4).find_map(|w| w.strip_prefix(k));
        match kind {
            "started" => {
                started.insert(sid, (at, arg("agent=").unwrap_or("-")));
            }
            "carried" => {
                if let Some(s) = arg("started_ms=").and_then(|v| v.parse().ok()) {
                    started.insert(sid, (s, arg("agent=").unwrap_or("-")));
                }
            }
            "stopped" => {
                if let Some((s, agent)) = started.remove(sid) {
                    out.push((agent.to_string(), (at.saturating_sub(s) / 1000) as u32));
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
#[path = "allocation_forecast_tests.rs"]
mod tests;

//! How fast each account's windows fill, from the feed's successive
//! readings, and how long an account lasts at that speed.
//!
//! The feed only says where a window stands. Telling a pane at the
//! last percent left it minutes: on 2026-11-24 seven panes heard P3
//! was nearly out at 03:15:46 and were all refused at 03:20:53. What
//! matters is how long the account lasts, which takes its speed.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use marspot::cc_usage::{CcAccount, CcUsage};

use crate::plugins::LogLevel;

use super::heads_up::nearly_out;
use super::quota::{same_model, Room, FIVE_HOUR_SPENT};

/// Readings older than this say nothing about the speed now, by the
/// clock: a feed that stopped, or one read again after a restart, does
/// not keep an old speed.
pub(super) const LOOKBACK_S: i64 = 90 * 60;
/// The feed counts whole percent: over a shorter span one step reads
/// as a burst.
pub(super) const MIN_SPAN_S: i64 = 30 * 60;
/// An account that cannot last this long at its speed is running short.
pub(super) const SHORT_H: f64 = 2.0;
/// Fable's cap as a share of the week: one point of Fable is this many
/// points of the week. Regressed over 32 account-days of the week's
/// rise on Fable's and on other models' use.
pub(super) const KAPPA: f64 = 0.528;
const MAX_SAMPLES: usize = 512;
/// A reading this much below the one before is a drop, not noise.
const DROP: f64 = 0.005;

/// One window's readings over the last `LOOKBACK_S`.
#[derive(Default, Debug, Clone)]
struct Series(VecDeque<(i64, f64)>);

impl Series {
    /// True when the reading is new.
    fn add(&mut self, at: i64, util: f64) -> bool {
        if at <= 0 {
            return false;
        }
        if let Some(&(last_at, last_u)) = self.0.back() {
            if at <= last_at {
                return false;
            }
            // readings do go down within a week (2026-11-22 P3 lost
            // about half its week); the speed is measured from the drop
            if util < last_u - DROP {
                self.0.clear();
            }
        }
        self.0.push_back((at, util));
        while self.0.front().is_some_and(|(t, _)| *t < at - LOOKBACK_S) || self.0.len() > MAX_SAMPLES {
            self.0.pop_front();
        }
        true
    }

    /// Share of the window used per hour, once the readings span long
    /// enough to say and the newest is still recent at `now`.
    fn per_hour(&self, now: i64) -> Option<f64> {
        let (&(t0, u0), &(t1, u1)) = (self.0.front()?, self.0.back()?);
        (t1 - t0 >= MIN_SPAN_S && self.fresh(now)).then(|| (u1 - u0).max(0.0) * 3600.0 / (t1 - t0) as f64)
    }

    fn fresh(&self, now: i64) -> bool {
        self.0.back().is_some_and(|(t, _)| *t >= now - LOOKBACK_S)
    }
}

/// Every account's windows, by address and window label (`7d`, `5h`,
/// or a model cap's label in lower case).
#[derive(Default)]
pub(super) struct Trends {
    series: HashMap<(String, String), Series>,
    changed: bool,
}

impl Trends {
    /// Take in what the feed reads now. A window the feed leaves out
    /// for a while keeps its readings until they age out.
    pub(super) fn note(&mut self, accounts: &[CcAccount], now: i64) {
        for a in accounts {
            if let Some(u) = a.util_7d {
                self.add(&a.email, "7d", a.collected_at, u, now);
            }
            if let Some(u) = a.util_5h {
                self.add(&a.email, "5h", a.collected_at, u, now);
            }
            for m in &a.model_limits {
                if let Some(u) = m.util {
                    self.add(&a.email, &m.label.to_ascii_lowercase(), a.limits_as_of.unwrap_or(a.collected_at), u, now);
                }
            }
        }
        let before = self.series.len();
        self.series.retain(|_, s| s.fresh(now));
        self.changed |= self.series.len() != before;
    }

    /// A reading already past `LOOKBACK_S` at `now` is not taken: it
    /// would start a series that ages out at once, every scan.
    fn add(&mut self, mail: &str, label: &str, at: i64, util: f64, now: i64) {
        if at < now - LOOKBACK_S {
            return;
        }
        let key = (mail.to_string(), label.to_string());
        self.changed |= self.series.entry(key).or_default().add(at, util);
    }

    /// Every window's speed at `now`, for the sweep.
    pub(super) fn speeds(&self, now: i64) -> Speeds {
        Speeds(self.series.iter().filter_map(|(k, s)| Some((k.clone(), s.per_hour(now)?))).collect())
    }

    /// One line per reading: `address label at util`.
    fn encode(&self) -> String {
        let mut keys: Vec<&(String, String)> = self.series.keys().collect();
        keys.sort();
        let mut out = String::new();
        for k in keys {
            for (at, u) in &self.series[k].0 {
                out.push_str(&format!("{}\t{}\t{at}\t{u}\n", k.0, k.1));
            }
        }
        out
    }

    /// Read back what `encode` wrote; a line that does not parse is
    /// dropped.
    pub(super) fn decode(text: &str) -> Self {
        let mut t = Trends::default();
        for l in text.lines() {
            let f: Vec<&str> = l.split('\t').collect();
            if let [mail, label, at, u] = f[..]
                && let (Ok(at), Ok(u)) = (at.parse(), u.parse())
            {
                t.add(mail, label, at, u, at);
            }
        }
        t.changed = false;
        t
    }

    /// The text to write, when a reading came in since the last write.
    pub(super) fn changed(&mut self) -> Option<String> {
        std::mem::take(&mut self.changed).then(|| self.encode())
    }
}

/// Each window's share used per hour, as the scan measured it.
#[derive(Default, Debug, Clone)]
pub(super) struct Speeds(HashMap<(String, String), f64>);

impl Speeds {
    pub(super) fn per_hour(&self, mail: &str, label: &str) -> Option<f64> {
        self.0.get(&(mail.to_string(), label.to_string())).copied()
    }
}

/// Use per hour, in points of the week: all of it, and the part on
/// Fable, which also fills the Fable cap.
#[derive(Default, Debug, Clone, Copy, PartialEq)]
pub(super) struct Load {
    pub week: f64,
    pub fable: f64,
}

/// What an account looks like for a pane on `model`.
pub(super) struct Outlook<'a> {
    pub speeds: &'a Speeds,
    pub account: &'a CcAccount,
    pub room: &'a Room,
    pub now: i64,
}

impl Outlook<'_> {
    /// Hours until the first window bounding a pane on `model` fills,
    /// with `added` more on it, or `None` while no such window's speed
    /// is known. `unknown` stands in for a window's own speed where it
    /// is not known yet. A window that
    /// renews before it fills never runs out. The five hours are taken
    /// at their own speed only: how a point of the week weighs there is
    /// not known.
    pub(super) fn hours_left(&self, model: Option<&str>, added: Load, unknown: Option<Load>) -> Option<f64> {
        let Room::Known { util_5h, util_7d, models, .. } = self.room else { return None };
        let a = self.account;
        let mut least: Option<f64> = None;
        let mut window = |left: f64, reset: Option<i64>, speed: Option<f64>| {
            let Some(speed) = speed else { return };
            let h = if speed <= 0.0 { f64::INFINITY } else { left.max(0.0) / speed };
            let renews_first = reset.is_some_and(|r| r > self.now && ((r - self.now) as f64 / 3600.0) < h);
            let h = if renews_first { f64::INFINITY } else { h };
            least = Some(least.map_or(h, |l| l.min(h)));
        };
        if let Some(u) = util_7d {
            let speed = self.speeds.per_hour(&a.email, "7d").or(unknown.map(|u| u.week)).map(|s| s + added.week);
            window(1.0 - u, a.reset_7d, speed);
        }
        if let Some(u) = util_5h {
            window(FIVE_HOUR_SPENT - u, a.reset_5h, self.speeds.per_hour(&a.email, "5h"));
        }
        if let Some(m) = model {
            let want = m.to_ascii_lowercase();
            for (label, u) in models {
                let Some(u) = u.filter(|_| same_model(&want, label)) else { continue };
                let cap = a.model_limits.iter().find(|x| x.label.eq_ignore_ascii_case(label));
                let speed = self.speeds.per_hour(&a.email, label).or(unknown.map(|u| u.fable / KAPPA)).map(|s| s + added.fable / KAPPA);
                window(1.0 - u, cap.and_then(|c| c.reset), speed);
            }
        }
        least
    }

    /// Is this account, for a pane on `model`, unable to last
    /// `SHORT_H` at the speed it is used? One step from out counts
    /// whatever the speed, which covers the half hour after a drop or
    /// a first reading before a speed is known.
    pub(super) fn short(&self, model: Option<&str>) -> bool {
        if *self.room == Room::Unknown || self.room.refused_for(model) {
            return false;
        }
        nearly_out(self.room, model) || self.hours_left(model, Load::default(), None).is_some_and(|h| h < SHORT_H)
    }
}

/// The readings, kept by the scan thread in `usage_trend.tsv` so a
/// restart of the shell -- every silent update is one -- does not wait
/// half an hour for a speed again.
pub(super) const TRENDS_FILE: &str = "usage_trend.tsv";

#[derive(Default)]
pub(super) struct TrendCache {
    trends: Trends,
    file: Option<PathBuf>,
}

impl TrendCache {
    pub(super) fn at(file: PathBuf) -> Self {
        TrendCache { trends: Trends::default(), file: Some(file) }
    }

    /// Read back what an earlier shell wrote.
    pub(super) fn load(&mut self) {
        if let Some(text) = self.file.as_ref().and_then(|f| std::fs::read_to_string(f).ok()) {
            self.trends = Trends::decode(&text);
        }
    }

    /// Take in this scan's feed, write the readings when they changed,
    /// and say every window's speed now.
    pub(super) fn take(&mut self, usage: Option<&CcUsage>, now: i64, log: &mut Vec<(LogLevel, &'static str, String)>) -> Speeds {
        self.trends.note(usage.map_or(&[][..], |u| &u.accounts), now);
        if let (Some(text), Some(file)) = (self.trends.changed(), self.file.as_ref())
            && let Err(e) = std::fs::write(file, text)
        {
            log.push((LogLevel::Warn, "quota.trend_persist_failed", format!("{e}")));
        }
        self.trends.speeds(now)
    }
}

#[cfg(test)]
#[path = "burn_tests.rs"]
mod tests;

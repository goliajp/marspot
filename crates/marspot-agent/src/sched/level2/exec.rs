//! What the executor would do with one committed switch at one turn
//! boundary of its pane: the stopping rule, cheapest kind first.
//!
//! 1. A clean rest (nothing live, a minute of quiet): switch.
//! 2. A short boundary (nothing live, five seconds): switch once the
//!    chance of a clean rest before the deadline is under `SHORT_CHANCE`.
//! 3. Otherwise a notice, timed so the notice's five minutes and the
//!    profile's longest wait for a boundary end at the deadline: a drain
//!    notice ("do not start the next one") for a session with long
//!    background work, else the five-minute notice.  After either the
//!    switch is taken at the first boundary with nothing live.  Over
//!    background work a five-minute notice switches at the first
//!    boundary once its five minutes are over, and a drain notice once
//!    the wait is over too, if the work is worth less than the switch's
//!    gain; work worth more cancels it and the session is told to carry
//!    on.  A profile that does not notice working panes never switches
//!    over background work: it cancels once the wait is over, inside the
//!    half hour a notice must be answered in.
//!
//! Never inside a turn, never while the user is at the pane.  A profile
//! that does not notice working panes notices only for a switch that
//! averts a refusal soon.  Nothing here executes anything: the answer
//! is written as a `would_switch` or `would_notify` fact while the
//! executor is in its dry run.

use super::{Kind, Limits};

/// A clean boundary this long is a clean rest, and this long a short one, seconds.
pub const CLEAN_S: i64 = 60;
pub const SHORT_S: i64 = 5;
/// A short boundary is taken when a clean rest before the deadline is
/// less likely than this.
pub const SHORT_CHANCE: f64 = 0.8;
/// The notice says the switch comes in this many seconds.
pub const NOTICE_S: i64 = 300;

/// One committed switch as the executor watches it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Watched {
    pub open_at: i64,
    pub dead_at: i64,
    /// It averts a refusal soon (the profile may notice working panes for it).
    pub exempt: bool,
    pub clean_per_h: f64,
    pub long_background: bool,
    /// Background work a switch now would throw away, and what it gains, weeks.
    pub lost_work: f64,
    pub gain: f64,
    /// When a notice for it went out.
    pub noticed_at: Option<i64>,
}

/// What the pane is doing at a boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Boundary {
    pub at: i64,
    pub turn_running: bool,
    /// Background tasks or a process under the agent are live.
    pub live: bool,
    /// The user is at the pane (focus, a key in ten minutes, text in the box).
    pub user: bool,
    /// Seconds the boundary has been quiet.
    pub quiet_s: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Would {
    Wait,
    Switch(Kind),
    Notify(Kind),
    /// The background work is worth more than the switch: tell the
    /// session to continue.
    Cancel,
}

impl Would {
    pub fn word(self) -> String {
        match self {
            Would::Wait => "wait".into(),
            Would::Switch(k) => format!("switch kind={}", k.word()),
            Would::Notify(k) => format!("notify kind={}", k.word()),
            Would::Cancel => "cancel".into(),
        }
    }
}

/// The chance of a clean rest within `s` seconds at `per_h` an hour.
fn clean_chance(per_h: f64, s: i64) -> f64 {
    1.0 - crate::detmath::exp(-per_h * s.max(0) as f64 / 3600.0)
}

pub fn decide(w: &Watched, b: &Boundary, lim: &Limits) -> Would {
    if b.at < w.open_at || b.turn_running || b.user {
        return Would::Wait;
    }
    let wait_s = i64::from(lim.notice_wait_min) * 60;
    if let Some(at) = w.noticed_at {
        return after_notice(w, b, lim, at, wait_s);
    }
    if !b.live && b.quiet_s >= CLEAN_S {
        return Would::Switch(Kind::Clean);
    }
    if !b.live && b.quiet_s >= SHORT_S && clean_chance(w.clean_per_h, w.dead_at - b.at) < SHORT_CHANCE {
        return Would::Switch(Kind::Short);
    }
    if !lim.notice_working && !w.exempt || b.at < w.dead_at - NOTICE_S - wait_s {
        return Would::Wait;
    }
    Would::Notify(if w.long_background { Kind::Drain } else { Kind::Notice })
}

fn after_notice(w: &Watched, b: &Boundary, lim: &Limits, at: i64, wait_s: i64) -> Would {
    let kind = if w.long_background { Kind::Drain } else { Kind::Notice };
    if !b.live {
        return Would::Switch(kind);
    }
    let over = at + NOTICE_S + if kind == Kind::Drain || !lim.notice_working { wait_s } else { 0 };
    if b.at < over {
        return Would::Wait;
    }
    if !lim.notice_working || w.lost_work >= w.gain { Would::Cancel } else { Would::Switch(kind) }
}

/// Deadlines held at most.
pub const DEADLINES_CAP: usize = 128;

/// Each committed switch's deadline as the executor holds it: the
/// earliest any pass gave it since its window opened.  Every pass works
/// the windows out again, and a switch that only saves reserve has its
/// deadline where its remaining gain falls under its cost, which moves
/// on with the clock while the gain holds: a notice timed against it
/// would never go.  A switch no pass hands over any more is forgotten,
/// and so is everything when a pass hands over no plan at all.  A held
/// deadline past by more than the profile's wait while the plan still
/// hands the switch over is stale: the plan's value starts again.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Deadlines {
    held: Vec<(u64, u8, u8, i64)>,
    next: Vec<(u64, u8, u8, i64)>,
}

impl Deadlines {
    /// `w` for pane `sid`'s switch from `from` to `to`, with its deadline
    /// held, and the held deadline dropped as stale if one was.
    pub fn hold(&mut self, sid: u64, from: u8, to: u8, mut w: Watched, now: i64, wait_s: i64) -> (Watched, Option<i64>) {
        if w.open_at > now {
            return (w, None);
        }
        let mut stale = None;
        if let Some(d) = self.held.iter().find(|h| h.0 == sid && h.1 == from && h.2 == to).map(|h| h.3) {
            if d < now - wait_s && d < w.dead_at {
                stale = Some(d);
            } else {
                w.dead_at = w.dead_at.min(d);
            }
        }
        if self.next.len() < DEADLINES_CAP {
            self.next.push((sid, from, to, w.dead_at));
        }
        (w, stale)
    }

    /// The pass is over: what it did not hand over is forgotten.
    pub fn passed(&mut self) {
        std::mem::swap(&mut self.held, &mut self.next);
        self.next.clear();
    }
}

#[path = "exec_record.rs"]
mod record;
pub use record::{parse, record, replay, words};

#[cfg(test)]
#[path = "exec_tests.rs"]
mod tests;

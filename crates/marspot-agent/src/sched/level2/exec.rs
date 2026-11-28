//! What the executor would do with one committed switch at one turn
//! boundary of its pane: the stopping rule, cheapest kind first.
//!
//! 1. A clean rest (nothing live, a minute of quiet): switch.
//! 2. A short boundary (nothing live, five seconds): switch once the
//!    chance of a clean rest before the deadline is under `SHORT_CHANCE`.
//! 3. Otherwise a notice: a drain notice at the window's opening for a
//!    session with long background work ("do not start the next one"),
//!    else the five-minute notice: at once for a working pane when a
//!    clean rest before the deadline is unlikely (under `SHORT_CHANCE`),
//!    else timed so the
//!    notice and the profile's longest wait for a boundary end at the
//!    deadline.  After a notice
//!    the switch is taken at the first boundary with nothing live, or,
//!    once the notice's five minutes are over, at the first boundary
//!    over background work worth less than the switch's gain;
//!    background work worth more cancels it and the session is told to
//!    carry on.  A pane whose agent always has a process under it would
//!    otherwise wait out the whole wait and then need one more boundary
//!    before the deadline.
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
        let kind = if w.long_background { Kind::Drain } else { Kind::Notice };
        if !b.live {
            return Would::Switch(kind);
        }
        if b.at < at + NOTICE_S {
            return Would::Wait;
        }
        return if w.lost_work >= w.gain { Would::Cancel } else { Would::Switch(kind) };
    }
    if !b.live && b.quiet_s >= CLEAN_S {
        return Would::Switch(Kind::Clean);
    }
    if !b.live && b.quiet_s >= SHORT_S && clean_chance(w.clean_per_h, w.dead_at - b.at) < SHORT_CHANCE {
        return Would::Switch(Kind::Short);
    }
    if !lim.notice_working && !w.exempt {
        return Would::Wait;
    }
    if w.long_background {
        return Would::Notify(Kind::Drain);
    }
    // a resting pane is left to reach a short or clean boundary first
    let unlikely = b.live && clean_chance(w.clean_per_h, w.dead_at - b.at) < SHORT_CHANCE;
    if unlikely || b.at >= w.dead_at - NOTICE_S - wait_s { Would::Notify(Kind::Notice) } else { Would::Wait }
}

#[path = "exec_record.rs"]
mod record;
pub use record::{parse, record, replay, words};

#[cfg(test)]
#[path = "exec_tests.rs"]
mod tests;

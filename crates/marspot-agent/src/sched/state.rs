//! What the scheduler remembers between decisions, in a fixed size:
//! the last answer said for each pane (so an answer is logged once,
//! when it changes), the prices last used for each account, and the
//! week's spending for efficiency.  All of it is derived and rebuilt
//! from nothing after a restart.

use super::price::{Price, crossed, held};

/// Panes remembered; the one unseen longest makes room for a new one.
pub const PANES: usize = 64;

/// Accounts remembered, by id.
pub const ACCOUNTS: usize = 32;

const WEEK_S: i64 = 7 * 24 * 3600;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PaneSlot {
    sid: u64,
    said: u64,
    at: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct AccountSlot {
    used: bool,
    week: f32,
    window: f32,
}

/// Which of an account's prices crossed 0.5 when last set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Crossed {
    pub week: bool,
    pub window: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct State {
    panes: [PaneSlot; PANES],
    accounts: [AccountSlot; ACCOUNTS],
    week_eff: f64,
    week_from: Option<i64>,
}

impl Default for State {
    fn default() -> Self {
        State { panes: [PaneSlot::default(); PANES], accounts: [AccountSlot::default(); ACCOUNTS], week_eff: 0.0, week_from: None }
    }
}

impl State {
    /// Records `key` as pane `sid`'s answer; true when it differs from
    /// the last one recorded (or none was).
    pub fn said(&mut self, sid: u64, key: u64, now: i64) -> bool {
        let i = match self.panes.iter().position(|s| s.at != 0 && s.sid == sid) {
            Some(i) => i,
            None => (0..PANES).min_by_key(|i| self.panes[*i].at).unwrap_or(0),
        };
        let s = &mut self.panes[i];
        let changed = s.at == 0 || s.sid != sid || s.said != key;
        *s = PaneSlot { sid, said: key, at: now.max(1) };
        changed
    }

    /// The prices to use for account `id` given its fresh ones, and
    /// which of them crossed 0.5.  Ids past `ACCOUNTS` are not held.
    pub fn price(&mut self, id: u8, fresh: Price) -> (Price, Crossed) {
        let Some(s) = self.accounts.get_mut(usize::from(id)) else { return (fresh, Crossed::default()) };
        let last = s.used.then_some((f64::from(s.week), f64::from(s.window)));
        let p = Price { week: held(fresh.week, last.map(|l| l.0)), window: held(fresh.window, last.map(|l| l.1)) };
        let c = Crossed { week: crossed(last.map(|l| l.0), p.week), window: crossed(last.map(|l| l.1), p.window) };
        *s = AccountSlot { used: true, week: p.week as f32, window: p.window as f32 };
        (p, c)
    }

    /// Points spent for efficiency in the week that holds `now`.
    pub fn week_eff(&mut self, now: i64) -> f64 {
        if self.week_from.is_none_or(|from| now - from >= WEEK_S) {
            self.week_from = Some(now);
            self.week_eff = 0.0;
        }
        self.week_eff
    }

    /// Takes a choice's spending for efficiency.
    pub fn set_week_eff(&mut self, after: f64, now: i64) {
        self.week_eff(now);
        self.week_eff = after;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sched_state_says_each_answer_once_and_evicts_the_oldest() {
        let mut s = State::default();
        assert!(s.said(7, 1, 10));
        assert!(!s.said(7, 1, 11));
        assert!(s.said(7, 2, 12));
        for sid in 100..100 + PANES as u64 {
            s.said(sid, 0, 20 + sid as i64);
        }
        // pane 7 was the oldest and made room
        assert!(s.said(7, 2, 1000));
    }

    #[test]
    fn sched_state_holds_prices_and_resets_the_week() {
        let mut s = State::default();
        let (p, c) = s.price(3, Price { week: 0.4, window: 0.0 });
        assert_eq!((p.week, c), (0.4, Crossed::default()));
        let (p, c) = s.price(3, Price { week: 0.55, window: 0.0 });
        assert!((p.week - 0.4).abs() < 1e-6 && !c.week);
        let (p, c) = s.price(3, Price { week: 0.7, window: 0.0 });
        assert!((p.week - 0.7).abs() < 1e-6 && c.week);
        assert_eq!(s.price(200, Price { week: 0.9, window: 0.1 }).0.week, 0.9);
        s.set_week_eff(0.6, 100);
        assert_eq!(s.week_eff(100 + WEEK_S - 1), 0.6);
        assert_eq!(s.week_eff(100 + WEEK_S), 0.0);
    }
}

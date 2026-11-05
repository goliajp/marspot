//! Moving a pane that is not stuck onto quota that would otherwise
//! lapse.
//!
//! Until 2026-11-05 a pane moved only when its account refused it, so
//! a pane stayed wherever it happened to start.  That day five Opus
//! panes worked on an account at 2% of a week that renewed in seven
//! days, while an account whose Fable was spent and whose week renewed
//! the next afternoon had nobody on it, and another's last 2% was due
//! to lapse the next night.  Quota left in a window at its renewal is
//! gone; a fresh week is not going anywhere.
//!
//! A move restarts the pane's claude, so this only touches a pane that
//! is resting, and only for a difference worth a restart.  The cost of
//! the restart -- the prompt cache does not survive it, so the next
//! turn sends the conversation again -- is charged to the account the
//! pane moves to, which is the one whose quota was about to lapse.

use std::time::Duration;

use super::quota::{best_profile, needs_fable, Room};

/// How much sooner the other account's week has to renew to be worth a
/// restart.  Accounts here renew days apart; hours apart is noise
/// against the cost of a restart.
pub(super) const MIN_LEAD_S: i64 = 6 * 3600;
/// How long the session must have written nothing before it counts as
/// resting rather than between two turns.
pub(super) const SETTLE: Duration = Duration::from_secs(60);
/// At most one such move per pane in this long.  The feed moves in
/// minutes, and a pane is not chased around by every sample.
pub(super) const COOLDOWN: Duration = Duration::from_secs(1800);
/// How long a move is taken to be on its way.  The pane reads as being
/// on the account it left until the scan binds the new claude.
pub(super) const LANDING: Duration = Duration::from_secs(120);

/// Why a pane is better off elsewhere.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Why {
    /// It is not on Fable, it sits on an account that still has Fable,
    /// and an account whose Fable is spent has room for it.  Fable
    /// panes can only go where Fable is left.
    KeepsFable,
    /// The other account's week renews this many hours sooner.
    RenewsSooner { hours: i64 },
}

impl std::fmt::Display for Why {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Why::KeepsFable => write!(f, "leaves Fable to the panes that need it"),
            Why::RenewsSooner { hours } => write!(f, "its week renews {hours}h sooner"),
        }
    }
}

/// The account a resting pane on `current` should be on instead, or
/// `None` where it is is good enough.
///
/// The ranking is `best_profile`'s, the same one a refused pane and a
/// badge click get; this only adds how much better is worth a restart.
/// A pane whose own account is out is not this function's business.
pub(super) fn better_home(rooms: &[(u8, Room)], current: u8, model: Option<&str>) -> Option<(u8, Why)> {
    let here = rooms.iter().find(|(p, _)| *p == current).map(|(_, r)| r)?;
    if *here == Room::Unknown || here.refused_for(model) {
        return None;
    }
    let target = best_profile(rooms, current, model)?;
    let there = rooms.iter().find(|(p, _)| *p == target).map(|(_, r)| r)?;
    if there.refused_for(model) {
        return None;
    }
    if !needs_fable(model) {
        match (here.fable_spent(), there.fable_spent()) {
            (false, true) => return Some((target, Why::KeepsFable)),
            // the ranking only offers this when no account whose Fable
            // is spent has room, and then here is the better place
            (true, false) => return None,
            _ => {}
        }
    }
    let lead = here.reset_7d() - there.reset_7d();
    (lead >= MIN_LEAD_S).then_some((target, Why::RenewsSooner { hours: lead / 3600 }))
}

/// What is known about the pane itself.
#[derive(Debug, Clone, Copy)]
pub(super) struct Pane {
    /// The state machine has the pane quiet and waiting for the user.
    pub awaiting_user: bool,
    pub user_here: bool,
    pub work_in_flight: bool,
    pub own_timer: bool,
    /// Since the session last wrote to its transcript.
    pub idle_for: Duration,
    /// Since this pane was last moved this way.
    pub since_last: Option<Duration>,
    /// A move of any kind is on its way and has not landed.
    pub move_in_flight: bool,
    /// The machine's one-minute load average, when it says.
    pub host_load: Option<f64>,
}

/// Why the pane is not to be touched now, or `None` when it may be.
pub(super) fn not_now(p: &Pane) -> Option<&'static str> {
    if p.move_in_flight {
        return Some("a move is on its way");
    }
    if !p.awaiting_user {
        return Some("it is working");
    }
    if p.user_here {
        // they may be typing; the restart would come up under them
        return Some("the user is in it");
    }
    if p.work_in_flight {
        return Some("work is in flight");
    }
    if p.own_timer {
        // a /loop's next wakeup lives in the claude a move kills
        return Some("it waits on its own timer");
    }
    if p.idle_for < SETTLE {
        return Some("it wrote within the last minute");
    }
    if p.since_last.is_some_and(|d| d < COOLDOWN) {
        return Some("it was moved within the half hour");
    }
    // On a saturated machine the restart outruns the checks that watch
    // it: on 2026-11-05, at load 22 to 48, four moves in a row waited
    // thirty seconds for a first byte the new claude had already
    // written, and each pane stood frozen for those thirty seconds.
    // Ten days earlier the same stall hit three refused panes at once.
    // A move that is only an improvement can wait for a quieter minute.
    if p.host_load.is_some_and(|l| l > marspot::host_load::BUSY) {
        return Some("the machine is overloaded");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::super::rooms::rooms_for;
    use super::*;
    use marspot::cc_usage::{CcAccount, CcModelLimit};

    fn resting() -> Pane {
        Pane {
            awaiting_user: true,
            user_here: false,
            work_in_flight: false,
            own_timer: false,
            idle_for: Duration::from_secs(600),
            since_last: None,
            move_in_flight: false,
            host_load: Some(4.0),
        }
    }

    #[test]
    fn a_resting_pane_may_be_moved_and_each_veto_stops_it() {
        assert_eq!(not_now(&resting()), None);
        let cases = [
            (Pane { move_in_flight: true, ..resting() }, "a move is on its way"),
            (Pane { awaiting_user: false, ..resting() }, "it is working"),
            (Pane { user_here: true, ..resting() }, "the user is in it"),
            (Pane { work_in_flight: true, ..resting() }, "work is in flight"),
            (Pane { own_timer: true, ..resting() }, "it waits on its own timer"),
            (Pane { idle_for: Duration::from_secs(59), ..resting() }, "it wrote within the last minute"),
            (Pane { since_last: Some(Duration::from_secs(1799)), ..resting() }, "it was moved within the half hour"),
            (Pane { host_load: Some(22.5), ..resting() }, "the machine is overloaded"),
        ];
        for (pane, why) in cases {
            assert_eq!(not_now(&pane), Some(why));
        }
        assert_eq!(not_now(&Pane { since_last: Some(COOLDOWN), ..resting() }), None);
        assert_eq!(not_now(&Pane { host_load: None, ..resting() }), None, "no answer is not busy");
    }

    /// The feed at 2026-11-05 05:33 UTC, addresses replaced.  All
    /// eight accounts are Ultra, so their 5h does not count.
    const NOW: i64 = 1_793_856_780;

    fn feed() -> Vec<CcAccount> {
        let row = |n: u8, status: &str, u7: f64, reset: i64, fable: f64| CcAccount {
            name: format!("Claude {n}"),
            email: format!("p{n}"),
            status: status.into(),
            util_5h: 0.0,
            util_7d: u7,
            reset_5h: 0,
            reset_7d: reset,
            model_limits: vec![CcModelLimit { label: "Fable".into(), util: fable, reset: Some(reset) }],
            collected_at: NOW - 5,
            limits_as_of: None,
            tier: "default_claude_max_20x".into(),
        };
        vec![
            row(1, "rejected", 1.00, 1_793_934_000, 0.74),
            row(2, "allowed", 0.98, 1_794_171_599, 0.84),
            row(3, "allowed", 0.98, 1_794_114_000, 0.27),
            row(4, "allowed", 0.98, 1_793_977_199, 0.77),
            row(5, "allowed", 0.02, 1_794_448_800, 0.00),
            row(6, "rejected", 0.98, 1_793_941_200, 1.00),
            row(7, "allowed", 0.00, 1_794_283_199, 0.00),
            row(8, "allowed", 0.27, 1_794_373_200, 0.14),
        ]
    }

    fn rooms_at(now: i64) -> Vec<(u8, Room)> {
        rooms_for(&[1, 2, 3, 4, 5, 6, 7, 8], &feed(), |p| Some(format!("p{p}")), now)
    }

    /// Where every pane running that afternoon is sent.
    ///
    /// Before: the four accounts at 98% counted as spent and nothing
    /// that was not refused moved, so all ten stayed put, and the
    /// Fable pane refused on P2 at 05:30 went to P7, a week renewing
    /// on 11-10.
    #[test]
    fn the_panes_of_2026_11_05_go_where_the_quota_lapses_first() {
        let rooms = rooms_at(NOW);
        let opus = Some("opus-5-5");
        let fable = Some("fable-5-1");
        // five Opus panes on P5: P6's Fable is spent and its week has
        // room, and it renews tomorrow
        assert_eq!(better_home(&rooms, 5, opus), Some((6, Why::KeepsFable)));
        // Fable panes on P8 and P7: P4's 2% lapses tomorrow night
        let p8 = better_home(&rooms, 8, fable).unwrap();
        assert_eq!(p8.0, 4);
        assert!(matches!(p8.1, Why::RenewsSooner { hours } if hours > 100), "{p8:?}");
        assert_eq!(better_home(&rooms, 7, fable).map(|m| m.0), Some(4));
        // the refused Fable pane on P2 also goes to P4, not P7
        assert_eq!(best_profile(&rooms, 2, fable), Some(4));
        // an Opus pane already on P6 stays
        assert_eq!(better_home(&rooms, 6, opus), None);
        // a Fable pane on P4 stays: nothing renews sooner with Fable left
        assert_eq!(better_home(&rooms, 4, fable), None);
    }

    /// The next afternoon, once P1 and P6 have renewed with nobody
    /// having read them again: both have a whole week ahead, and P4 is
    /// still the one to empty.
    #[test]
    fn the_day_after_the_renewed_accounts_rank_by_their_next_renewal() {
        let rooms = rooms_at(1_793_941_200 + 3_600);
        assert!(!rooms[0].1.refused_for(Some("fable-5-1")), "P1 has renewed");
        assert_eq!(better_home(&rooms, 5, Some("opus-5-5")).map(|m| m.0), Some(4));
        assert_eq!(better_home(&rooms, 8, Some("fable-5-1")).map(|m| m.0), Some(4));
    }

    /// Hours apart is not worth a restart.
    #[test]
    fn a_renewal_less_than_six_hours_sooner_is_not_worth_a_move() {
        let rooms = vec![
            (1, room(0.5, 10_000 + MIN_LEAD_S - 1, 0.1)),
            (2, room(0.5, 10_000, 0.1)),
        ];
        assert_eq!(better_home(&rooms, 1, Some("opus-5-5")), None);
        let rooms = vec![(1, room(0.5, 10_000 + MIN_LEAD_S, 0.1)), (2, room(0.5, 10_000, 0.1))];
        assert_eq!(better_home(&rooms, 1, Some("opus-5-5")), Some((2, Why::RenewsSooner { hours: 6 })));
    }

    /// A pane not on Fable that sits where Fable is spent is where it
    /// should be, even if an account with Fable left renews sooner.
    #[test]
    fn a_pane_off_fable_does_not_leave_an_account_whose_fable_is_spent_for_one_whose_is_not() {
        let rooms = vec![(1, room(0.5, 900_000, 1.0)), (2, room(0.5, 10_000, 0.1))];
        assert_eq!(better_home(&rooms, 1, Some("opus-5-5")), None);
        assert_eq!(better_home(&rooms, 1, Some("fable-5-1")), None, "a Fable pane is out there");
    }

    /// Out is the refused path's business, and a refused account is no
    /// destination.
    #[test]
    fn nothing_is_moved_from_or_to_an_account_that_is_out() {
        let rooms = vec![(1, room(1.0, 900_000, 0.1)), (2, room(0.5, 10_000, 0.1))];
        assert_eq!(better_home(&rooms, 1, None), None);
        let rooms = vec![(1, room(0.5, 900_000, 0.1)), (2, room(1.0, 10_000, 0.1))];
        assert_eq!(better_home(&rooms, 1, None), None);
        let rooms = vec![(1, Room::Unknown), (2, room(0.5, 10_000, 0.1))];
        assert_eq!(better_home(&rooms, 1, None), None, "nothing known about here");
    }

    fn room(u7: f64, reset: i64, fable: f64) -> Room {
        Room::Known {
            status: "allowed".into(),
            util_5h: 0.0,
            util_7d: u7,
            reset_7d: reset,
            models: vec![("fable".into(), fable)],
            models_measured_at: i64::MAX,
            measured_at: i64::MAX,
        }
    }
}

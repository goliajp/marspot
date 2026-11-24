//! When a pane that is not stuck may be moved: the vetoes every
//! resting move checks, whatever chose it.
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

/// How long the session must have written nothing before it counts as
/// resting rather than between two turns.
pub(super) const SETTLE: Duration = Duration::from_secs(60);
/// At most one such move per pane in this long.  The feed moves in
/// minutes, and a pane is not chased around by every sample.
pub(super) const COOLDOWN: Duration = Duration::from_secs(1800);
/// How long a move is taken to be on its way.  The pane reads as being
/// on the account it left until the scan binds the new claude.
pub(super) const LANDING: Duration = Duration::from_secs(120);

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
    /// It is a helper, or helpers work for it: it rests while it waits
    /// on them, and a restart costs their exchange its prompt cache.
    pub helper_bound: bool,
}

/// Why the pane is not to be touched now, or `None` when it may be.
pub(super) fn not_now(p: &Pane) -> Option<&'static str> {
    if p.move_in_flight {
        return Some("a move is on its way");
    }
    if !p.awaiting_user {
        return Some("it is working");
    }
    if p.helper_bound {
        return Some("it is a helper or helpers work for it");
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
    use super::*;

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
            helper_bound: false,
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
            (Pane { helper_bound: true, ..resting() }, "it is a helper or helpers work for it"),
        ];
        for (pane, why) in cases {
            assert_eq!(not_now(&pane), Some(why));
        }
        assert_eq!(not_now(&Pane { since_last: Some(COOLDOWN), ..resting() }), None);
        assert_eq!(not_now(&Pane { host_load: None, ..resting() }), None, "no answer is not busy");
    }

    #[test]
    fn rebalance_leaves_a_helper_and_a_pane_with_live_helpers() {
        let roles = super::super::with_helpers::HelperRoles::of([(1, 0), (2, 0), (3, 2)]);
        let pane = |sid| Pane { helper_bound: roles.sits_out(sid), ..resting() };
        assert_eq!(not_now(&pane(1)), None);
        assert_eq!(not_now(&pane(2)), Some("it is a helper or helpers work for it"), "its helper is live");
        assert_eq!(not_now(&pane(3)), Some("it is a helper or helpers work for it"), "it is a helper");
    }

}

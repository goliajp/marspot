// CARVE-OUT: pre-existing file: choosing the profile that has room left; shrink on every touch
//! Which profile has room left.
//!
//! Eight accounts back the profiles on this machine, and their limits
//! are not interchangeable: measured 2026-09-29, six of the eight had
//! their seven-day window at 100% and were being refused, while the
//! badge's left click walked to the next profile NUMBER — straight
//! onto a refused account most of the time.
//!
//! The usage feed a separate collector keeps (`marspot::cc_usage`)
//! already says, per account, how much of each rolling window is gone
//! and when it comes back.  This turns that into a choice.
//!
//! **The feed is the only trigger, and its lag is accepted.**  A pane
//! learns it has been refused the moment the API says so; the feed
//! learns on its own schedule, so panes can sit on a spent account for
//! minutes before anything moves them.  Measured 2026-09-30: the feed
//! was written at 07:00:10.641 and the first pane moved at
//! 07:00:11.106 — the sweep is not slow, it was waiting to be told.
//! Adding a second, faster trigger from each pane's own transcript was
//! considered and turned down: one source that is right about every
//! pane at once beats two sources that can disagree.

#[cfg(test)]
use std::time::Duration;

pub(in crate::plugins) use marspot_agent::quota::room::{Refusal, Room, Window};
pub(super) use marspot_agent::quota::room::{FIVE_HOUR_SPENT, needs_fable, same_model};
#[path = "helper_pick.rs"]
pub(in crate::plugins) mod helper_pick;

pub(super) use super::profiles::profile_email;
pub(super) use super::rooms::rooms_for;

/// The profile to move a pane to, or `None` when there is nowhere
/// better than where it already is.
///
/// Order: an account with room, **the one nearest its seven-day
/// renewal first**.  What is left in a window that renews within the
/// hour is about to be replaced whether it is spent or not, while an
/// account that just renewed has to carry a whole week — so the cheap
/// quota goes first and the expensive quota is left alone.  Headroom
/// only breaks ties, and then the account with less left goes first.
///
/// A pane that is not working in Fable goes to an account whose Fable
/// is already spent before anything else: that account's remaining
/// week is no use to the panes that can only run on Fable, while the
/// accounts with Fable left are the only place those panes can go.
/// Callers only ask for a pane whose model is known.
///
/// A profile the feed cannot speak for is never a destination: a pane
/// is only moved to an account known to have room for its model.
/// An account the feed has a row for but no reading of its week comes
/// after every account known to have room and before the refused: it
/// may have a whole week or none, and trying it costs one move where
/// staying costs a pane that cannot work.
/// An account one step from refusing the pane comes after every
/// account with more room, whatever its renewal.
/// Refused accounts last, soonest to come back first, so a caller can
/// tell "every account is out" from "nowhere else to go".
///
/// Landing on an account that refuses immediately is possible and
/// bounded rather than prevented: a pane moves once per refusal and
/// not again for ten minutes, which is the cost of this order and the
/// reason no headroom floor is invented here.
pub(in crate::plugins) fn best_profile(rooms: &[(u8, Room)], current: u8, model: Option<&str>) -> Option<u8> {
    let mut ranked: Vec<&(u8, Room)> = rooms
        .iter()
        .filter(|(p, r)| *p != current && *r != Room::Unknown)
        .collect();
    if ranked.is_empty() {
        return None;
    }
    let needs_fable = needs_fable(model);
    ranked.sort_by(|a, b| {
        let tier = |r: &Room| if r.refused_for(model) { 1 } else { 0 };
        // Only among the usable: the refused are ordered by when they
        // come back, and nothing else.
        let keeps_fable = |r: &Room| tier(r) == 0 && !needs_fable && r.fable_spent();
        let unread = |r: &Room| tier(r) == 0 && !r.known_for(model);
        // one step from refusing: a pane sent there is moved again
        // within the hour, or works on under a warning to wind down
        let low = |r: &Room| tier(r) == 0 && super::heads_up::nearly_out(r, model);
        tier(&a.1)
            .cmp(&tier(&b.1))
            .then_with(|| low(&a.1).cmp(&low(&b.1)))
            .then_with(|| unread(&a.1).cmp(&unread(&b.1)))
            .then_with(|| keeps_fable(&b.1).cmp(&keeps_fable(&a.1)))
            .then_with(|| {
                // Both ends of the list are ordered by the clock: the
                // refused by when they come back, the usable by when
                // their week renews.
                a.1.reset_7d()
                    .cmp(&b.1.reset_7d())
                    .then_with(|| {
                        // the fuller one first: what is left there is
                        // used up before a fresh account is started
                        a.1.headroom_for(model)
                            .partial_cmp(&b.1.headroom_for(model))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
            })
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked.first().map(|(p, _)| *p)
}

#[path = "refused_move.rs"]
mod refused_move;
pub(in crate::plugins) use refused_move::{hold_for, next_move, Move, Situation};
#[cfg(test)]
pub(in crate::plugins) use refused_move::TOOL_HOLD_MAX;

#[cfg(test)]
mod tests {

    fn room(util_5h: f64, util_7d: f64, reset_7d: i64, models: &[(&str, f64)]) -> Room {
        Room::Known {
            status: "allowed".into(),
            util_5h: Some(util_5h),
            util_7d: Some(util_7d),
            reset_7d,
            models: models.iter().map(|(l, u)| (l.to_string(), Some(*u))).collect(),
            // Measured now, in the tests that are not about staleness.
            models_measured_at: i64::MAX, measured_at: i64::MAX,
        }
    }

    /// A logged-out account is nowhere to go, whatever its numbers
    /// last said.
    #[test]
    fn an_account_that_cannot_log_in_is_out_for_every_pane() {
        let logged_out = Room::Known {
            status: "auth_error".into(),
            util_5h: Some(0.0),
            util_7d: Some(0.78),
            reset_7d: 100,
            models: vec![("fable".to_string(), Some(1.0))],
            models_measured_at: i64::MAX,
            measured_at: i64::MAX,
        };
        assert!(logged_out.refused_for(Some("opus-5-5")));
        let r = vec![(1, logged_out), (5, fable_spent_but_open(5_000))];
        let s = Situation { current: 4, needs_model: Some("opus-5-5"), ..situation(&r) };
        assert_eq!(next_move(&s), Move::To(5), "not P1, whose renewal is nearer");
    }

    fn fable_refusal(at: i64) -> Refusal {
        Refusal { at, model: "fable".into(), logged_out: false, window: Window::Week }
    }

    /// An account can be at 7% of its week and still be shut out of
    /// one model.
    ///
    /// Some projects only run on Fable, and Fable is metered on its
    /// own *as well as* against the account's 5h and 7d windows. So a
    /// profile with a fresh week and a spent Fable is not a
    /// destination for a Fable pane -- moving it there moves it
    /// nowhere, and the pane comes straight back refused.
    #[test]
    fn a_fable_pane_does_not_go_to_a_profile_whose_fable_is_spent() {
        let rooms = vec![
            // Nearest renewal and nearly empty -- the ordinary winner.
            (2, room(0.10, 0.07, 1_000, &[("fable", 1.00)])),
            // Further out and fuller, but Fable is free.
            (3, room(0.30, 0.40, 9_000, &[("fable", 0.20)])),
        ];
        assert_eq!(
            best_profile(&rooms, 1, None),
            Some(2),
            "without a model to satisfy, the week decides and P2 wins"
        );
        assert_eq!(
            best_profile(&rooms, 1, Some("fable-5")),
            Some(3),
            "a Fable pane cannot use P2's week, so it takes the one with Fable left"
        );
    }

    /// The other way round: a pane that does not run on Fable takes
    /// an account whose Fable is spent, so the accounts with Fable
    /// left stay for the panes that can only run there.
    #[test]
    fn a_pane_not_on_fable_takes_an_account_whose_fable_is_spent() {
        let rooms = vec![
            // Nearest renewal and nearly empty -- the ordinary winner,
            // and one of the few places a Fable pane can still go.
            (2, room(0.10, 0.07, 1_000, &[("fable", 0.20)])),
            // Further out and fuller, with nothing left for Fable.
            (3, room(0.30, 0.40, 9_000, &[("fable", 1.00)])),
        ];
        assert_eq!(best_profile(&rooms, 1, Some("opus-5-5")), Some(3));
        assert_eq!(
            best_profile(&rooms, 1, Some("fable-5")),
            Some(2),
            "a Fable pane is not steered there"
        );
        assert_eq!(
            best_profile(&rooms, 1, None),
            Some(3),
            "no model given: it is not assumed to be on Fable"
        );
    }

    /// Spent Fable is a preference among accounts that can answer,
    /// not a way past a refusal.
    #[test]
    fn spent_fable_does_not_make_a_refused_account_a_destination() {
        let rooms = vec![
            (2, room(0.10, 1.00, 1_000, &[("fable", 1.00)])),
            (3, room(0.30, 0.40, 9_000, &[("fable", 0.20)])),
        ];
        assert_eq!(best_profile(&rooms, 1, Some("opus-5-5")), Some(3));
    }

    /// A feed with no model rows at all changes nothing.
    #[test]
    fn an_older_feed_without_model_rows_decides_as_before() {
        let rooms = vec![(2, room(0.10, 0.07, 1_000, &[])), (3, room(0.30, 0.40, 9_000, &[]))];
        assert_eq!(best_profile(&rooms, 1, Some("fable-5")), Some(2));
        assert_eq!(best_profile(&rooms, 1, None), Some(2));
    }
    use super::*;
    use marspot::cc_usage::CcAccount;

    /// "Now" for the tests: before every renewal they write.
    const NOW: i64 = 50;








    fn rooms(spec: &[(u8, &str, f64, i64)]) -> Vec<(u8, Room)> {
        spec.iter()
            .map(|(p, status, util7, reset7)| {
                (
                    *p,
                    Room::Known {
                        status: (*status).into(),
                        util_5h: Some(0.0),
                        util_7d: Some(*util7),
                        reset_7d: *reset7, models: Vec::new(), models_measured_at: i64::MAX, measured_at: i64::MAX
                    },
                )
            })
            .collect()
    }

    /// The shape the feed was actually in when every pane on P7 had to
    /// be switched by hand: the five-hour window all but gone, and the
    /// status word still short of saying so.
    #[test]
    fn a_spent_window_is_out_whatever_the_status_says() {
        let warned = Room::Known {
            status: "allowed_warning".into(),
            util_5h: Some(0.99),
            util_7d: Some(0.37),
            reset_7d: 500, models: Vec::new(), models_measured_at: i64::MAX, measured_at: i64::MAX
        };
        assert!(warned.refused_for(None), "99% of the five-hour window is not a warning");

        let fresh = Room::Known {
            status: "allowed_warning".into(),
            util_5h: Some(0.10),
            util_7d: Some(0.80),
            reset_7d: 900, models: Vec::new(), models_measured_at: i64::MAX, measured_at: i64::MAX
        };
        assert!(!fresh.refused_for(None), "a warning with room left is still usable");

        // And it is not offered as somewhere to go.
        let r = vec![(1, Room::Known { status: "rejected".into(), util_5h: Some(0.0), util_7d: Some(1.0), reset_7d: 100 , models: Vec::new(), models_measured_at: i64::MAX, measured_at: i64::MAX}), (7, warned), (2, fresh)];
        assert_eq!(best_profile(&r, 1, None), Some(2));
    }

    fn situation<'a>(rooms: &'a [(u8, Room)]) -> Situation<'a> {
        Situation {
            needs_model: None,
            rooms,
            current: 1,
            tool_executing: false,
            work_running: false,
            moves_this_episode: 0,
            since_last_move: None,
            last_target: None,
            stranded_for: None,
            warned: false,
            background_refused: None,
        }
    }

    /// Background agents hold a refused pane only while they can get
    /// an answer from its account.
    #[test]
    fn background_work_the_account_refuses_does_not_hold_the_pane() {
        let out_of_fable = room(0.10, 0.50, 100, &[("fable", 1.0)]);
        let r = vec![(1, out_of_fable.clone()), (2, room(0.10, 0.20, 900, &[("fable", 0.2)]))];
        let fable = Refusal { model: "fable-5-2".into(), ..fable_refusal(2_000) };
        let opus = Refusal { model: "opus-5-5".into(), ..fable_refusal(2_000) };
        let busy = || Situation { needs_model: Some("fable-5-2"), work_running: true, ..situation(&r) };

        // an agent was refused Fable, and this account is out of Fable
        let s = Situation { background_refused: Some(&fable), ..busy() };
        assert_eq!(next_move(&s), Move::To(2));

        // no agent reported a refusal: the work is taken to be running
        assert_eq!(next_move(&busy()), Move::Hold("work is in flight here"));

        // refused on a model this account still serves: other agents
        // there can go on
        let s = Situation { background_refused: Some(&opus), ..busy() };
        assert_eq!(next_move(&s), Move::Hold("work is in flight here"));

        // a refusal that names no model is about the account's own
        // windows, which here have room
        let unnamed = Refusal { model: String::new(), ..fable_refusal(2_000) };
        let s = Situation { background_refused: Some(&unnamed), ..busy() };
        assert_eq!(next_move(&s), Move::Hold("work is in flight here"));

        // a command in the foreground holds the pane whatever its agents met
        let s = Situation { tool_executing: true, background_refused: Some(&fable), ..busy() };
        assert_eq!(next_move(&s), Move::Hold("a tool is running here"));

        // nowhere with Fable left: it stays, for the reason it always did
        let shut = vec![(1, out_of_fable.clone()), (2, out_of_fable)];
        let s = Situation { rooms: &shut, background_refused: Some(&fable), ..busy() };
        assert_eq!(next_move(&s), Move::Hold("every account is out"));
    }

    /// An account one step from full is not out: a pane there with
    /// work in the background is held, whatever an agent reported.
    #[test]
    fn a_nearly_spent_account_still_holds_a_pane_for_its_background_work() {
        let r = vec![
            (1, room(0.10, 0.99, 100, &[("fable", 0.99)])),
            (2, room(0.10, 0.20, 900, &[("fable", 0.2)])),
        ];
        let fable = Refusal { model: "fable-5-2".into(), ..fable_refusal(2_000) };
        let s = Situation {
            needs_model: Some("fable-5-2"),
            work_running: true,
            background_refused: Some(&fable),
            ..situation(&r)
        };
        assert_eq!(next_move(&s), Move::Hold("work is in flight here"));
    }

    /// The whole policy, as a table.
    #[test]
    fn what_happens_to_a_refused_pane() {
        let r = rooms(&[(1, "rejected", 1.0, 100), (2, "allowed", 0.2, 900), (3, "allowed", 0.6, 400)]);

        // Nearest renewal wins, not most left.
        assert_eq!(next_move(&situation(&r)), Move::To(3));

        // Not while it is in the middle of something.
        let s = Situation { tool_executing: true, ..situation(&r) };
        assert_eq!(next_move(&s), Move::Hold("a tool is running here"));

        // Nor while something of its own runs in the background -- for
        // the same while, and then it goes: a loop polling for a file
        // held a spent pane for half an hour.
        let s = Situation { work_running: true, ..situation(&r) };
        assert_eq!(next_move(&s), Move::Hold("work is in flight here"));
        let s = Situation {
            work_running: true,
            stranded_for: Some(Duration::from_secs(299)),
            ..situation(&r)
        };
        assert_eq!(next_move(&s), Move::Hold("work is in flight here"), "just under five minutes");
        let s = Situation {
            work_running: true,
            stranded_for: Some(Duration::from_secs(301)),
            ..situation(&r)
        };
        assert_eq!(next_move(&s), Move::To(3), "past five minutes");

        // Told in time, it does not wait on its background work at all;
        // a command in the foreground still holds it until it ends.
        let s = Situation { work_running: true, warned: true, ..situation(&r) };
        assert_eq!(next_move(&s), Move::To(3), "warned: background work does not hold");
        let s = Situation { tool_executing: true, work_running: true, warned: true, ..situation(&r) };
        assert_eq!(next_move(&s), Move::Hold("a tool is running here"));

        // But that hold is not forever.  A CLI parked until its window
        // reopens reads exactly like a running tool, and held one pane
        // for nine hours before this bound existed.
        let s = Situation {
            tool_executing: true,
            stranded_for: Some(Duration::from_secs(301)),
            ..situation(&r)
        };
        assert_eq!(next_move(&s), Move::To(3));

        // A move already made, and the pane is not there yet: wait
        // for it to land rather than start a second resume on top of
        // the first.
        let s = Situation { moves_this_episode: 1, last_target: Some(3), since_last_move: Some(Duration::from_secs(5)), ..situation(&r) };
        assert_eq!(next_move(&s), Move::Hold("the last move has not landed"));

        // It landed, and that account refuses it too: the second move
        // is the correction for the first.
        let landed = rooms(&[(1, "allowed", 0.2, 900), (3, "rejected", 1.0, 400), (4, "allowed", 0.5, 500)]);
        let s = Situation { current: 3, moves_this_episode: 1, last_target: Some(3), since_last_move: Some(Duration::from_secs(5)), ..situation(&landed) };
        assert_eq!(next_move(&s), Move::To(4));

        // The third waits.
        let s = Situation { current: 3, moves_this_episode: 2, last_target: Some(3), since_last_move: Some(Duration::from_secs(5)), ..situation(&landed) };
        assert_eq!(next_move(&s), Move::Hold("cooling down"));

        // Unless the wait is over.
        let s = Situation { current: 3, moves_this_episode: 2, last_target: Some(3), since_last_move: Some(Duration::from_secs(601)), ..situation(&landed) };
        assert_eq!(next_move(&s), Move::To(4));

        // Everything out: stay, and say so.
        let all_out = rooms(&[(1, "rejected", 1.0, 100), (2, "rejected", 1.0, 900)]);
        assert_eq!(next_move(&situation(&all_out)), Move::Hold("every account is out"));

        // Only one profile exists.
        let alone = rooms(&[(1, "rejected", 1.0, 100)]);
        assert_eq!(next_move(&situation(&alone)), Move::Hold("nowhere else to go"));
    }

    /// A profile nothing is known about is not somewhere to go.
    #[test]
    fn a_profile_the_feed_cannot_speak_for_is_never_a_destination() {
        let r = vec![(1, Room::Unknown), (2, Room::Unknown)];
        assert_eq!(next_move(&situation(&r)), Move::Hold("nowhere else to go"));
        let mixed = vec![(1, Room::Unknown), (2, Room::Unknown), (3, room(0.1, 0.1, 900, &[]))];
        assert_eq!(best_profile(&mixed, 1, Some("opus-5-5")), Some(3));
    }

    fn acct(email: &str, status: &str, u5: f64, u7: f64, reset7: i64) -> CcAccount {
        CcAccount {
            name: email.into(),
            email: email.into(),
            status: status.into(),
            util_5h: Some(u5),
            util_7d: Some(u7),
            reset_5h: None,
            reset_7d: Some(reset7),
            model_limits: Vec::new(),
            collected_at: 0,
            limits_as_of: None,
            tier: "default_claude_max_5x".into(),
        }
    }

    /// Spend the quota that is about to be replaced.
    ///
    /// Two accounts have room: p2 with more of its week left but a
    /// renewal far off, p4 renewing soon.  p4 wins — what is left in
    /// p4 disappears at the renewal whether it is used or not, while
    /// p2 has to cover the days until its own.  A refused account is
    /// never preferred over either.
    #[test]
    fn a_click_spends_the_week_that_renews_first() {
        let accounts = vec![
            acct("p1", "rejected", 0.0, 1.0, 100),
            acct("p2", "allowed", 0.0, 0.20, 900),
            acct("p3", "allowed_warning", 0.29, 0.87, 800),
            acct("p4", "allowed", 0.0, 0.60, 400),
        ];
        let rooms = rooms_for(&[1, 2, 3, 4], &accounts, |p| Some(format!("p{p}")), NOW);
        assert_eq!(best_profile(&rooms, 3, None), Some(4));
    }

    /// Same renewal, different room: the one with less left goes
    /// first, so it is used up before the other is started on.
    #[test]
    fn headroom_breaks_a_tie_on_the_clock() {
        let accounts = vec![
            acct("p1", "allowed", 0.0, 0.80, 500),
            acct("p2", "allowed", 0.0, 0.20, 500),
        ];
        let rooms = rooms_for(&[1, 2], &accounts, |p| Some(format!("p{p}")), NOW);
        assert_eq!(best_profile(&rooms, 0, None), Some(1));
    }

    /// A profile the feed has never heard of is not chosen over a
    /// refused one: nothing is known about whether it has room.
    #[test]
    fn an_unknown_profile_is_not_chosen_even_over_a_refused_one() {
        let accounts = vec![acct("p1", "rejected", 0.0, 1.0, 400)];
        let rooms = rooms_for(&[1, 9], &accounts, |p| Some(format!("p{p}")), NOW);
        assert_eq!(best_profile(&rooms, 0, None), Some(1), "the refused one, so the caller holds");
    }

    /// Everything is refused: the answer is the one that comes back
    /// first, and it is still an answer — the pane has to sit
    /// somewhere.
    #[test]
    fn when_all_are_refused_the_soonest_reset_wins() {
        let accounts = vec![
            acct("p1", "rejected", 0.0, 1.0, 900),
            acct("p2", "rejected", 0.0, 1.0, 400),
            acct("p3", "rejected", 0.0, 1.0, 700),
        ];
        let rooms = rooms_for(&[1, 2, 3], &accounts, |p| Some(format!("p{p}")), NOW);
        assert_eq!(best_profile(&rooms, 1, None), Some(2));
    }

    /// Nowhere else to go.
    #[test]
    fn a_single_profile_has_no_answer() {
        let rooms = rooms_for(&[1], &[], |_| None, NOW);
        assert_eq!(best_profile(&rooms, 1, None), None);
    }
    /// The feed's numbers for P1 on 2026-10-03, when an Opus pane put
    /// there by hand was moved off it six seconds later: both account
    /// windows with room, Fable spent, and the collector's word
    /// `rejected` -- which it writes when any window is full, the
    /// per-model ones included.
    fn fable_spent_but_open(reset_7d: i64) -> Room {
        Room::Known {
            status: "rejected".into(),
            util_5h: Some(0.0),
            util_7d: Some(0.77),
            reset_7d,
            models: vec![("fable".to_string(), Some(1.0))],
            models_measured_at: i64::MAX,
            measured_at: i64::MAX,
        }
    }

    /// With the rejection read per pane, an Opus pane leaving a spent
    /// account goes where Fable is already gone, and is not held as if
    /// every account were out.
    #[test]
    fn an_opus_pane_moves_to_an_account_whose_only_lack_is_fable() {
        let r = vec![
            (4, room(0.16, 0.05, 9_000, &[("fable", 0.01)])),
            (1, fable_spent_but_open(5_000)),
            (6, Room::Known {
                status: "rejected".into(),
                util_5h: Some(0.08),
                util_7d: Some(0.98),
                reset_7d: 100,
                models: vec![("fable".to_string(), Some(1.0))],
                models_measured_at: i64::MAX,
                measured_at: i64::MAX,
            }),
        ];
        let s = Situation { current: 6, needs_model: Some("opus-5-5"), ..situation(&r) };
        assert_eq!(next_move(&s), Move::To(1), "P4's Fable is kept for the panes that need it");
        let s = Situation { current: 6, needs_model: Some("fable-5"), ..situation(&r) };
        assert_eq!(next_move(&s), Move::To(4));

        // The only other account lacks just Fable: that is a place to
        // go for an Opus pane, not "every account is out".
        let only = vec![r[1].clone(), r[2].clone()];
        let s = Situation { current: 6, needs_model: Some("opus-5-5"), ..situation(&only) };
        assert_eq!(next_move(&s), Move::To(1));
        let s = Situation { current: 6, needs_model: Some("fable-5"), ..situation(&only) };
        assert_eq!(next_move(&s), Move::Hold("every account is out"));
    }

    /// A pane met a login failure on P5 after the feed last read it:
    /// P5 is no destination, though its numbers have room. Once the
    /// feed reads it again, someone has logged in, and it is.
    #[test]
    fn a_profile_a_pane_could_not_log_in_to_is_no_destination_until_it_is_read_again() {
        use super::super::refusal::mark_logged_out;
        use std::collections::HashMap;
        let read_at = |at| Room::Known {
            status: "allowed".into(),
            util_5h: Some(0.18),
            util_7d: Some(0.32),
            reset_7d: 100,
            models: vec![],
            models_measured_at: at,
            measured_at: at,
        };
        let seen = HashMap::from([(5, 2_000)]);
        let mut rooms = vec![(5, read_at(1_000)), (6, room(0.5, 0.9, 900, &[]))];
        mark_logged_out(&mut rooms, &seen);
        let s = Situation { current: 8, ..situation(&rooms) };
        assert_eq!(next_move(&s), Move::To(6), "not P5, whose renewal is nearer");

        let mut rooms = vec![(5, read_at(3_000)), (6, room(0.5, 0.9, 900, &[]))];
        mark_logged_out(&mut rooms, &seen);
        let s = Situation { current: 8, ..situation(&rooms) };
        assert_eq!(next_move(&s), Move::To(5));
    }

    /// The feed writes `null` for a window that has reset since it was
    /// last read, or that has not started.  Nothing is known against an
    /// account reported that way, and with no account known to have
    /// room it is where a pane goes.
    #[test]
    fn an_account_whose_windows_are_all_null_is_somewhere_to_go() {
        let feed = r#"{"generated_at":"2026-10-09T03:10:00+00:00","accounts":[
            {"name":"Claude 1","email":"a@x","status":"allowed",
             "utilization_5h":null,"utilization_7d":null,"reset_5h":null,"reset_7d":null,
             "model_limits":[],"collected_at":"2026-10-04T06:31:50+00:00"}]}"#;
        let usage = marspot::cc_usage::parse(feed).expect("the feed parses");
        let rooms = rooms_for(&[1], &usage.accounts, |_| Some("a@x".into()), NOW);
        assert!(!rooms[0].1.refused_for(Some("fable-5-1")));
        let s = Situation { current: 8, needs_model: Some("fable-5-1"), ..situation(&rooms) };
        assert_eq!(next_move(&s), Move::To(1));
    }
}

#[cfg(test)]
#[path = "tests/home_feed.rs"]
mod home_feed_tests;

#[cfg(test)]
#[path = "tests/five_hour.rs"]
mod five_hour_tests;

#[cfg(test)]
#[path = "tests/week_unknown.rs"]
mod week_unknown_tests;

#[cfg(test)]
#[path = "tests/fable_stranded.rs"]
mod fable_stranded_tests;

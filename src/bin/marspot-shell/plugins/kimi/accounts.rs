//! What is known about each kimi account, in the form the account
//! chooser shared with Claude reads: the usage feed's row for it, and
//! the refusals panes have met there.
//!
//! The feed has a row only for an account the collector could read,
//! and it can read one only while that account's token is live -- a
//! token lasts minutes and is renewed by the CLI while a session is in
//! use.  So an account nobody is on often has no row, or an old one.
//! An account without a row is taken to have room: the one thing known
//! against it would be a refusal, and that is kept here.

use marspot::cc_usage::CcUsage;
use marspot::kimi_usage::account_name;

use crate::plugins::claudecode::quota::{Room, Window};
use crate::plugins::claudecode::rooms::room_of;
use crate::plugins::claudecode::seen::{Resets, Seen};

const WEEK: i64 = 7 * 24 * 3600;
/// A week this full is what a refusal was about, when the feed knows
/// the week's end.
const WEEK_NEARLY_SPENT: f64 = 0.99;

/// An account the feed has no row for: no reading of its week, so an
/// account known to have room is chosen first.
fn unmeasured(now: i64) -> Room {
    Room::Known {
        status: "allowed".into(),
        util_5h: None,
        util_7d: None,
        reset_7d: now + WEEK,
        models: Vec::new(),
        models_measured_at: 0,
        measured_at: 0,
    }
}

fn row(usage: Option<&CcUsage>, n: u8) -> Option<&marspot::cc_usage::CcAccount> {
    let name = account_name(n);
    usage?.accounts.iter().find(|a| a.name == name)
}

/// Each of `numbers` as the chooser sees it at `now`.  An account a
/// pane was refused on is full until the window that refusal was about
/// renews: at the feed's reset time when it gives one, else after the
/// window's length.
pub(super) fn rooms(numbers: &[u8], usage: Option<&CcUsage>, seen: &Seen, now: i64) -> Vec<(u8, Room)> {
    numbers
        .iter()
        .map(|&n| {
            let account = row(usage, n);
            let mut room = account.map_or_else(|| unmeasured(now), |a| room_of(a, now));
            let resets = account
                .map(|a| Resets { five_hour: a.reset_5h.unwrap_or(0), week: a.reset_7d.unwrap_or(0), models: Vec::new() })
                .unwrap_or_default();
            if seen.shut(n, &resets, now)
                && let Room::Known { util_5h, .. } = &mut room
            {
                *util_5h = Some(1.0);
            }
            (n, room)
        })
        .collect()
}

/// Which window a refusal on account `n` was about.  kimi's error does
/// not say; the week when the feed shows it all but spent and still
/// running, else the five hours, which is the shorter wait.
pub(super) fn refused_window(usage: Option<&CcUsage>, n: u8, now: i64) -> Window {
    match row(usage, n) {
        Some(a) if a.util_7d.is_some_and(|u| u >= WEEK_NEARLY_SPENT) && a.reset_7d.is_some_and(|r| r > now) => Window::Week,
        _ => Window::FiveHour,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::claudecode::heads_up::nearly_out;
    use crate::plugins::claudecode::quota::{best_profile, next_move, Move, Refusal, Situation};

    const NOW: i64 = 1_794_290_114;

    fn feed(rows: &[(&str, f64, f64, i64, i64)]) -> CcUsage {
        let rows: Vec<String> = rows
            .iter()
            .map(|(name, five, week, reset_5h, reset_7d)| {
                format!(
                    r#"{{"name":"{name}","email":"","status":"allowed","utilization_5h":{five},"utilization_7d":{week},"reset_5h":{reset_5h},"reset_7d":{reset_7d},"model_limits":[],"credits":null,"tier":"","collected_at":"2026-11-10T05:50:00+00:00"}}"#
                )
            })
            .collect();
        let body = format!(r#"{{"generated_at":"2026-11-10T05:55:14+00:00","accounts":[{}]}}"#, rows.join(","));
        marspot::cc_usage::parse(&body).unwrap()
    }

    fn refused(at: i64, window: Window) -> Refusal {
        Refusal { at, model: String::new(), logged_out: false, window }
    }

    fn situation(rooms: &[(u8, Room)], current: u8) -> Situation<'_> {
        Situation {
            rooms,
            current,
            tool_executing: false,
            work_running: false,
            moves_this_episode: 0,
            since_last_move: None,
            stranded_for: None,
            warned: false,
            last_target: None,
            needs_model: None,
            background_refused: None,
        }
    }

    #[test]
    fn a_pane_goes_to_an_account_with_room_and_never_to_one_that_is_out() {
        let usage = feed(&[
            ("Kimi 1", 0.99, 0.5, NOW + 3_600, NOW + 2 * 86_400),
            ("Kimi 2", 0.2, 1.0, NOW + 3_600, NOW + 86_400),
            ("Kimi 3", 0.2, 0.6, NOW + 3_600, NOW + 3 * 86_400),
            ("Kimi 4", 0.1, 0.1, NOW + 3_600, NOW + 5 * 86_400),
        ]);
        let rooms = rooms(&[1, 2, 3, 4], Some(&usage), &Seen::default(), NOW);
        let out: Vec<bool> = rooms.iter().map(|(_, r)| r.refused_for(None)).collect();
        assert_eq!(out, [true, true, false, false], "five hours at 99% and a full week are both out");
        // the shared order: among accounts with room, the week that renews first
        assert_eq!(best_profile(&rooms, 1, None), Some(3));
        assert_eq!(next_move(&situation(&rooms, 1)), Move::To(3));
        assert_eq!(next_move(&situation(&rooms[..2], 1)), Move::Hold("every account is out"));
        assert_eq!(next_move(&situation(&rooms[..1], 1)), Move::Hold("nowhere else to go"), "one account: nothing moves");
    }

    #[test]
    fn an_account_without_a_row_has_room_and_comes_after_one_known_to() {
        let usage = feed(&[("Kimi 2", 0.2, 0.3, NOW + 3_600, NOW + 86_400)]);
        let rooms_with = rooms(&[1, 2, 3], Some(&usage), &Seen::default(), NOW);
        assert_eq!(best_profile(&rooms_with, 1, None), Some(2));
        let rooms_without = rooms(&[0, 2], None, &Seen::default(), NOW);
        assert!(rooms_without.iter().all(|(_, r)| !r.refused_for(None) && !nearly_out(r, None)));
        // with no feed at all, a refusal still sends the pane on
        let mut seen = Seen::default();
        assert!(seen.note(0, &refused(NOW - 5, Window::FiveHour)));
        let rooms_without = rooms(&[0, 2], None, &seen, NOW);
        assert!(rooms_without[0].1.refused_for(None));
        assert_eq!(best_profile(&rooms_without, 0, None), Some(2));
    }

    #[test]
    fn a_refusal_outlives_a_restart_and_ends_with_its_window() {
        let mut seen = Seen::default();
        assert!(seen.note(2, &refused(NOW - 600, Window::FiveHour)));
        assert!(!seen.note(2, &refused(NOW - 600, Window::FiveHour)), "the same failed turn is one refusal");
        let written = seen.changed().expect("something to write");
        assert_eq!(written, format!("out\t2\t{}\t5h\t\n", NOW - 600));
        assert_eq!(seen.changed(), None);

        let seen = Seen::decode(&written);
        // the feed read the account before the refusal and still shows room
        let usage = feed(&[("Kimi 2", 0.4, 0.3, NOW + 3 * 3_600, NOW + 86_400), ("Kimi 3", 0.5, 0.5, NOW + 3_600, NOW + 2 * 86_400)]);
        let marked = rooms(&[1, 2, 3], Some(&usage), &seen, NOW);
        assert!(marked[1].1.refused_for(None), "refused inside the window the feed says is still running");
        assert_eq!(best_profile(&marked, 1, None), Some(3), "not the refused account, though its week renews sooner");
        assert_eq!(best_profile(&rooms(&[1, 2, 3], Some(&usage), &Seen::default(), NOW), 1, None), Some(2));

        // the feed's reset time has passed: the window renewed
        let later = NOW + 3 * 3_600 + 1;
        assert!(!rooms(&[2], Some(&usage), &seen, later)[0].1.refused_for(None));
        // no row: the window's own length
        assert!(rooms(&[2], None, &seen, NOW + 4 * 3_600)[0].1.refused_for(None));
        assert!(!rooms(&[2], None, &seen, NOW - 600 + 5 * 3_600)[0].1.refused_for(None));
    }

    #[test]
    fn a_refusal_is_about_the_week_only_when_the_feed_shows_it_spent() {
        let usage = feed(&[("Kimi", 0.3, 0.995, NOW + 3_600, NOW + 86_400), ("Kimi 2", 0.3, 0.995, NOW + 3_600, NOW - 60)]);
        assert_eq!(refused_window(Some(&usage), 0, NOW), Window::Week);
        assert_eq!(refused_window(Some(&usage), 2, NOW), Window::FiveHour, "that week is over");
        assert_eq!(refused_window(Some(&usage), 3, NOW), Window::FiveHour);
        assert_eq!(refused_window(None, 0, NOW), Window::FiveHour);
        let mut seen = Seen::default();
        seen.note(0, &refused(NOW - 60, Window::Week));
        assert!(rooms(&[0], Some(&usage), &seen, NOW + 6 * 3_600)[0].1.refused_for(None), "shut until the week renews");
        assert!(!rooms(&[0], Some(&usage), &seen, NOW + 86_400 + 1)[0].1.refused_for(None));
    }

    #[test]
    fn a_busy_pane_is_told_when_its_account_is_one_step_from_out() {
        let usage = feed(&[("Kimi 1", 0.97, 0.5, NOW + 3_600, NOW + 86_400), ("Kimi 2", 0.5, 0.99, NOW + 3_600, NOW + 86_400), ("Kimi 3", 0.5, 0.5, NOW + 3_600, NOW + 86_400)]);
        let rooms = rooms(&[1, 2, 3], Some(&usage), &Seen::default(), NOW);
        let near: Vec<bool> = rooms.iter().map(|(_, r)| nearly_out(r, None)).collect();
        assert_eq!(near, [true, true, false]);
    }
}

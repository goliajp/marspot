//! The feed's rows, as the chooser reads them.
//!
//! A row is what the collector last managed to read, and for an
//! account no pane is using that can be days old: the collector reads
//! an account through the token its profile keeps fresh, and an idle
//! profile's token expires.  On 2026-11-05 two rows were two days
//! behind.  Whatever such a row says about a window whose renewal has
//! since passed is about a week that is over.

use marspot::cc_usage::CcAccount;

use super::quota::Room;

/// The windows renew at a fixed time each week.
const WEEK: i64 = 7 * 24 * 3600;

/// What the feed says about each candidate, in the caller's order,
/// as of `now` (unix seconds).
pub(super) fn rooms_for(
    profiles: &[u8],
    accounts: &[CcAccount],
    email_of: impl Fn(u8) -> Option<String>,
    now: i64,
) -> Vec<(u8, Room)> {
    profiles
        .iter()
        .map(|&p| {
            let room = email_of(p)
                .and_then(|mail| accounts.iter().find(|a| a.email == mail))
                .map(|a| room_of(a, now))
                .unwrap_or(Room::Unknown);
            (p, room)
        })
        .collect()
}

pub(in crate::plugins) fn room_of(a: &CcAccount, now: i64) -> Room {
    let renewed = |reset: i64| reset > 0 && reset <= now;
    // no reading stays none; a reading whose window has ended since
    // is a limit with nothing used
    let util_5h = a.util_5h.map(|u| if a.reset_5h.is_some_and(renewed) { 0.0 } else { u });
    let (util_7d, reset_7d) = if a.reset_7d <= 0 {
        // `null`: the week has not started, so a whole one is ahead.
        // Read as 0 it sorted first among accounts by renewal.
        (a.util_7d, now + WEEK)
    } else if renewed(a.reset_7d) {
        (0.0, next_renewal(a.reset_7d, now))
    } else {
        (a.util_7d, a.reset_7d)
    };
    let models: Vec<(String, f64)> = a
        .model_limits
        .iter()
        .map(|m| {
            let util = if m.reset.is_some_and(renewed) { 0.0 } else { m.util };
            (m.label.to_ascii_lowercase(), util)
        })
        .collect();
    let full_before = a.util_5h.is_some_and(|u| u >= 1.0) || a.util_7d >= 1.0 || a.model_limits.iter().any(|m| m.util >= 1.0);
    let full_now = util_5h.is_some_and(|u| u >= 1.0) || util_7d >= 1.0 || models.iter().any(|(_, u)| *u >= 1.0);
    // The collector writes `rejected` when any window is full, the 5h
    // included.  A full window that has renewed since, with nothing
    // else full, is that and nothing else.
    let status = if a.status == "rejected" && full_before && !full_now {
        "allowed".to_string()
    } else {
        a.status.clone()
    };
    Room::Known {
        status,
        util_5h,
        util_7d,
        reset_7d,
        models,
        // When the feed says the caps were carried, that is when they
        // were measured; otherwise this row's own collection time.
        models_measured_at: a.limits_as_of.unwrap_or(a.collected_at),
        measured_at: a.collected_at,
    }
}

/// Unix seconds, for `rooms_for`.
pub(in crate::plugins) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// The first renewal after `now`, counting whole weeks from one that
/// has passed.
fn next_renewal(passed: i64, now: i64) -> i64 {
    passed + ((now - passed) / WEEK + 1) * WEEK
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot::cc_usage::CcModelLimit;

    fn row(status: &str, u7: f64, reset7: i64, fable: (f64, Option<i64>)) -> CcAccount {
        CcAccount {
            name: "a".into(),
            email: "a".into(),
            status: status.into(),
            util_5h: None,
            util_7d: u7,
            reset_5h: None,
            reset_7d: reset7,
            model_limits: vec![CcModelLimit { label: "Fable".into(), util: fable.0, reset: fable.1 }],
            collected_at: 0,
            limits_as_of: None,
            tier: "default_claude_max_20x".into(),
        }
    }

    fn known(r: Room) -> (String, f64, i64, f64) {
        match r {
            Room::Known { status, util_7d, reset_7d, models, .. } => (status, util_7d, reset_7d, models[0].1),
            Room::Unknown => unreachable!(),
        }
    }

    /// P1 on 2026-11-05: spent, and nobody uses it, so the row is read
    /// no more.  Once its week renews it is somewhere to go, and it
    /// renews again a week later.
    #[test]
    fn a_week_that_has_renewed_since_the_row_was_read_is_empty() {
        let p1 = row("rejected", 1.0, 1_000, (0.74, Some(1_000)));
        assert_eq!(known(room_of(&p1, 999)), ("rejected".into(), 1.0, 1_000, 0.74), "not yet");
        let (status, u7, reset, fable) = known(room_of(&p1, 1_000 + 3_600));
        assert_eq!((status.as_str(), u7, fable), ("allowed", 0.0, 0.0));
        assert_eq!(reset, 1_000 + WEEK);
        assert_eq!(next_renewal(1_000, 1_000 + 2 * WEEK), 1_000 + 3 * WEEK, "two weeks unread");
    }

    /// A model cap renews on its own clock.
    #[test]
    fn a_model_cap_that_has_renewed_is_empty_while_the_week_is_not() {
        let p6 = row("rejected", 0.98, 9_000, (1.0, Some(1_000)));
        let (status, u7, _, fable) = known(room_of(&p6, 2_000));
        assert_eq!((status.as_str(), u7, fable), ("allowed", 0.98, 0.0));
    }

    /// `null` is a week not yet started: it renews last, not first.
    #[test]
    fn a_week_with_no_renewal_time_ranks_as_a_whole_week_ahead() {
        let fresh = row("allowed", 0.0, 0, (0.0, None));
        assert_eq!(known(room_of(&fresh, 5_000)).2, 5_000 + WEEK);
    }

    /// A rejection nothing full explains is kept.
    #[test]
    fn a_rejection_with_no_full_window_is_kept() {
        let odd = row("rejected", 0.5, 9_000, (0.3, Some(9_000)));
        assert_eq!(known(room_of(&odd, 2_000)).0, "rejected");
    }
}

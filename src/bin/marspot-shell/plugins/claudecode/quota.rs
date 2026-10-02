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

use std::time::Duration;

use marspot::cc_usage::CcAccount;

/// What is known about a profile when choosing.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Room {
    /// The feed answered for this profile.
    Known {
        status: String,
        util_5h: f64,
        util_7d: f64,
        reset_7d: i64,
        /// Per-model caps the feed reports, label lower-cased.
        ///
        /// Some projects can only run on Fable, and Fable has a
        /// quota of its own *on top of* the account's windows -- it
        /// is bounded by 5h and 7d as well, so it is an extra
        /// constraint rather than a separate pool. An account can be
        /// at 7% of its week and still be shut out of Fable, and for
        /// a pane pinned to that model the account-level numbers are
        /// not the whole answer.
        models: Vec<(String, f64)>,
        /// Unix seconds the per-model caps were actually measured.
        ///
        /// Not the same as when the row was collected: the feed
        /// carries `model_limits` over from an earlier sample and says
        /// so, and a carried number can be an hour and a half behind
        /// what the API is doing. This is what a refusal seen in a
        /// pane is compared against.
        models_measured_at: i64,
        /// Unix seconds the account-level windows were collected.
        measured_at: i64,
    },
    /// No account matched it — a profile the collector does not cover,
    /// or one whose `.claude.json` names an address the feed has never
    /// seen.  Not the same as "full".
    Unknown,
}

/// A window this full is over, whatever the status word says.
///
/// The feed carries two different kinds of answer.  `status` is the
/// unified rate-limit header, which only says `rejected` once a
/// request has actually been turned away and the collector happened to
/// see it; the utilisations are counted continuously.  Measured
/// 2026-09-29: the account behind P7 sat at 99% of its five-hour
/// window with the status still reading `allowed_warning`, while every
/// pane on it was being refused and had to be moved by hand.  Waiting
/// for the word is waiting for something that arrives late or not at
/// all.
const WINDOW_SPENT: f64 = 0.98;

/// The model with a cap of its own, as the feed labels it, lower case.
const FABLE: &str = "fable";

impl Room {
    pub(super) fn refused(&self) -> bool {
        self.refused_for(None)
    }

    /// Is this profile out, for a pane that needs `model`?
    ///
    /// The model's own cap is an additional way to be out, never a
    /// way to be in: an account whose week is spent is spent whatever
    /// Fable says, because Fable is bounded by that week too.
    pub(super) fn refused_for(&self, model: Option<&str>) -> bool {
        match self {
            Room::Known { status, util_5h, util_7d, .. } => {
                if status == "rejected" || *util_5h >= WINDOW_SPENT || *util_7d >= WINDOW_SPENT {
                    return true;
                }
                self.model_util(model).is_some_and(|u| u >= WINDOW_SPENT)
            }
            Room::Unknown => false,
        }
    }

    /// Is this profile out for a pane that needs `model`, counting
    /// what the pane has seen for itself?
    ///
    /// `refused_at` is when this pane was last told by the API that it
    /// had reached a limit. The feed and the pane are not two opinions
    /// to reconcile -- they are two witnesses to the same thing, and
    /// the later one is believed. That keeps the rule the feed was
    /// made the only trigger for: while it is current, it is still the
    /// only thing speaking.
    ///
    /// It has to be this way round because collection is behind
    /// consumption by construction, and can be behind by a lot: the
    /// feed carries a row's model caps over from an earlier sample
    /// when it cannot re-measure them, and one sat at 0.62 for 94
    /// minutes while the API refused that account sixteen times.
    pub(super) fn refused_for_now(&self, model: Option<&str>, refused_at: Option<i64>) -> bool {
        if self.refused_for(model) {
            return true;
        }
        let Some(seen) = refused_at else { return false };
        match self {
            // Compare against the numbers that would otherwise be
            // believed: the model's cap when the pane is pinned to
            // one, the account's windows when it is not.
            Room::Known { models_measured_at, measured_at, .. } => {
                let measured = if model.is_some() { *models_measured_at } else { *measured_at };
                seen > measured
            }
            // Nothing to be newer than. A pane that has been refused
            // where the feed cannot speak for it is out.
            Room::Unknown => true,
        }
    }

    /// How much of `model`'s own window is gone here, when the feed
    /// says and the pane cares.
    fn model_util(&self, model: Option<&str>) -> Option<f64> {
        let want = model?.to_ascii_lowercase();
        match self {
            Room::Known { models, .. } => models
                .iter()
                // The badge says `fable-5`, the feed says `Fable`.
                // Neither is a prefix of the other in general, so
                // match on whichever side is shorter.
                .find(|(label, _)| want.starts_with(label.as_str()) || label.starts_with(&want))
                .map(|(_, util)| *util),
            Room::Unknown => None,
        }
    }
    /// Seven days is the window that runs out here; five hours refills
    /// on its own within an afternoon.  Ties break on the shorter one.
    fn headroom(&self) -> (f64, f64) {
        self.headroom_for(None)
    }

    /// Room to work in, for a pane that needs `model`.
    ///
    /// The model's cap narrows the seven-day figure rather than
    /// adding a dimension: what a Fable-pinned pane can actually use
    /// is whichever of the two runs out first. Keeping it in the same
    /// pair means the ordering rule does not change -- the week
    /// first, the five hours to break ties -- which is the whole
    /// point of folding it in here instead of beside it.
    fn headroom_for(&self, model: Option<&str>) -> (f64, f64) {
        match self {
            Room::Known { util_5h, util_7d, .. } => {
                let week = 1.0 - util_7d;
                let capped = match self.model_util(model) {
                    Some(u) => week.min(1.0 - u),
                    None => week,
                };
                (capped, 1.0 - util_5h)
            }
            Room::Unknown => (0.0, 0.0),
        }
    }
    /// Is Fable's own cap spent on this account?
    fn fable_spent(&self) -> bool {
        self.model_util(Some(FABLE)).is_some_and(|u| u >= WINDOW_SPENT)
    }
    fn reset_7d(&self) -> i64 {
        match self {
            Room::Known { reset_7d, .. } => *reset_7d,
            Room::Unknown => i64::MAX,
        }
    }
}

/// Read a profile's account address out of its own config.
///
/// The feed is keyed by address, the pane is keyed by directory, and
/// this is the only thing on the machine that ties the two together.
pub(super) fn profile_email(home: &std::path::Path, profile: u8) -> Option<String> {
    let body = std::fs::read_to_string(home.join(format!(".claude-profile-{profile}/.claude.json")))
        .ok()?;
    let at = body.find("\"oauthAccount\"")?;
    // The file is pretty-printed, so the colon has a space after it —
    // matching `"emailAddress":"` found nothing on the real thing,
    // which is what the live test caught.
    let rest = &body[at..];
    let key = rest.find("\"emailAddress\"")? + "\"emailAddress\"".len();
    let open = key + rest[key..].find('"')? + 1;
    let close = open + rest[open..].find('"')?;
    Some(rest[open..close].to_string())
}

/// What the feed says about each candidate, in the caller's order.
pub(super) fn rooms_for(
    profiles: &[u8],
    accounts: &[CcAccount],
    email_of: impl Fn(u8) -> Option<String>,
) -> Vec<(u8, Room)> {
    profiles
        .iter()
        .map(|&p| {
            let room = email_of(p)
                .and_then(|mail| accounts.iter().find(|a| a.email == mail))
                .map(|a| Room::Known {
                    status: a.status.clone(),
                    util_5h: a.util_5h,
                    util_7d: a.util_7d,
                    reset_7d: a.reset_7d,
                    models: a
                        .model_limits
                        .iter()
                        .map(|m| (m.label.to_ascii_lowercase(), m.util))
                        .collect(),
                    // When the feed says the caps were carried, that
                    // is when they were measured; otherwise this row's
                    // own collection time.
                    models_measured_at: a.limits_as_of.unwrap_or(a.collected_at),
                    measured_at: a.collected_at,
                })
                .unwrap_or(Room::Unknown);
            (p, room)
        })
        .collect()
}

/// The profile to move a pane to, or `None` when there is nowhere
/// better than where it already is.
///
/// Order: an account with room, **the one nearest its seven-day
/// renewal first**.  What is left in a window that renews within the
/// hour is about to be replaced whether it is spent or not, while an
/// account that just renewed has to carry a whole week — so the cheap
/// quota goes first and the expensive quota is left alone.  Headroom
/// only breaks ties.
///
/// A pane that is not working in Fable goes to an account whose Fable
/// is already spent before anything else: that account's remaining
/// week is no use to the panes that can only run on Fable, while the
/// accounts with Fable left are the only place those panes can go.
/// A pane whose model has not been read yet might be one of them, so
/// it is not steered this way.
///
/// Then the ones the feed cannot speak for — not knowing is not the
/// same as being full.  Refused accounts last, soonest to come back
/// first, because when every account is refused the only question left
/// is how long the wait is.
///
/// Landing on an account that refuses immediately is possible and
/// bounded rather than prevented: a pane moves once per refusal and
/// not again for ten minutes, which is the cost of this order and the
/// reason no headroom floor is invented here.
pub(super) fn best_profile(rooms: &[(u8, Room)], current: u8, model: Option<&str>) -> Option<u8> {
    let mut ranked: Vec<&(u8, Room)> = rooms.iter().filter(|(p, _)| *p != current).collect();
    if ranked.is_empty() {
        return None;
    }
    // Unknown counts as Fable: steering it off the Fable accounts is
    // only right when it is known not to need them.
    let needs_fable = model.is_none_or(|m| m.to_ascii_lowercase().starts_with(FABLE));
    ranked.sort_by(|a, b| {
        let tier = |r: &Room| match r {
            Room::Known { .. } if !r.refused_for(model) => 0,
            Room::Unknown => 1,
            _ => 2,
        };
        // Only among the usable: the refused are ordered by when they
        // come back, and nothing else.
        let keeps_fable = |r: &Room| tier(r) == 0 && !needs_fable && r.fable_spent();
        tier(&a.1)
            .cmp(&tier(&b.1))
            .then_with(|| keeps_fable(&b.1).cmp(&keeps_fable(&a.1)))
            .then_with(|| {
                // Both ends of the list are ordered by the clock: the
                // refused by when they come back, the usable by when
                // their week renews.
                a.1.reset_7d()
                    .cmp(&b.1.reset_7d())
                    .then_with(|| {
                        b.1.headroom_for(model)
                            .partial_cmp(&a.1.headroom_for(model))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
            })
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked.first().map(|(p, _)| *p)
}


/// Everything the decision depends on.
///
/// Gathered by the caller, decided here: one place says what happens
/// to a refused pane, and it can be read as a table.
pub(super) struct Situation<'a> {
    /// What the feed says about every profile on the machine.
    pub rooms: &'a [(u8, Room)],
    /// The profile the pane is on — the one that just refused it.
    pub current: u8,
    /// A tool is executing on this machine right now.  Not "the agent
    /// is busy" — an agent stranded on an account that refuses it is
    /// not busy, it is stuck, and waiting for it to finish waits
    /// forever.  This is the narrower thing: a command of the user's
    /// is running, and killing the process would take it with it.
    pub tool_executing: bool,
    /// Moves already made while this pane has been continuously
    /// refused.
    pub moves_this_episode: u8,
    /// Since the last move, if there was one.
    pub since_last_move: Option<Duration>,
    /// How long this pane has been stuck on an account that is out.
    pub stranded_for: Option<Duration>,
    /// Where the last move was aimed, if there was one.  A pane is
    /// bound to a profile by the scan, which happens after the
    /// process it names exists — so between the move and the binding
    /// the pane still reads as being on the account it is leaving.
    pub last_target: Option<u8>,
    /// The model this pane is working in, when the feed caps it
    /// separately.
    ///
    /// Some projects can only run on Fable. Fable carries a quota of
    /// its own, and it is bounded by the account's 5h and 7d windows
    /// as well -- so a profile is usable for such a pane only when
    /// both the account and the model have room. For every other pane
    /// this is `None` and nothing about the decision changes.
    pub needs_model: Option<&'a str>,
}

/// What to do with a pane its account has refused.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Move {
    /// Move it to this profile.
    To(u8),
    /// Leave it where it is, for this reason — worth a log line and
    /// worth asserting on.
    Hold(&'static str),
}

/// Two moves per refusal: the second corrects the first.
pub(super) const MOVES_PER_EPISODE: u8 = 2;
/// And then it waits.
pub(super) const MOVE_COOLDOWN: Duration = Duration::from_secs(600);
/// How long "a tool is running here" is allowed to hold a pane.
///
/// The hold exists to protect a command of the user's from being
/// killed with the process.  It is not meant to cover a CLI that has
/// parked itself until its window reopens, which reads the same from
/// the transcript — and did, for nine hours and 170 log lines
/// (2026-09-29, sid 430).  Past this, the pane moves.
pub(super) const TOOL_HOLD_MAX: Duration = Duration::from_secs(600);

/// Where a refused pane goes next, or why it stays.
///
/// Mechanical on purpose: no clock of its own, no filesystem, no
/// side effect.  The caller gathers, this decides, the caller acts.
pub(super) fn next_move(s: &Situation) -> Move {
    // Moving kills the process, and a command running under it goes
    // with it.  That one waits; it will finish, and the next pass will
    // move the pane.
    if s.tool_executing
        && s.stranded_for.map(|d| d < TOOL_HOLD_MAX).unwrap_or(true)
    {
        return Move::Hold("a tool is running here");
    }
    // A move already made and not yet visible.  The pane is rebound by
    // the scan, and the scan can only bind it once the new process is
    // there — until then it still reads as sitting on the account it
    // is leaving, and moving again restarts a resume that is halfway
    // through.  The caller has to keep this tally across the gap where
    // the pane has no binding at all, which is exactly where it was
    // being dropped (pane 442, 2026-09-29: three cycles in 40 seconds).
    if let Some(t) = s.last_target
        && t != s.current {
            return Move::Hold("the last move has not landed");
        }
    if s.moves_this_episode >= MOVES_PER_EPISODE
        && s.since_last_move.map(|d| d < MOVE_COOLDOWN).unwrap_or(false)
    {
        return Move::Hold("cooling down");
    }
    let Some(target) = best_profile(s.rooms, s.current, s.needs_model) else {
        return Move::Hold("nowhere else to go");
    };
    // Every account is out.  That is a normal ending: the CLI is
    // already waiting for its own window to reopen, and moving would
    // trade one closed door for another.
    let known_shut = s
        .rooms
        .iter()
        .find(|(p, _)| *p == target)
        .map(|(_, r)| r.refused())
        .unwrap_or(false);
    if known_shut {
        return Move::Hold("every account is out");
    }
    Move::To(target)
}

#[cfg(test)]
mod tests {

    fn room(util_5h: f64, util_7d: f64, reset_7d: i64, models: &[(&str, f64)]) -> Room {
        Room::Known {
            status: "allowed".into(),
            util_5h,
            util_7d,
            reset_7d,
            models: models.iter().map(|(l, u)| (l.to_string(), *u)).collect(),
            // Measured now, in the tests that are not about staleness.
            models_measured_at: i64::MAX, measured_at: i64::MAX,
        }
    }

    /// A room whose numbers were measured at `measured`, with room to
    /// spare by every figure the feed reports.
    fn roomy_measured_at(measured: i64) -> Room {
        Room::Known {
            status: "allowed_warning".into(),
            util_5h: 0.85,
            util_7d: 0.88,
            reset_7d: 500,
            models: vec![("fable".to_string(), 0.62)],
            models_measured_at: measured,
            measured_at: measured,
        }
    }

    /// The case this exists for, with the real numbers: the feed says
    /// 0.62 of Fable is gone and the API says the account is out.
    ///
    /// The feed is not wrong, it is old -- it carried that row's model
    /// caps forward from a sample taken 94 minutes earlier, while
    /// fourteen panes it had just sent there spent what was left.
    #[test]
    fn a_refusal_newer_than_the_numbers_is_believed_over_them() {
        let feed_at = 1_790_850_134; // 10:22, when the caps were measured
        let refused_at = 1_790_855_512; // 11:51, when the API said no
        let r = roomy_measured_at(feed_at);
        assert!(
            !r.refused_for(Some("fable-5")),
            "by the feed alone there is room, which is why nothing moved"
        );
        assert!(
            r.refused_for_now(Some("fable-5"), Some(refused_at)),
            "but the pane was refused after those numbers were taken"
        );
    }

    /// The other half, and the reason this is not a second trigger
    /// that can disagree with the first: a refusal older than the
    /// numbers is already accounted for in them.
    ///
    /// Without the comparison -- taking any refusal as proof -- a pane
    /// would be moved off an account that has since been re-measured
    /// and has room, once per refusal still sitting in its transcript.
    #[test]
    fn a_refusal_older_than_the_numbers_is_not() {
        let refused_at = 1_790_850_000;
        let feed_at = 1_790_855_000;
        let r = roomy_measured_at(feed_at);
        assert!(
            !r.refused_for_now(Some("fable-5"), Some(refused_at)),
            "the feed has looked since, and it says there is room"
        );
        assert!(
            !r.refused_for_now(None, Some(refused_at)),
            "and the same for a pane with no model of its own"
        );
    }

    /// A profile the feed cannot speak for has nothing to be newer
    /// than. Being refused there is all the evidence there is.
    #[test]
    fn a_refusal_where_the_feed_is_silent_counts() {
        assert!(
            Room::Unknown.refused_for_now(Some("fable-5"), Some(1)),
            "unknown plus a closed door is a closed door"
        );
        assert!(
            !Room::Unknown.refused_for_now(Some("fable-5"), None),
            "unknown on its own is still not full"
        );
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
            (2, room(0.10, 0.07, 1_000, &[("fable", 0.99)])),
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
            Some(2),
            "nor is a pane whose model is not known: it might be on Fable"
        );
    }

    /// Spent Fable is a preference among accounts that can answer,
    /// not a way past a refusal.
    #[test]
    fn spent_fable_does_not_make_a_refused_account_a_destination() {
        let rooms = vec![
            (2, room(0.10, 0.99, 1_000, &[("fable", 1.00)])),
            (3, room(0.30, 0.40, 9_000, &[("fable", 0.20)])),
        ];
        assert_eq!(best_profile(&rooms, 1, Some("opus-5-5")), Some(3));
    }

    /// The model's cap is a way to be out, never a way to be in.
    ///
    /// Fable is bounded by the account's windows too, so a spent week
    /// is spent whatever the model row says.
    #[test]
    fn a_free_model_does_not_rescue_a_spent_account() {
        let spent_week = room(0.10, 0.99, 1_000, &[("fable", 0.00)]);
        assert!(spent_week.refused_for(Some("fable-5")), "the week is gone");
        assert!(spent_week.refused_for(None));
    }

    /// The badge writes `fable-5`; the feed writes `Fable`.
    #[test]
    fn the_badge_token_and_the_feed_label_find_each_other() {
        let r = room(0.1, 0.1, 1_000, &[("fable", 0.99)]);
        assert!(r.refused_for(Some("fable-5")), "badge token, feed label");
        assert!(r.refused_for(Some("fable")), "and the bare name");
        assert!(!r.refused_for(Some("opus-5-5")), "a model it does not meter");
        assert!(!r.refused_for(None));
    }

    /// A feed with no model rows at all changes nothing.
    #[test]
    fn an_older_feed_without_model_rows_decides_as_before() {
        let rooms = vec![(2, room(0.10, 0.07, 1_000, &[])), (3, room(0.30, 0.40, 9_000, &[]))];
        assert_eq!(best_profile(&rooms, 1, Some("fable-5")), Some(2));
        assert_eq!(best_profile(&rooms, 1, None), Some(2));
    }
    use super::*;








    fn rooms(spec: &[(u8, &str, f64, i64)]) -> Vec<(u8, Room)> {
        spec.iter()
            .map(|(p, status, util7, reset7)| {
                (
                    *p,
                    Room::Known {
                        status: (*status).into(),
                        util_5h: 0.0,
                        util_7d: *util7,
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
            util_5h: 0.99,
            util_7d: 0.37,
            reset_7d: 500, models: Vec::new(), models_measured_at: i64::MAX, measured_at: i64::MAX
        };
        assert!(warned.refused(), "99% of the five-hour window is not a warning");

        let fresh = Room::Known {
            status: "allowed_warning".into(),
            util_5h: 0.10,
            util_7d: 0.80,
            reset_7d: 900, models: Vec::new(), models_measured_at: i64::MAX, measured_at: i64::MAX
        };
        assert!(!fresh.refused(), "a warning with room left is still usable");

        // And it is not offered as somewhere to go.
        let r = vec![(1, Room::Known { status: "rejected".into(), util_5h: 0.0, util_7d: 1.0, reset_7d: 100 , models: Vec::new(), models_measured_at: i64::MAX, measured_at: i64::MAX}), (7, warned), (2, fresh)];
        assert_eq!(best_profile(&r, 1, None), Some(2));
    }

    fn situation<'a>(rooms: &'a [(u8, Room)]) -> Situation<'a> {
        Situation {
            needs_model: None,
            rooms,
            current: 1,
            tool_executing: false,
            moves_this_episode: 0,
            since_last_move: None,
            last_target: None,
            stranded_for: None,
        }
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

        // But that hold is not forever.  A CLI parked until its window
        // reopens reads exactly like a running tool, and held one pane
        // for nine hours before this bound existed.
        let s = Situation {
            tool_executing: true,
            stranded_for: Some(Duration::from_secs(601)),
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

    /// No feed, and the pane has just been refused where it is:
    /// somewhere unknown beats somewhere known shut.
    #[test]
    fn without_a_feed_it_still_leaves_a_closed_door() {
        let r = vec![(1, Room::Unknown), (2, Room::Unknown)];
        assert_eq!(next_move(&situation(&r)), Move::To(2));
    }

    fn acct(email: &str, status: &str, u5: f64, u7: f64, reset7: i64) -> CcAccount {
        CcAccount {
            name: email.into(),
            email: email.into(),
            status: status.into(),
            util_5h: u5,
            util_7d: u7,
            reset_5h: 0,
            reset_7d: reset7,
            model_limits: Vec::new(),
            collected_at: 0,
            limits_as_of: None,
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
        let rooms = rooms_for(&[1, 2, 3, 4], &accounts, |p| Some(format!("p{p}")));
        assert_eq!(best_profile(&rooms, 3, None), Some(4));
    }

    /// Same renewal, different room: then it is the fuller one.
    #[test]
    fn headroom_breaks_a_tie_on_the_clock() {
        let accounts = vec![
            acct("p1", "allowed", 0.0, 0.80, 500),
            acct("p2", "allowed", 0.0, 0.20, 500),
        ];
        let rooms = rooms_for(&[1, 2], &accounts, |p| Some(format!("p{p}")));
        assert_eq!(best_profile(&rooms, 0, None), Some(2));
    }

    /// A profile the feed has never heard of is a maybe, and a maybe
    /// beats a refusal.
    #[test]
    fn an_unknown_profile_outranks_a_refused_one() {
        let accounts = vec![acct("p1", "rejected", 0.0, 1.0, 400)];
        let rooms = rooms_for(&[1, 9], &accounts, |p| Some(format!("p{p}")));
        assert_eq!(best_profile(&rooms, 0, None), Some(9));
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
        let rooms = rooms_for(&[1, 2, 3], &accounts, |p| Some(format!("p{p}")));
        assert_eq!(best_profile(&rooms, 1, None), Some(2));
    }

    /// Nowhere else to go.
    #[test]
    fn a_single_profile_has_no_answer() {
        let rooms = rooms_for(&[1], &[], |_| None);
        assert_eq!(best_profile(&rooms, 1, None), None);
    }
}

#[cfg(test)]
mod live_feed_tests {
    use super::*;

    /// The feed and the profiles on this machine have to agree about
    /// who is who, and the only thing tying them together is the
    /// address each profile keeps in its own config.  Skips itself
    /// where there is no feed — a machine without the collector is a
    /// fine place to run tests.
    #[test]
    fn the_profiles_on_this_machine_resolve_to_accounts() {
        let Some(usage) = marspot::cc_usage::read() else { return };
        let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else { return };
        let profiles = super::super::discover_profiles();
        if profiles.is_empty() {
            return;
        }
        let rooms = rooms_for(&profiles, &usage.accounts, |p| profile_email(&home, p));
        let known = rooms.iter().filter(|(_, r)| *r != Room::Unknown).count();
        assert!(
            known > 0,
            "none of {profiles:?} matched an account in the feed — the address \
             in .claude.json and the feed's email no longer line up: {rooms:?}"
        );
    }
}

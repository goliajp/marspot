//! What is known about one account when choosing where a pane goes,
//! and what a pane was told by the API when it was refused.

use std::collections::HashMap;

/// What is known about a profile when choosing.
#[derive(Debug, Clone, PartialEq)]
pub enum Room {
    /// The feed answered for this profile.
    Known {
        status: String,
        /// The five-hour window's reading; `None` where the account has
        /// none, which is not a window with nothing used.
        util_5h: Option<f64>,
        /// The week's reading; `None` where the feed has none.  Such
        /// an account is not known to be full and not known to have
        /// room: nothing moves off it on that account, a refused pane
        /// goes to it only after every account known to have room, and
        /// no resting pane is moved to it.
        util_7d: Option<f64>,
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
        ///
        /// `None` where the feed reports the cap with no reading: not
        /// full and not known to have room, as for the week.
        models: Vec<(String, Option<f64>)>,
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

/// A five-hour window this full is over, whatever the status word says.
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
pub const FIVE_HOUR_SPENT: f64 = 0.98;

/// The week and the per-model caps are over only when the API says
/// they are full.
///
/// They used to share the five-hour margin, and the last two percent
/// of four accounts' weeks went unused: on 2026-11-05 P2, P3, P4 and
/// P6 all sat at 98% and none of them counted as somewhere to go,
/// while the transcripts of the two days before held not one weekly
/// refusal and P2 read `allowed` at 98% a minute after a pane had been
/// moved off it.  A pane that does run out first is caught by its own
/// refusal (`refused_for_now`), which did not exist when the margin
/// was chosen.
const WINDOW_SPENT: f64 = 1.0;

/// The model with a cap of its own, as the feed labels it, lower case.
const FABLE: &str = "fable";

impl Room {
    /// Is this profile out, for a pane that needs `model`?
    ///
    /// The model's own cap is an additional way to be out, never a
    /// way to be in: an account whose week is spent is spent whatever
    /// Fable says, because Fable is bounded by that week too.
    pub fn refused_for(&self, model: Option<&str>) -> bool {
        match self {
            Room::Known { status, util_5h, util_7d, .. } => {
                // The collector could not log in as this account. Its
                // numbers are whatever was last read, and a claude
                // started there cannot authenticate (P1 on 2026-10-03:
                // 78% of the week, logged out).
                if status == "auth_error" {
                    return true;
                }
                if util_5h.is_some_and(|u| u >= FIVE_HOUR_SPENT) || util_7d.is_some_and(|u| u >= WINDOW_SPENT) {
                    return true;
                }
                if status == "rejected" && !self.rejected_only_for_another_model(model) {
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
    /// `refusal` is when this pane was last told by the API that it
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
    pub fn refused_for_now(&self, model: Option<&str>, refusal: Option<&Refusal>) -> bool {
        if self.refused_for(model) {
            return true;
        }
        let Some(seen) = refusal.filter(|r| r.applies_to(model)).map(|r| r.at) else {
            return false;
        };
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

    /// Is the account's `rejected` explained by a model this pane does
    /// not use?
    ///
    /// The collector writes `rejected` when ANY of an account's windows
    /// is full, its per-model ones included. Measured 2026-10-03: P1 at
    /// 0% of five hours and 77% of the week read `rejected` because its
    /// Fable cap was spent, and an Opus pane put there by hand was moved
    /// off it six seconds later. The account windows are already
    /// checked on their own numbers, so the word only adds something
    /// when no capped model accounts for it, or when the pane is on
    /// that model. No model given means none is assumed, Fable included.
    fn rejected_only_for_another_model(&self, model: Option<&str>) -> bool {
        match self {
            Room::Known { models, .. } => {
                let mut spent = models.iter().filter(|(_, u)| u.is_some_and(|u| u >= WINDOW_SPENT)).peekable();
                spent.peek().is_some()
                    && spent.all(|(label, _)| {
                        model.is_none_or(|m| !same_model(&m.to_ascii_lowercase(), label))
                    })
            }
            Room::Unknown => false,
        }
    }

    /// How much of `model`'s own window is gone here, when the feed
    /// says and the pane cares.
    fn model_util(&self, model: Option<&str>) -> Option<f64> {
        let want = model?.to_ascii_lowercase();
        match self {
            Room::Known { models, .. } => models
                .iter()
                .find(|(label, _)| same_model(&want, label))
                .and_then(|(_, util)| *util),
            Room::Unknown => None,
        }
    }
    /// Room to work in, for a pane that needs `model`.
    ///
    /// The model's cap narrows the seven-day figure rather than
    /// adding a dimension: what a Fable-pinned pane can actually use
    /// is whichever of the two runs out first. Keeping it in the same
    /// pair means the ordering rule does not change -- the week
    /// first, the five hours to break ties -- which is the whole
    /// point of folding it in here instead of beside it.
    ///
    /// The first figure is `None` where no reading bounds it: a week
    /// the feed does not give, for a pane no model cap narrows.
    pub fn headroom_for(&self, model: Option<&str>) -> (Option<f64>, f64) {
        match self {
            Room::Known { util_5h, util_7d, .. } => {
                let week = util_7d.map(|u| 1.0 - u);
                let capped = match self.model_util(model) {
                    Some(u) => Some(week.map_or(1.0 - u, |w| w.min(1.0 - u))),
                    None => week,
                };
                // no five-hour limit leaves all of it
                (capped, util_5h.map_or(1.0, |u| 1.0 - u))
            }
            Room::Unknown => (None, 0.0),
        }
    }
    /// Does the feed give a reading for this account's week?
    pub fn week_known(&self) -> bool {
        matches!(self, Room::Known { util_7d: Some(_), .. })
    }
    /// Does the feed give a reading of everything that bounds a pane on
    /// `model` here: the week, and the model's own cap where it reports
    /// one?
    pub fn known_for(&self, model: Option<&str>) -> bool {
        let unread_cap = match (self, model) {
            (Room::Known { models, .. }, Some(m)) => {
                let want = m.to_ascii_lowercase();
                models.iter().any(|(label, u)| u.is_none() && same_model(&want, label))
            }
            _ => false,
        };
        self.week_known() && !unread_cap
    }
    /// Is Fable's own cap spent on this account?
    pub fn fable_spent(&self) -> bool {
        self.model_util(Some(FABLE)).is_some_and(|u| u >= WINDOW_SPENT)
    }
    pub fn reset_7d(&self) -> i64 {
        match self {
            Room::Known { reset_7d, .. } => *reset_7d,
            Room::Unknown => i64::MAX,
        }
    }
}

/// Is the pane working in Fable?  No model given means no.
pub fn needs_fable(model: Option<&str>) -> bool {
    model.is_some_and(|m| m.to_ascii_lowercase().starts_with(FABLE))
}

/// Does the pane's model (lower case) name the feed's capped model?
///
/// The badge says `fable-5`, the feed says `fable`. Neither is a prefix
/// of the other in general, so match on whichever side is shorter.
pub fn same_model(want: &str, label: &str) -> bool {
    want.starts_with(label) || label.starts_with(want)
}

/// Treat as logged out every profile a pane has found it cannot log in
/// to since the feed last read it.
///
/// `seen` is, per profile, the newest login failure any pane met
/// there. The feed keeps reporting such an account by its old numbers
/// -- it cannot tell it from an idle one -- so without this every
/// refused pane would be sent back to it. A newer reading means
/// someone has logged in again, and the feed is believed once more.
pub fn mark_logged_out(rooms: &mut [(u8, Room)], seen: &HashMap<u8, i64>) {
    for (p, room) in rooms.iter_mut() {
        if let Room::Known { status, measured_at, .. } = room
            && seen.get(p).is_some_and(|at| at > measured_at)
        {
            *status = "auth_error".into();
        }
    }
}

/// A pane was told by the API that it had run out.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    /// Unix seconds, from the record's own timestamp.
    pub at: i64,
    /// The model the refusal names, as `short_model` spells it, or
    /// empty when it could not be read.
    ///
    /// Every refusal the CLI writes names one: "You've reached your
    /// Fable limit." or "Opus 5.5 requires usage credits.". It says
    /// nothing about any other model on the same account: on
    /// 2026-10-03 a Fable refusal moved a pane that had just been
    /// switched to Opus, onto an account where it ran out again.
    pub model: String,
    /// The account could not log in at all, rather than being out of
    /// quota.
    ///
    /// The feed cannot tell a logged-out account from an idle one:
    /// both show as an access token that has expired and was never
    /// refreshed. Only a claude that tried to use it finds out, and
    /// that is the only place this is learnt. It names no model.
    pub logged_out: bool,
    /// Which of the account's windows ran out.
    pub window: Window,
}

/// The window a refusal is about, which is how long it stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    /// "session" and "5-hour": over within five hours.
    FiveHour,
    /// The week, a model's cap, or credits.
    Week,
}

impl Refusal {
    /// Does this refusal shut out a pane working in `model`?
    ///
    /// One that names no model covers every pane. One that names a
    /// model covers only panes on it: with no model given, nothing is
    /// assumed, Fable included.
    pub fn applies_to(&self, model: Option<&str>) -> bool {
        if self.model.is_empty() {
            return true;
        }
        model.is_some_and(|m| same_model(&m.to_ascii_lowercase(), &self.model))
    }
}


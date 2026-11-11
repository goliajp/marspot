//! What panes have learnt about their accounts, kept past the panes
//! and past a restart of the shell.
//!
//! A refusal is a fact about the account, not about the pane that met
//! it: every pane there on the same model is shut out at the same
//! moment. Kept per pane, it moved only the panes that happened to
//! send something; on 2026-11-07 P1 refused its first pane at 17:39
//! while the feed still read it as short of full, and ten panes left
//! one by one over the next three and a half minutes, each after a
//! refusal of its own -- an idle one only once someone spoke to it.
//! The panes that saw it move off, so it has to outlive them; the
//! next pane to be moved must not be sent back there either.

use std::collections::HashMap;

use marspot::cc_usage::CcAccount;

use super::quota::{same_model, Refusal, Room, Window};
use super::refusal::mark_logged_out;

const FIVE_HOURS: i64 = 5 * 3600;
const WEEK: i64 = 7 * 24 * 3600;

/// When an account's windows end, as the feed last gave them. Zero
/// where it gave none.
#[derive(Debug, Clone, Default, PartialEq)]
pub(in crate::plugins) struct Resets {
    pub five_hour: i64,
    pub week: i64,
    /// Per capped model, label lower-cased.
    pub models: Vec<(String, i64)>,
}

/// The resets of each profile's account; a profile the feed does not
/// cover has none.
pub(super) fn resets_for(
    profiles: &[u8],
    accounts: &[CcAccount],
    email_of: impl Fn(u8) -> Option<String>,
) -> Vec<(u8, Resets)> {
    profiles
        .iter()
        .map(|&p| {
            let r = email_of(p)
                .and_then(|mail| accounts.iter().find(|a| a.email == mail))
                .map(|a| Resets {
                    five_hour: a.reset_5h,
                    week: a.reset_7d,
                    models: a
                        .model_limits
                        .iter()
                        .filter_map(|m| Some((m.label.to_ascii_lowercase(), m.reset?)))
                        .collect(),
                })
                .unwrap_or_default();
            (p, r)
        })
        .collect()
}

#[derive(Debug, Default)]
pub(in crate::plugins) struct Seen {
    /// Per profile, the newest login failure a pane met there.
    pub logged_out: HashMap<u8, i64>,
    /// Per profile and the model a refusal named (empty: the account's
    /// own windows), the newest refusal a pane met there and the window
    /// it was about. Bounded by profiles times models.
    pub out: HashMap<(u8, String), (i64, Window)>,
    dirty: bool,
}

impl Seen {
    /// Take in what one pane on `profile` was told. True when it is
    /// newer than anything already known for that account and model.
    pub(in crate::plugins) fn note(&mut self, profile: u8, r: &Refusal) -> bool {
        let news = if r.logged_out {
            let at = self.logged_out.entry(profile).or_insert(i64::MIN);
            let news = r.at > *at;
            *at = (*at).max(r.at);
            news
        } else {
            let e = self.out.entry((profile, r.model.clone())).or_insert((i64::MIN, r.window));
            let news = r.at > e.0;
            if news {
                *e = (r.at, r.window);
            }
            news
        };
        self.dirty |= news;
        news
    }

    /// Mark every account a refusal still stands for.
    ///
    /// A refusal stands until the window it was about renews. A
    /// measurement taken after it that reads short of full does not
    /// lift it: the measurement is a sample and runs behind, the
    /// refusal happened, and believing the sample sent panes back to be
    /// refused again, each time breaking off what they were doing. The
    /// last sliver of such an account waits for the renewal.
    ///
    /// Where the feed gives no end for the window, the refusal stands
    /// as it used to: until the account is measured again.
    ///
    /// The window the refusal names is set full: the account's when it
    /// names no model, that model's cap when it does, so a Fable
    /// refusal shuts out Fable panes only. A failed login is different
    /// in kind -- logging in again is all it takes -- and a newer
    /// measurement is still believed there.
    pub(super) fn mark(&self, rooms: &mut [(u8, Room)], resets: &[(u8, Resets)], now: i64) {
        mark_logged_out(rooms, &self.logged_out);
        for ((p, model), (at, window)) in &self.out {
            let Some((_, Room::Known { util_5h, models, models_measured_at, measured_at, .. })) =
                rooms.iter_mut().find(|(n, _)| n == p)
            else {
                continue;
            };
            let none = Resets::default();
            let r = resets.iter().find(|(n, _)| n == p).map_or(&none, |(_, r)| r);
            let standing = stands(*at, *window, model, r, now);
            if model.is_empty() {
                // which account window it was does not matter to a
                // pane: either shuts the account
                if standing || at > measured_at {
                    *util_5h = util_5h.max(1.0);
                }
            } else if standing || at > models_measured_at {
                match models.iter_mut().find(|(label, _)| same_model(model, label)) {
                    Some((_, u)) => *u = u.max(1.0),
                    None => models.push((model.clone(), 1.0)),
                }
            }
        }
    }

    /// Does a refusal of `profile`'s own windows still stand?  For a
    /// caller whose accounts are measured seldom or never: a five-hour
    /// end the feed gave, since passed, ends the refusal it followed.
    pub(in crate::plugins) fn shut(&self, profile: u8, resets: &Resets, now: i64) -> bool {
        self.out.get(&(profile, String::new())).is_some_and(|(at, window)| {
            let renewed = *window == Window::FiveHour && resets.five_hour > *at && resets.five_hour <= now;
            !renewed && stands(*at, *window, "", resets, now)
        })
    }

    /// One line per fact, for the file that carries this across a
    /// restart; None when nothing changed since the last call.
    pub(in crate::plugins) fn changed(&mut self) -> Option<String> {
        if !std::mem::take(&mut self.dirty) {
            return None;
        }
        let mut lines: Vec<String> = self
            .logged_out
            .iter()
            .map(|(p, at)| format!("login\t{p}\t{at}\n"))
            .chain(self.out.iter().map(|((p, model), (at, w))| {
                let w = if *w == Window::FiveHour { "5h" } else { "week" };
                format!("out\t{p}\t{at}\t{w}\t{model}\n")
            }))
            .collect();
        lines.sort();
        Some(lines.concat())
    }

    /// What `changed` wrote. A line that does not read is dropped.
    pub(in crate::plugins) fn decode(text: &str) -> Self {
        let mut seen = Self::default();
        for line in text.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            let (Some(p), Some(at)) = (
                f.get(1).and_then(|s| s.parse::<u8>().ok()),
                f.get(2).and_then(|s| s.parse::<i64>().ok()),
            ) else {
                continue;
            };
            match (f[0], f.get(3), f.get(4)) {
                ("login", _, _) => {
                    seen.logged_out.insert(p, at);
                }
                ("out", Some(w), Some(model)) => {
                    let w = if *w == "5h" { Window::FiveHour } else { Window::Week };
                    seen.out.insert((p, model.to_string()), (at, w));
                }
                _ => {}
            }
        }
        seen
    }
}

/// Is the window a refusal at `at` was about still the current one?
fn stands(at: i64, window: Window, model: &str, r: &Resets, now: i64) -> bool {
    // inside the window that ends at `reset`, which has not ended
    let within = |reset: i64, len: i64| reset > now && at >= reset - len;
    match window {
        // five hours at most, whatever the feed knows
        Window::FiveHour if r.five_hour > now => at >= r.five_hour - FIVE_HOURS,
        Window::FiveHour => now < at + FIVE_HOURS,
        Window::Week => {
            let named = r.models.iter().find(|(label, _)| !model.is_empty() && same_model(model, label));
            match named {
                Some((_, reset)) if *reset > now => at >= reset - WEEK,
                _ => within(r.week, WEEK),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_794_300_000;

    fn known(measured_at: i64) -> Room {
        Room::Known {
            status: "allowed".into(),
            util_5h: 0.1,
            util_7d: 0.97,
            reset_7d: NOW + 3 * 86400,
            models: vec![("fable".into(), 0.5)],
            models_measured_at: measured_at,
            measured_at,
        }
    }

    fn refusal(at: i64, model: &str, window: Window) -> Refusal {
        Refusal { at, model: model.into(), logged_out: false, window }
    }

    fn out(rooms: &[(u8, Room)], model: Option<&str>) -> bool {
        rooms[0].1.refused_for(model)
    }

    /// The case this exists for: refused on the week, then measured at
    /// 97%. The account stays shut until the week renews.
    #[test]
    fn a_weekly_refusal_outlasts_a_later_measurement_and_ends_with_the_week() {
        let mut seen = Seen::default();
        seen.note(1, &refusal(NOW - 3600, "", Window::Week));
        let resets = vec![(1, Resets { week: NOW + 3 * 86400, ..Default::default() })];

        let mut rooms = vec![(1, known(NOW - 60))];
        seen.mark(&mut rooms, &resets, NOW);
        assert!(out(&rooms, Some("opus-5-5")), "measured after the refusal, still shut");

        // the week has renewed: the feed now names the next one
        let renewed = vec![(1, Resets { week: NOW + 10 * 86400, ..Default::default() })];
        let mut rooms = vec![(1, known(NOW - 60))];
        seen.mark(&mut rooms, &renewed, NOW + 4 * 86400);
        assert!(!out(&rooms, Some("opus-5-5")), "a refusal from the week before");
    }

    #[test]
    fn a_five_hour_refusal_ends_within_five_hours() {
        let mut seen = Seen::default();
        seen.note(1, &refusal(NOW, "", Window::FiveHour));
        let unknown = vec![(1, Resets::default())];

        let mut rooms = vec![(1, known(NOW + 60))];
        seen.mark(&mut rooms, &unknown, NOW + FIVE_HOURS - 1);
        assert!(out(&rooms, None));
        let mut rooms = vec![(1, known(NOW + 60))];
        seen.mark(&mut rooms, &unknown, NOW + FIVE_HOURS);
        assert!(!out(&rooms, None), "no window is longer than five hours");

        // the feed names the window after the one that refused
        let next = vec![(1, Resets { five_hour: NOW + 6 * 3600, ..Default::default() })];
        let mut rooms = vec![(1, known(NOW + 60))];
        seen.mark(&mut rooms, &next, NOW + 2 * 3600);
        assert!(!out(&rooms, None));
    }

    /// A model's cap ends on its own date, and shuts out that model only.
    #[test]
    fn a_model_refusal_follows_the_models_own_reset() {
        let mut seen = Seen::default();
        seen.note(1, &refusal(NOW - 3600, "fable", Window::Week));
        let resets = vec![(
            1,
            Resets { week: NOW + 86400, models: vec![("fable".into(), NOW + 5 * 86400)], ..Default::default() },
        )];
        let mut rooms = vec![(1, known(NOW - 60))];
        seen.mark(&mut rooms, &resets, NOW + 2 * 86400);
        assert!(out(&rooms, Some("fable")), "the account's week is over, Fable's is not");
        assert!(!out(&rooms, Some("opus-5-5")));
    }

    /// With no end known for the window, the old rule: until measured again.
    #[test]
    fn without_a_reset_a_refusal_stands_until_the_next_measurement() {
        let mut seen = Seen::default();
        seen.note(1, &refusal(NOW - 3600, "", Window::Week));
        let none = vec![(1, Resets::default())];
        let mut rooms = vec![(1, known(NOW - 7200))];
        seen.mark(&mut rooms, &none, NOW);
        assert!(out(&rooms, None));
        let mut rooms = vec![(1, known(NOW - 60))];
        seen.mark(&mut rooms, &none, NOW);
        assert!(!out(&rooms, None));
    }

    #[test]
    fn what_was_seen_survives_being_written_and_read() {
        let mut seen = Seen::default();
        assert!(seen.changed().is_none());
        seen.note(1, &refusal(10, "", Window::FiveHour));
        seen.note(2, &refusal(20, "fable", Window::Week));
        seen.note(3, &Refusal { at: 30, model: String::new(), logged_out: true, window: Window::Week });
        let text = seen.changed().expect("three facts are news");
        assert!(seen.changed().is_none(), "written once");
        let back = Seen::decode(&format!("{text}garbage\nout\tx\ty\n"));
        assert_eq!(back.out, seen.out);
        assert_eq!(back.logged_out, seen.logged_out);
    }
}

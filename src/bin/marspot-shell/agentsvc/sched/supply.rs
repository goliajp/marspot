//! An account's stored resets, as last read, turned into the supply
//! with a deadline the allocation counts on.  A claim the planner plans
//! is supply from its claim time on; using it still waits for the
//! user's confirmation on the accounts panel while `sched.reset` is off.

use marspot::agent_resets::{AccountResets, Grant, brings_back};
use marspot_agent::sched::reset::{Supply, WeekCut};

fn hours(at: i64, now: i64) -> f64 {
    (at - now) as f64 / 3600.0
}

/// What one grant brings back; none for one with nothing left or one
/// that brings back neither the week nor the restricted window.
fn of_grant(g: &Grant, usable_below_left: Option<f64>, now: i64) -> Option<Supply> {
    let left = g.left.unwrap_or(1);
    let b = brings_back(g);
    let (week, window, gives_back_week) = (b.week, b.window, b.gives_back_week);
    let week_cut = b.week_share.map_or(WeekCut::Nothing, WeekCut::ShareOfUsed);
    if left == 0 || !(week || window) {
        return None;
    }
    Some(Supply {
        week,
        window,
        gives_back_week,
        week_cut,
        moves_week: g.moves_weekly_reset,
        // not said is read as needing the limit, the stricter of the two
        requires_limit: g.use_requires_limit.unwrap_or(true),
        starts_h: g.starts_at.map_or(0.0, |s| hours(s, now).max(0.0)),
        ends_h: g.ends_at.map(|e| hours(e, now)),
        left,
        usable_below_left,
    })
}

/// The supply of account `profile` of `agent` in `all`, as of `now`;
/// `usable_below_left` is the agent's rule for when a reset may be used,
/// as [`Supply::usable_below_left`].
pub(crate) fn of_account(all: &[AccountResets], agent: &str, profile: u8, usable_below_left: Option<f64>, now: i64) -> Vec<Supply> {
    let Some(a) = all.iter().find(|a| a.agent == agent && a.profile == profile) else { return Vec::new() };
    a.resets.grants.iter().filter(|g| g.ends_at.is_none_or(|e| e > now)).filter_map(|g| of_grant(g, usable_below_left, now)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot::agent_resets::{CLAUDE_FABLE, CLAUDE_FULL, Resets};

    const NOW: i64 = 1_800_000_000;

    fn account(grants: Vec<Grant>) -> Vec<AccountResets> {
        vec![AccountResets { agent: "a".into(), profile: 1, name: "A 1".into(), resets: Resets { grants, ..Resets::none(NOW, "app_server") } }]
    }

    #[test]
    fn grants_become_supply_by_what_they_bring_back() {
        let week = Grant {
            kind: "whole".into(),
            restores: vec!["seven_day".into()],
            moves_weekly_reset: true,
            left: Some(1),
            use_requires_limit: Some(false),
            starts_at: Some(NOW - 60),
            ends_at: Some(NOW + 7_200),
            ..Grant::default()
        };
        let fable = Grant { kind: "window".into(), restores: vec!["seven_day_overage_included".into()], gives_back_weekly: true, left: None, ends_at: None, starts_at: Some(NOW + 3_600), ..Grant::default() };
        let session = Grant { kind: "session".into(), restores: vec!["five_hour".into()], left: Some(2), ..Grant::default() };
        let used = Grant { left: Some(0), ..week.clone() };
        let ended = Grant { ends_at: Some(NOW), ..week.clone() };
        let all = account(vec![week, fable, session, used, ended]);
        let got = of_account(&all, "a", 1, None, NOW);
        assert_eq!(got.len(), 2, "the session reset brings back no week; the used and ended ones nothing");
        assert_eq!(got[0], Supply { week: true, window: false, gives_back_week: false, week_cut: WeekCut::Nothing, moves_week: true, requires_limit: false, starts_h: 0.0, ends_h: Some(2.0), left: 1, usable_below_left: None });
        assert_eq!(got[1], Supply { week: false, window: true, gives_back_week: true, week_cut: WeekCut::Nothing, moves_week: false, requires_limit: true, starts_h: 1.0, ends_h: None, left: 1, usable_below_left: None });
        assert!(of_account(&all, "a", 2, None, NOW).is_empty());
        assert!(of_account(&all, "b", 1, None, NOW).is_empty());
    }

    #[test]
    fn claudes_two_stored_resets_bring_back_by_their_kind() {
        let full = Grant { kind: CLAUDE_FULL.into(), left: Some(1), ends_at: Some(NOW + 7_200), ..Grant::default() };
        let fable = Grant { kind: CLAUDE_FABLE.into(), restores: vec!["seven_day".into()], gives_back_weekly: true, left: Some(2), ..Grant::default() };
        let got = of_account(&account(vec![full, fable]), "a", 1, None, NOW);
        assert_eq!((got[0].week, got[0].window, got[0].gives_back_week, got[0].week_cut), (true, true, false, WeekCut::Nothing));
        assert_eq!(got[0].ends_h, Some(2.0));
        assert_eq!((got[1].week, got[1].window, got[1].gives_back_week, got[1].week_cut), (false, true, false, WeekCut::ShareOfUsed(marspot::agent_resets::FABLE_WEEK_SHARE)), "the kind wins over what it says");
        assert_eq!(got[1].left, 2);
        assert_eq!(WeekCut::ShareOfUsed(marspot::agent_resets::FABLE_WEEK_SHARE).of(0.6), 0.3);
    }

    #[test]
    fn the_agents_rule_for_using_a_reset_is_carried_into_its_supply() {
        let all = account(vec![Grant { kind: "whole".into(), restores: vec!["seven_day".into()], left: Some(1), ..Grant::default() }]);
        assert_eq!(of_account(&all, "a", 1, Some(0.8), NOW)[0].usable_below_left, Some(0.8));
    }
}

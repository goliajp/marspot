//! Using a stored reset from the accounts panel: each line of an
//! account's resets is a button for the grant of that kind that ends
//! first.  The first press arms it and the line says what comes back
//! and when it would come back by itself; a second press on the same
//! line within [`ARMED_FOR_S`] confirms.  Presses come only from the
//! panel's own pointer handling, so nothing a pane or another program
//! sends can confirm.

use super::{Grant, Resets};

/// How long an armed line waits for its second press.
pub const ARMED_FOR_S: i64 = 15;

/// One line of an account card's resets, as drawn.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResetLine {
    pub text: String,
    /// It is a button: a grant of a provider marspot can claim for.
    pub pressable: bool,
    /// Armed by a first press, waiting for the second.
    pub armed: bool,
}

/// Whether marspot can claim a grant of `kind` on an `agent` account:
/// every Codex credit; of Claude's, only the weekly session reset,
/// typed as `/limit-reset`.  A stored Claude grant (`claude_full`,
/// `claude_fable`) would be chosen from a list the CLI has never been
/// seen to show, so none is claimed.
pub fn claim_path_on(agent: &str, kind: &str) -> bool {
    match agent {
        "codex" => true,
        "claude" => kind == "session",
        _ => false,
    }
}

/// The grant line `line` of `r` claims, with its index among the
/// grants: of that line's kind, one with some left that may be used
/// now, the one ending first.
pub fn claimable(r: &Resets, line: usize) -> Option<(usize, &Grant)> {
    let mut kinds: Vec<&str> = Vec::new();
    for g in &r.grants {
        if !kinds.contains(&g.kind.as_str()) {
            kinds.push(&g.kind);
        }
    }
    let kind = *kinds.get(line)?;
    r.grants
        .iter()
        .enumerate()
        .filter(|(_, g)| g.kind == kind && g.left.is_none_or(|n| n > 0) && g.usable_now != Some(false))
        .min_by_key(|(_, g)| g.ends_at.unwrap_or(i64::MAX))
}

/// Which line of which account is armed, and since when.
#[derive(Clone, Debug, PartialEq)]
pub struct Armed {
    pub agent: String,
    pub profile: u8,
    pub line: usize,
    pub at: i64,
}

/// The panel's two-press state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResetButtons {
    armed: Option<Armed>,
}

impl ResetButtons {
    /// A press on line `line` of the account `(agent, profile)` at `now`:
    /// the armed line when this press confirms it, else `None` (this
    /// press armed the line, or moved the arming to it).
    pub fn press(&mut self, agent: &str, profile: u8, line: usize, now: i64) -> Option<Armed> {
        let same = self.armed_at(now).is_some_and(|a| a.agent == agent && a.profile == profile && a.line == line);
        if same {
            return self.armed.take();
        }
        self.armed = Some(Armed { agent: agent.into(), profile, line, at: now });
        None
    }

    /// A press anywhere else on the panel lets the armed line go.
    pub fn disarm(&mut self) {
        self.armed = None;
    }

    /// The armed line, unless it waited too long.
    pub fn armed_at(&self, now: i64) -> Option<&Armed> {
        self.armed.as_ref().filter(|a| now - a.at <= ARMED_FOR_S)
    }
}

/// A window's name in the line: `5h`, `7d`, `fable`.
fn short(window: &str) -> &str {
    match window {
        "five_hour" => "5h",
        "seven_day" => "7d",
        "seven_day_overage_included" => "fable",
        other => other,
    }
}

/// The armed line: what comes back now and when it would by itself,
/// `renews` being the soonest renewal of what it brings back.
pub fn armed_text(g: &Grant, renews: Option<i64>, when: impl Fn(i64) -> String) -> String {
    let back: Vec<&str> = g.restores.iter().map(|w| short(w)).collect();
    let back = if back.is_empty() { "limits".to_string() } else { back.join("+") };
    match renews {
        Some(t) => format!("press again: {back} back now, else at {}", when(t)),
        None => format!("press again: {back} back now"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_resets::{CLAUDE_FABLE, CLAUDE_FULL};

    fn grant(kind: &str, left: u32, ends: i64) -> Grant {
        Grant { kind: kind.into(), restores: vec!["seven_day".into()], left: Some(left), total: Some(1), ends_at: Some(ends), ..Grant::default() }
    }

    #[test]
    fn a_line_claims_the_first_ending_usable_grant_of_its_kind() {
        let mut r = Resets::none(0, "app_server");
        r.grants = vec![grant("codex_full", 1, 300), grant("claude_fable", 1, 50), grant("codex_full", 1, 200), grant("codex_full", 0, 100)];
        assert_eq!(claimable(&r, 0).map(|(i, _)| i), Some(2), "line 0 is codex_full: the one ending at 200, not the spent one");
        assert_eq!(claimable(&r, 1).map(|(i, _)| i), Some(1));
        assert_eq!(claimable(&r, 2), None);
        r.grants[2].usable_now = Some(false);
        assert_eq!(claimable(&r, 0).map(|(i, _)| i), Some(0));
    }

    #[test]
    fn of_claude_grants_only_the_session_reset_has_a_claim_path() {
        assert!(claim_path_on("codex", "codex_full") && claim_path_on("claude", "session"));
        for kind in [CLAUDE_FULL, CLAUDE_FABLE] {
            assert!(!claim_path_on("claude", kind), "{kind}");
        }
        assert!(!claim_path_on("kimi", "session"));
    }

    #[test]
    fn the_second_press_on_the_armed_line_confirms_and_nothing_else_does() {
        let mut b = ResetButtons::default();
        assert_eq!(b.press("codex", 1, 0, 1_000), None, "the first press arms");
        assert_eq!(b.armed_at(1_000).map(|a| a.line), Some(0));
        assert_eq!(b.press("codex", 1, 1, 1_001), None, "another line moves the arming");
        assert_eq!(b.press("codex", 2, 1, 1_002), None, "the same line of another account too");
        let done = b.press("codex", 2, 1, 1_003).expect("the second press on the armed line");
        assert_eq!((done.agent.as_str(), done.profile, done.line), ("codex", 2, 1));
        assert_eq!(b.armed_at(1_003), None, "a confirmation is used up");
        assert_eq!(b.press("codex", 2, 1, 1_004), None, "a third press arms again");

        b.press("claude", 3, 0, 2_000);
        assert_eq!(b.press("claude", 3, 0, 2_000 + ARMED_FOR_S + 1), None, "too late: it arms again");
        b.disarm();
        assert_eq!(b.press("claude", 3, 0, 2_100), None, "a press elsewhere let it go");
    }

    #[test]
    fn the_armed_line_says_what_comes_back_and_when_it_would_anyway() {
        let mut g = grant("codex_full", 1, 0);
        assert_eq!(armed_text(&g, Some(7), |t| format!("t{t}")), "press again: 7d back now, else at t7");
        g.restores = vec!["five_hour".into(), "seven_day".into()];
        assert_eq!(armed_text(&g, None, |t| t.to_string()), "press again: 5h+7d back now");
    }
}

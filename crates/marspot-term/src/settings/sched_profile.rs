//! How freely marspot moves panes between accounts: `sched.profile`,
//! and single numbers of it overridden as `sched.<name> = <number>`.
//!
//! `steady` switches only when it must and protects nothing by rule;
//! `balanced`, the default, switches a working pane at a boundary after
//! a notice and keeps other models off what only Fable can use;
//! `full` switches soonest and most often.  A word that names no
//! profile reads as `steady`, the most careful; a missing line is the
//! default.  The overrides are read here as numbers by name and checked
//! by the planner, which knows their ranges; the panel never writes
//! them, so a hand-written line is kept as written.
//!
//! `sched.credits` says when a Codex account's paid credits may be
//! spent: `last` (the default, also any unknown word) only when no other
//! account has plan allowance left; `hold` never; `free` without limit.

use super::Settings;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SchedProfile {
    Steady,
    #[default]
    Balanced,
    Full,
}

impl SchedProfile {
    pub(super) fn parse(v: &str) -> Self {
        match v.to_ascii_lowercase().as_str() {
            "balanced" => Self::Balanced,
            "full" => Self::Full,
            _ => Self::Steady,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Steady => "steady",
            Self::Balanced => "balanced",
            Self::Full => "full",
        }
    }

    /// The next profile, for a control that steps through them.
    pub fn next(self) -> Self {
        match self {
            Self::Steady => Self::Balanced,
            Self::Balanced => Self::Full,
            Self::Full => Self::Steady,
        }
    }
}

/// When paid credits may be spent, see the module note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SchedCredits {
    Hold,
    #[default]
    Last,
    Free,
}

impl SchedCredits {
    fn parse(v: &str) -> Self {
        match v.to_ascii_lowercase().as_str() {
            "hold" => Self::Hold,
            "free" => Self::Free,
            _ => Self::Last,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hold => "hold",
            Self::Last => "last",
            Self::Free => "free",
        }
    }
}

/// Overrides kept at most; a file with more keeps the first ones.
const OVERRIDES_CAP: usize = 32;

/// `sched.profile`, and the numeric `sched.*` overrides in file order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SchedSettings {
    pub profile: SchedProfile,
    pub credits: SchedCredits,
    pub overrides: Vec<(String, f64)>,
}

pub(super) fn value_of(s: &Settings, key: &str) -> Option<String> {
    match key {
        "sched.profile" => Some(s.sched.profile.as_str().to_string()),
        "sched.credits" => Some(s.sched.credits.as_str().to_string()),
        _ => None,
    }
}

/// Apply `key = v` when it is `sched.profile` or a numeric `sched.*`
/// override; false otherwise.
pub(super) fn parse_key(s: &mut Settings, key: &str, v: &str) -> bool {
    if key == "sched.profile" {
        s.sched.profile = SchedProfile::parse(v);
        return true;
    }
    if key == "sched.credits" {
        s.sched.credits = SchedCredits::parse(v);
        return true;
    }
    let Some(name) = key.strip_prefix("sched.") else { return false };
    let Ok(n) = v.parse::<f64>() else { return false };
    if !n.is_finite() {
        return false;
    }
    let o = &mut s.sched.overrides;
    match o.iter().position(|(k, _)| k == name) {
        Some(i) => o[i].1 = n,
        None if o.len() < OVERRIDES_CAP => o.push((name.to_string(), n)),
        None => {}
    }
    true
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::{SchedCredits, SchedProfile};

    #[test]
    fn sched_profile_reads_and_writes_back_and_an_unknown_word_is_steady() {
        assert_eq!(Settings::default().sched.profile, SchedProfile::Balanced);
        for (word, want) in [("steady", SchedProfile::Steady), ("Balanced", SchedProfile::Balanced), ("full", SchedProfile::Full), ("fast", SchedProfile::Steady), ("", SchedProfile::Steady)] {
            let body = format!("sched.profile = {word}\n");
            let s = parse(&body);
            assert_eq!(s.sched.profile, want, "{word:?}");
            assert_eq!(parse(&render(&s, &body)).sched.profile, want, "{word:?} after a rewrite");
        }
        assert_eq!(value_of(&parse("sched.profile = full\n"), "sched.profile"), "full");
        assert!(render(&Settings::default(), "").contains("sched.profile = balanced\n"));
    }

    #[test]
    fn sched_overrides_are_read_by_name_and_kept_by_a_rewrite() {
        let body = "sched.profile = full\nsched.min_stay_h = 2\nsched.min_stay_h = 2.5\nsched.per_day = x\nsched.reset = dryrun\n";
        let s = parse(body);
        assert_eq!(s.sched.overrides, vec![("min_stay_h".to_string(), 2.5)]);
        assert_eq!(s.sched_reset, AgentAllocation::DryRun, "the reset switch is not an override");
        let mut changed = s.clone();
        changed.sched.profile = SchedProfile::Steady;
        let out = render(&changed, body);
        assert!(out.contains("sched.profile = steady\nsched.min_stay_h = 2\nsched.min_stay_h = 2.5\n"), "{out}");
        assert_eq!(parse(&out).sched.overrides, s.sched.overrides);
        assert_eq!(SchedProfile::Full.next(), SchedProfile::Steady);
    }

    #[test]
    fn sched_credits_reads_and_writes_back_and_an_unknown_word_is_last() {
        assert_eq!(Settings::default().sched.credits, SchedCredits::Last);
        for (word, want) in [("hold", SchedCredits::Hold), ("Free", SchedCredits::Free), ("last", SchedCredits::Last), ("never", SchedCredits::Last)] {
            let body = format!("sched.credits = {word}\n");
            let s = parse(&body);
            assert_eq!(s.sched.credits, want, "{word:?}");
            assert!(s.sched.overrides.is_empty(), "not a numeric override");
            assert_eq!(parse(&render(&s, &body)).sched.credits, want, "{word:?} after a rewrite");
        }
        assert_eq!(value_of(&parse("sched.credits = hold\n"), "sched.credits"), "hold");
        assert!(render(&Settings::default(), "").contains("sched.credits = last\n"));
    }
}

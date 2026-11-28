//! The kinds of stored reset, in the words used here.

/// A Claude reset stored on the account: the week and the Fable window
/// both back to nothing used.
pub const CLAUDE_FULL: &str = "claude_full";
/// A Claude reset stored on the account: the Fable window back to
/// nothing used and part of the week's usage taken off.
pub const CLAUDE_FABLE: &str = "claude_fable";

/// The kind of an `agent` grant as a feed names it, in the words used
/// here: devops' first feeds named Claude's two stored resets `full`
/// and `fable`.
pub fn kind_of(agent: &str, word: &str) -> String {
    match (agent, word) {
        ("claude", "full") => CLAUDE_FULL.into(),
        ("claude", "fable") => CLAUDE_FABLE.into(),
        _ => word.into(),
    }
}

/// What a claude_fable reset takes off the week's usage, as a share of
/// what was used.  Half is all that is known; whether that is half of
/// what was used or a fixed half week is not verified yet.
pub const FABLE_WEEK_SHARE: f64 = 0.5;

/// What a grant brings back: the week, the restricted window, whether
/// using it gives the week back, and the share of the week's usage it
/// takes off.  The kinds known here by name decide; any other grant by
/// what it says it restores.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Brings {
    pub week: bool,
    pub window: bool,
    pub gives_back_week: bool,
    pub week_share: Option<f64>,
}

pub fn brings_back(g: &super::Grant) -> Brings {
    match g.kind.as_str() {
        CLAUDE_FULL => Brings { week: true, window: true, gives_back_week: false, week_share: None },
        CLAUDE_FABLE => Brings { week: false, window: true, gives_back_week: false, week_share: Some(FABLE_WEEK_SHARE) },
        _ => Brings {
            week: g.restores.iter().any(|w| w == "seven_day"),
            window: g.gives_back_weekly || g.restores.iter().any(|w| w == "seven_day_overage_included"),
            gives_back_week: g.gives_back_weekly,
            week_share: None,
        },
    }
}

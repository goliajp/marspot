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

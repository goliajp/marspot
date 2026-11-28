//! The lines an account card shows for its stored resets.

use super::{Grant, Outcome, Resets};

/// No account card lists more lines of resets than this.
pub const MAX_LINES: usize = 3;

/// How many lines [`lines`] gives for `r`, without writing them.
pub fn line_count(r: &Resets) -> usize {
    if r.outcome != Outcome::Read {
        return 1;
    }
    let distinct = r.grants.iter().enumerate().filter(|(i, g)| r.grants[..*i].iter().all(|o| o.kind != g.kind)).count();
    distinct.min(MAX_LINES)
}

/// The lines an account card shows for `r` at `now`, one per kind of
/// reset: `full 2/3 to 12/1 05:26`, or `full 2/3 to 12/1 05:26 from 11/28 09:00`
/// while the first of them cannot be used yet.  A Codex credit is
/// `full` there too, and Claude's stored resets `full` and `fable`.  `when` writes a time.  An account with none shows
/// none.
pub fn lines(r: &Resets, now: i64, when: impl Fn(i64) -> String) -> Vec<String> {
    match r.outcome {
        Outcome::CommandMissing => return vec!["resets: no /limit-reset".into()],
        Outcome::Ineligible => return vec![format!("resets: ineligible ({})", r.ineligible_reason.as_deref().unwrap_or("-"))],
        Outcome::Read => {}
    }
    let mut kinds: Vec<&str> = Vec::new();
    for g in &r.grants {
        if !kinds.contains(&g.kind.as_str()) {
            kinds.push(&g.kind);
        }
    }
    kinds
        .into_iter()
        .take(MAX_LINES)
        .map(|k| {
            let of: Vec<&Grant> = r.grants.iter().filter(|g| g.kind == k).collect();
            let left: u32 = of.iter().filter_map(|g| g.left).sum();
            let total: Option<u32> = of.iter().map(|g| g.total).sum();
            let live = || of.iter().filter(|g| g.left != Some(0));
            let shown = k.strip_prefix("codex_").or_else(|| k.strip_prefix("claude_")).unwrap_or(k);
            let mut line = match total {
                Some(t) => format!("{shown} {left}/{t}"),
                None => format!("{shown} {left}"),
            };
            let start = live().filter_map(|g| g.starts_at).min().filter(|s| *s > now);
            match (start, live().filter_map(|g| g.ends_at).min()) {
                (Some(s), Some(e)) => line.push_str(&format!(" to {} from {}", when(e), when(s))),
                (Some(s), None) => line.push_str(&format!(" from {}", when(s))),
                (None, Some(e)) => line.push_str(&format!(" to {}", when(e))),
                (None, None) => {}
            }
            if live().any(|g| g.usable_now == Some(false)) {
                line.push_str(" not now");
            }
            line
        })
        .collect()
}

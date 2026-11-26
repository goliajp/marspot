//! When each profile's windows end, as the feed gives them.  What panes
//! have learnt about their accounts is `marspot_agent::quota::seen`.

use marspot::cc_usage::CcAccount;

#[cfg(test)]
pub(in crate::plugins) use marspot_agent::quota::seen::Out;
pub(in crate::plugins) use marspot_agent::quota::seen::{Resets, Seen};

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
                    five_hour: a.reset_5h.unwrap_or(0),
                    week: a.reset_7d.unwrap_or(0),
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

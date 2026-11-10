//! What a kimi pane's badge says, and the one place it is published.

use super::super::{LogLevel, PluginHost};

/// `P2@k3` — the shape the other agents' badges have.  With no model
/// known the badge still says which agent and which account.
pub(super) fn badge_text(profile: Option<u8>, model: Option<&str>) -> String {
    let what = model.unwrap_or("kimi");
    match profile {
        Some(p) => format!("P{p}@{what}"),
        None => what.to_string(),
    }
}

/// Send the badge, every tick, and log it only when it is news.  The
/// send is unconditional because the core can be replaced while this
/// plugin still remembers having sent one; the core ignores a repeat.
pub(super) fn publish_badge(
    host: &dyn PluginHost,
    last: &mut std::collections::HashMap<u64, String>,
    sid: u64,
    text: &str,
) {
    if let Err(e) = host.set_pane_badge(sid, text) {
        host.log(LogLevel::Warn, "badge.set_failed", &format!("sid={sid}: {e}"));
        return;
    }
    if last.get(&sid).map(String::as_str) != Some(text) {
        host.log(LogLevel::Info, "badge.changed", &format!("sid={sid} badge={text:?}"));
        last.insert(sid, text.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_badge_leads_with_the_profile_and_names_the_model() {
        assert_eq!(badge_text(Some(2), Some("k3")), "P2@k3");
        assert_eq!(badge_text(None, Some("k3")), "k3");
        assert_eq!(badge_text(Some(1), None), "P1@kimi");
        assert_eq!(badge_text(None, None), "kimi");
    }
}

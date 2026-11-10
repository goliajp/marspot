//! What a kimi pane's badge says, and the one place it is published.

use super::super::{LogLevel, PluginHost};

/// `P2@k3·high` — the shape the claude and codex badges have for
/// profile, model and effort.  The effort is kimi's own word for it:
/// one of the model's levels, `on` for a model that thinks without
/// levels, `off`.  What is not known is left out, and with no model
/// known the badge still says which agent and which account.
pub(super) fn badge_text(profile: Option<u8>, model: Option<&str>, thinking: Option<&str>) -> String {
    let what = match (model, thinking) {
        (Some(m), Some(t)) => format!("{m}·{t}"),
        (Some(m), None) => m.to_string(),
        (None, Some(t)) => format!("kimi·{t}"),
        (None, None) => "kimi".to_string(),
    };
    match profile {
        Some(p) => format!("P{p}@{what}"),
        None => what,
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
        assert_eq!(badge_text(Some(2), Some("k3"), Some("high")), "P2@k3·high");
        assert_eq!(badge_text(None, Some("k3"), None), "k3");
        assert_eq!(badge_text(None, Some("kimi-for-coding-highspeed"), Some("on")), "kimi-for-coding-highspeed·on");
        assert_eq!(badge_text(Some(1), None, None), "P1@kimi");
        assert_eq!(badge_text(None, None, Some("off")), "kimi·off");
        assert_eq!(badge_text(None, None, None), "kimi");
    }
}

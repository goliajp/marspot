//! What a gemini pane's badge says, and the one place it is published.

use super::super::{LogLevel, PluginHost};

/// `P2@gemini-3-pro` — the shape the claude and codex badges use, so
/// the three read as one system.  gemini has no effort setting to show.
/// The model is written as gemini names it; with none known the badge
/// still says which agent and which account the pane is on.
pub(super) fn badge_text(profile: Option<u8>, model: Option<&str>) -> String {
    let what = model.unwrap_or("gemini");
    match profile {
        Some(p) => format!("P{p}@{what}"),
        None => what.to_string(),
    }
}

/// Send the badge, every tick, and log it only when it is news.
///
/// The send is unconditional because L2's state can outlive L1's: a
/// core swap starts L2 with an empty badge map while this plugin still
/// remembers having sent one.  L2 ignores a repeat without repainting,
/// so the cache decides only whether there is anything worth a log line.
pub(super) fn publish_badge(
    host: &dyn PluginHost,
    last: &mut std::collections::HashMap<u64, String>,
    sid: u64,
    text: &str,
) {
    if let Err(e) = host.set_pane_badge(sid, text) {
        host.log(LogLevel::Warn, "gemini.badge.set_failed", &format!("sid={sid}: {e}"));
        return;
    }
    if last.get(&sid).map(String::as_str) != Some(text) {
        host.log(LogLevel::Info, "gemini.badge.changed", &format!("sid={sid} badge={text:?}"));
        last.insert(sid, text.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::PluginError;
    use std::path::PathBuf;
    use std::sync::Mutex;

    #[test]
    fn the_badge_leads_with_the_profile_and_names_the_model() {
        assert_eq!(badge_text(Some(2), Some("gemini-3-pro")), "P2@gemini-3-pro");
        assert_eq!(badge_text(None, Some("flash")), "flash");
        assert_eq!(badge_text(Some(1), None), "P1@gemini");
        assert_eq!(badge_text(None, None), "gemini");
    }

    #[derive(Default)]
    struct FakeHost {
        badges: Mutex<Vec<(u64, String)>>,
        logs: Mutex<Vec<String>>,
    }

    impl PluginHost for FakeHost {
        fn state_dir(&self) -> Result<PathBuf, PluginError> {
            Ok(std::env::temp_dir())
        }
        fn log(&self, _l: LogLevel, tag: &str, _m: &str) {
            self.logs.lock().unwrap().push(tag.to_string());
        }
        fn set_pane_badge(&self, sid: u64, text: &str) -> Result<(), PluginError> {
            self.badges.lock().unwrap().push((sid, text.to_string()));
            Ok(())
        }
    }

    #[test]
    fn an_unchanged_gemini_badge_is_still_sent() {
        let host = FakeHost::default();
        let mut last = std::collections::HashMap::new();
        for _ in 0..3 {
            publish_badge(&host, &mut last, 7, "P1@gemini");
        }
        assert_eq!(host.badges.lock().unwrap().len(), 3, "every tick sends");
        assert_eq!(
            host.logs.lock().unwrap().iter().filter(|t| *t == "gemini.badge.changed").count(),
            1,
            "but only the first one is news"
        );
    }
}

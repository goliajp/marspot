//! What a codex pane's badge says, and the one place it is published.

use super::super::{LogLevel, PluginHost};

/// `gpt-6-astra·high` — the shape claudecode's badge already uses for
/// its own model and effort, so the two read as one system.
pub(super) fn badge_text(model: Option<&str>, effort: Option<&str>) -> String {
    match (model, effort) {
        (Some(m), Some(e)) => format!("{m}·{e}"),
        (Some(m), None) => m.to_string(),
        (None, Some(e)) => format!("codex·{e}"),
        (None, None) => "codex".to_string(),
    }
}

/// Send the badge, every tick, and log it only when it is news.
///
/// The send is unconditional for the same reason the declarations
/// beside it are: L2's state can outlive L1's.  A core swap starts L2
/// with an empty badge map while this plugin lives on with a cache
/// saying the badge is already there, so a badge sent once was gone
/// for good after the next silent update and the corner stayed blank
/// until the model or effort happened to change.  L2 ignores a repeat
/// without repainting, so the cache decides only whether there is
/// anything worth a log line.
pub(super) fn publish_badge(
    host: &dyn PluginHost,
    last: &mut std::collections::HashMap<u64, String>,
    sid: u64,
    text: &str,
) {
    if let Err(e) = host.set_pane_badge(sid, text) {
        host.log(LogLevel::Warn, "codex.badge.set_failed", &format!("sid={sid}: {e}"));
        return;
    }
    if last.get(&sid).map(String::as_str) != Some(text) {
        host.log(LogLevel::Info, "codex.badge.changed", &format!("sid={sid} badge={text:?}"));
        last.insert(sid, text.to_string());
    }
}

#[cfg(test)]
mod badge_publish_tests {
    use super::*;
    use std::path::PathBuf;
    use crate::plugins::PluginError;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeHost {
        badges: Mutex<Vec<(u64, String)>>,
        logs: Mutex<Vec<String>>,
    }

    impl PluginHost for FakeHost {
        fn pane_count(&self) -> usize {
            0
        }
        fn pane_pty_device(&self, _p: usize) -> Result<Option<PathBuf>, PluginError> {
            Ok(None)
        }
        fn pane_pty_pid_tree(
            &self,
            _p: usize,
        ) -> Result<Vec<crate::plugins::PtyChild>, PluginError> {
            Ok(Vec::new())
        }
        fn pane_focused(&self) -> Option<usize> {
            None
        }
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

    /// The badge goes out on every tick, and only a change is logged.
    ///
    /// Sending it once was enough until a core swap: L2 comes back
    /// with an empty badge map, this plugin comes back with nothing to
    /// send, and the pane's corner stays blank — which is what the
    /// user saw after a silent update (2026-09-26).
    #[test]
    fn an_unchanged_badge_is_still_sent() {
        let host = FakeHost::default();
        let mut last = std::collections::HashMap::new();
        for _ in 0..3 {
            publish_badge(&host, &mut last, 7, "gpt-6-astra\u{b7}medium");
        }
        assert_eq!(
            host.badges.lock().unwrap().len(),
            3,
            "every tick sends; the cache must not gate the send"
        );
        assert_eq!(
            host.logs.lock().unwrap().iter().filter(|t| *t == "codex.badge.changed").count(),
            1,
            "but only the first one is news"
        );
    }

    /// A refused send must not be remembered as delivered.
    #[test]
    fn a_refused_send_is_not_cached() {
        struct Refusing;
        impl PluginHost for Refusing {
            fn pane_count(&self) -> usize {
                0
            }
            fn pane_pty_device(&self, _p: usize) -> Result<Option<PathBuf>, PluginError> {
                Ok(None)
            }
            fn pane_pty_pid_tree(
                &self,
                _p: usize,
            ) -> Result<Vec<crate::plugins::PtyChild>, PluginError> {
                Ok(Vec::new())
            }
            fn pane_focused(&self) -> Option<usize> {
                None
            }
            fn state_dir(&self) -> Result<PathBuf, PluginError> {
                Ok(std::env::temp_dir())
            }
            fn log(&self, _l: LogLevel, _t: &str, _m: &str) {}
            fn set_pane_badge(&self, _sid: u64, _text: &str) -> Result<(), PluginError> {
                Err(PluginError::Other("no".into()))
            }
        }
        let mut last = std::collections::HashMap::new();
        publish_badge(&Refusing, &mut last, 7, "codex");
        assert!(last.is_empty(), "nothing reached the pane, so nothing is remembered");
    }
}

#[cfg(test)]
mod badge_text_tests {
    use super::badge_text;

    /// The badge reads like claudecode's, so a window with both does
    /// not look like two unrelated tools.
    #[test]
    fn badge_pairs_model_with_effort() {
        assert_eq!(badge_text(Some("gpt-6-astra"), Some("high")), "gpt-6-astra·high");
        assert_eq!(badge_text(Some("gpt-6-astra"), None), "gpt-6-astra");
        assert_eq!(badge_text(None, None), "codex");
    }
}

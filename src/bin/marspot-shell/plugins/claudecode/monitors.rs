//! The auto-retry monitors' upkeep, from the tick.

use super::{ClaudecodePlugin, MonitorState, retryable_error_kind};
use crate::plugins::{LogLevel, PluginHost};

impl ClaudecodePlugin {
    /// RFC-003 C7 (observe-only): drain raw PTY bytes for every
    /// active monitor and log any retryable error markers.  No
    /// auto-action yet — user wants the wire proven without the
    /// "press Enter for them" half landing as policy.  Action is
    /// added later when there's a concrete decision to make per
    /// `kind`; the throttle counter still ticks so we can see how
    /// often patterns fire in practice.
    pub(super) fn pump_monitors(&mut self, host: &dyn PluginHost) {
        const MATCH_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
        const MAX_LINE_BUF: usize = 16 * 1024;

        let now = std::time::Instant::now();
        for (sid, mon) in self.monitors.iter_mut() {
            // Drain raw bytes; cheap when nothing new.
            let mut pending: Vec<u8> = Vec::new();
            {
                let rx = mon.rx.lock().unwrap();
                while let Ok(chunk) = rx.try_recv() {
                    pending.extend_from_slice(&chunk);
                }
            }
            if pending.is_empty() {
                continue;
            }
            mon.line_buf.extend_from_slice(&pending);
            if mon.line_buf.len() > MAX_LINE_BUF {
                let keep = mon.line_buf.len() - MAX_LINE_BUF / 2;
                mon.line_buf.drain(..keep);
            }
            let Some(kind) = retryable_error_kind(&mon.line_buf) else { continue };
            mon.line_buf.clear();
            // Update the rolling window so we can SEE rate even if
            // we don't act on it.
            mon.retry_history.retain(|t| now.duration_since(*t) <= MATCH_WINDOW);
            mon.retry_history.push(now);
            host.log(
                LogLevel::Info,
                "monitor.match",
                &format!(
                    "shelld_session={} kind={} count_in_window={}",
                    sid,
                    kind,
                    mon.retry_history.len()
                ),
            );
        }
    }

    /// Spin up monitors for newly-bound sessions; drop monitors whose
    /// bind went away.  Called from tick() after `last_meta` is
    /// updated, so it reflects the current bound set.
    pub(super) fn refresh_monitors(&mut self, host: &dyn PluginHost) {
        let Some(client) = self.shelld.as_ref() else { return };
        // F2+2a — `attach_raw_only` is a hard-coded `Err(Unsupported)`
        // until the L3 PTY fan-out wire ships (see line 105 TODO).
        // Without this gate, every plugin tick re-attempts attach for
        // every bound session and logs `monitor.attach_failed` each
        // time — measured as 4.5 lines / s across 9 panes, 94 % of
        // all log volume.  Once the first attempt comes back
        // `Unsupported`, latch the flag and skip the whole loop.
        if self.monitor_unsupported {
            return;
        }
        // Add monitors for newly-bound sessions.
        let mut new_keys: Vec<u64> = Vec::new();
        for sid in self.last_meta.keys() {
            if !self.monitors.contains_key(sid) {
                new_keys.push(*sid);
            }
        }
        for sid in new_keys {
            match client.attach_raw_only(sid) {
                Ok(rx) => {
                    self.monitors.insert(
                        sid,
                        MonitorState {
                            rx: std::sync::Mutex::new(rx),
                            line_buf: Vec::new(),
                            retry_history: Vec::new(),
                        },
                    );
                    host.log(
                        LogLevel::Info,
                        "monitor.start",
                        &format!("shelld_session={}", sid),
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
                    // Permanent: the host doesn't (yet) expose PTY
                    // raw broadcast.  Log once at Info, latch the
                    // flag so subsequent ticks short-circuit.
                    host.log(
                        LogLevel::Info,
                        "monitor.disabled",
                        &format!(
                            "attach_raw_only unsupported by host ({}); \
                             monitor feature disabled until wire lands",
                            e
                        ),
                    );
                    self.monitor_unsupported = true;
                    break;
                }
                Err(e) => {
                    host.log(
                        LogLevel::Warn,
                        "monitor.attach_failed",
                        &format!("shelld_session={} err={}", sid, e),
                    );
                }
            }
        }
        // Drop monitors whose bind went away.
        let stale: Vec<u64> = self
            .monitors
            .keys()
            .copied()
            .filter(|sid| !self.last_meta.contains_key(sid))
            .collect();
        for sid in stale {
            self.monitors.remove(&sid);
            // detach_raw returns Result; ignore the error here — the
            // pane is going away regardless, and a stale ATTACH that
            // failed to clean up is harmless (next L3 reattach overrides).
            let _ = client.detach_raw(sid);
            host.log(
                LogLevel::Info,
                "monitor.stop",
                &format!("shelld_session={}", sid),
            );
        }
    }
}

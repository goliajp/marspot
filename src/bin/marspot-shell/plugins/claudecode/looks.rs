//! What each pane wears for this scan: badge and title.

use super::*;

impl ClaudecodePlugin {
    /// Push every pane's badge + title for this scan.
    ///
    /// Three populations, and the middle one is why this is its own
    /// method: panes that just lost their claude (clear), panes we are
    /// deliberately holding (keep exactly what they were frozen with),
    /// and panes running claude now (the live badge).
    ///
    /// Re-pushed every tick rather than on change: L2 can spawn /
    /// crash / respawn between ticks (CORE_BOOT_LOOP, silent update),
    /// and transition-only would leave a fresh core with no badges
    /// until something moved.  Per tick ≤ 9 small frames.
    pub(super) fn publish_looks(&self, host: &dyn PluginHost, result: &ScanResult) {
        for (sh_sid, cc_sid) in &self.last_mapping {
            if result.new_mapping.contains_key(sh_sid) {
                continue;
            }
            // A held pane lost its claude because WE took it down.
            // Clearing its corner would be the reclamation announcing
            // itself on a pane whose whole point is standing still —
            // the frozen look is re-asserted below instead.
            if self.held.contains_key(sh_sid) {
                continue;
            }
            host.log(
                LogLevel::Info,
                "session.unbound",
                &format!("shelld_session={} (was sid={})", sh_sid, cc_sid),
            );
            let _ = host.set_pane_badge(*sh_sid, "");
            let _ = host.set_pane_agent_tui(*sh_sid, false);
            let _ = host.set_pane_title(*sh_sid, "");
        }
        // Held panes wear the badge and title they were frozen with.
        for (sh_sid, look) in &self.held {
            if result.new_mapping.contains_key(sh_sid) {
                continue;
            }
            let _ = host.set_pane_badge(*sh_sid, &look.badge);
            let _ = host.set_pane_agent_tui(*sh_sid, true);
            if !look.title.is_empty() {
                let _ = host.set_pane_title(*sh_sid, &look.title);
            }
        }
        for (sh_sid, cc_sid) in &result.new_mapping {
            // Dev-cycle diagnostic.  The badge is assembled from four
            // separate lookups (profile tag, bound uuid, transcript
            // path, model) and any one going quiet leaves a half-badge
            // that says nothing about which — `P3` with no `@model`
            // took a screenshot and a manual dig to explain
            // (2026-08-10).  Logged only when it *changes*, so a
            // steady screen costs nothing.
            // The badge text does not include the session, so a pane
            // moving to another session under the same badge said
            // nothing; this is where a wrong binding is first visible.
            if let Some(meta) = result.new_meta.get(sh_sid) {
                let was = self.last_meta.get(sh_sid);
                if was.map(|w| (w.uuid.as_str(), w.bound_by)) != Some((meta.uuid.as_str(), meta.bound_by)) {
                    host.log(
                        LogLevel::Info,
                        "session.binding",
                        &format!(
                            "sid={sh_sid} uuid={:?} by={} was={:?} by={}",
                            meta.uuid,
                            meta.bound_by,
                            was.map(|w| w.uuid.as_str()).unwrap_or("-"),
                            was.map(|w| w.bound_by).unwrap_or("-"),
                        ),
                    );
                }
            }
            if self.last_mapping.get(sh_sid) != Some(cc_sid) {
                let meta = result.new_meta.get(sh_sid);
                host.log(
                    LogLevel::Info,
                    "badge.changed",
                    &format!(
                        "sid={sh_sid} badge={cc_sid:?} was={:?} cfg={:?} uuid={:?}",
                        self.last_mapping.get(sh_sid).map(|s| s.as_str()).unwrap_or("-"),
                        meta.and_then(|m| m.config_dir.as_deref()).unwrap_or("-"),
                        meta.map(|m| m.uuid.as_str()).unwrap_or("-"),
                    ),
                );
            }
            // Re-issued every tick: a core swap starts L2 with an
            // empty map, and this is what tells it to merge the hard
            // wraps a fixed-width TUI puts in the middle of a path.
            let _ = host.set_pane_agent_tui(*sh_sid, true);
            if let Err(e) = host.set_pane_badge(*sh_sid, cc_sid) {
                host.log(LogLevel::Warn, "pane_badge.set_failed", &format!("{e}"));
            }
            // Title 只用 project basename — profile 已经在 badge 里
            // ("P3 …"),title 再重复就冗余.
            if let Some(meta) = result.new_meta.get(sh_sid)
                && !meta.project_basename.is_empty()
                    && let Err(e) = host.set_pane_title(*sh_sid, &meta.project_basename) {
                        host.log(
                            LogLevel::Warn,
                            "pane_title.set_failed",
                            &format!("{e}"),
                        );
                    }
        }
    }
}

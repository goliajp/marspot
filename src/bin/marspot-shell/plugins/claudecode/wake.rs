//! How a reclaim run ended, back to the plugin that started it.
//!
//! The run (park, wait for the user, resume) belongs to the op queue,
//! and the plugin keeps three things about it: the pane is `armed`, its
//! `dormant` record, and the frozen look in `held`.  A run that ends
//! without claude coming back must let go of all three, or the pane
//! stays marked dormant for ever with a claude badge over a plain shell.

use std::sync::{Arc, Mutex};

use super::{ClaudecodePlugin, LogLevel, PluginHost, pty_op};

/// Where runs report their end: `(pane, outcome)`, drained every tick.
pub(super) type WakeEnds = Arc<Mutex<Vec<(u64, pty_op::OpOutcome)>>>;

impl ClaudecodePlugin {
    /// The end hook for a reclaim run on `sid`.
    pub(super) fn wake_reporter(&self, sid: u64) -> pty_op::OnEnd {
        let ends = Arc::clone(&self.wake_ends);
        Arc::new(move |outcome| ends.lock().unwrap().push((sid, outcome.clone())))
    }

    /// Forget every pane whose reclaim run failed or timed out without
    /// claude back.
    ///
    /// `Done` means claude is running again (resumed, or found already
    /// there); the scan's binding takes the pane over from there.
    pub(super) fn settle_wake_ends(&mut self, host: &dyn PluginHost) {
        let ended = std::mem::take(&mut *self.wake_ends.lock().unwrap());
        let mut dropped = false;
        for (sid, outcome) in ended {
            if outcome.is_done() {
                continue;
            }
            // Ended from outside (the pane closed, the host tore its
            // sessions down): the run is gone but the pane may still be
            // parked.  Un-arm it and let the re-arm sweep judge it.
            if matches!(outcome, pty_op::OpOutcome::Cancelled(_)) {
                self.armed.remove(&sid);
                continue;
            }
            host.log(
                LogLevel::Warn,
                "hibernate.wake_failed",
                &format!("pane {sid}: {outcome:?}; no longer dormant"),
            );
            self.armed.remove(&sid);
            let before = self.dormant.len();
            self.dormant.retain(|d| d.shelld_sid != sid);
            dropped |= self.dormant.len() != before;
            if self.held.remove(&sid).is_some() {
                // nothing else clears a look the scan no longer maps
                let _ = host.set_pane_badge(sid, "");
                let _ = host.set_pane_agent_tui(sid, false);
                let _ = host.set_pane_title(sid, "");
                if let Some(client) = &self.shelld {
                    let _ = client.hold_grid_of(sid, false);
                }
            }
        }
        if dropped {
            self.persist_dormant(host);
        }
    }
}

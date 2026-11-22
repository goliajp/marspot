//! Panes by session, wherever they live.
//!
//! A pane is either in a window or in `shadows`, which holds sessions
//! that run without being drawn.  Anything keyed by session id, or
//! done once for every session, goes through `panes_mut`; anything
//! about what is drawn (render, layout, hit-testing) walks `windows`.

use super::*;

impl CoreApp {
    /// Every pane: those in windows, then the shadows.
    pub(super) fn panes_mut(&mut self) -> impl Iterator<Item = &mut Pane> {
        self.windows
            .iter_mut()
            .flat_map(|w| w.panes.iter_mut())
            .chain(self.shadows.iter_mut())
    }

    /// The pane whose L3 is session `sid`, drawn or not.
    pub(super) fn pane_by_sid_mut(&mut self, sid: u64) -> Option<&mut Pane> {
        self.panes_mut()
            .find(|p| p.session().l3_session_id() == Some(sid))
    }

    /// An off-loop spawn finished: the pending slot takes the L3, or
    /// falls back to a vacant slot whose revive-on-keystroke path is
    /// exactly the retry this needs.
    pub(super) fn adopt_spawn_outcome(&mut self, sid: u64, outcome: SpawnOutcome) {
        // the slot was closed while the spawn ran: the child is dropped
        // with the L3Spawn, which kills it, so nothing to reap here
        let Some(pane) = self
            .panes_mut()
            .find(|p| p.shelld_session_id() == Some(sid))
        else {
            return;
        };
        match outcome.0 {
            Ok(spawn) => {
                pane.adopt_backend(marspot::pane::PaneBackend::L3(marspot::pane::L3Conn::new(
                    spawn, sid,
                )));
                lx_event!(
                    "L3_SPAWN_ADOPTED",
                    "off-loop spawn landed; pending slot is now live",
                    session_id = sid
                );
            }
            Err(e) => {
                // swap the backend rather than replacing the whole
                // `Pane`, so pane-level state outlives a failed spawn
                let (c, r) = {
                    let g = pane.session().grid();
                    (g.cols(), g.rows())
                };
                pane.adopt_backend(marspot::pane::PaneBackend::Vacant(
                    marspot::pane::VacantPane::new(sid, c, r),
                ));
                lx_error!("core.spawn.failed", &e.to_string(), session_id = sid);
            }
        }
        self.mark_sid_window_dirty(sid);
    }
}

#[cfg(test)]
#[path = "panes_mut_tests.rs"]
mod panes_mut_tests;

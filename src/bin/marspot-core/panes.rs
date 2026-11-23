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

    /// Every pane's display name by session id.  Every window and the
    /// shadows: whether a name is shared cannot be known from one
    /// window alone, and a shadow pane is named the way L1 names it.
    pub(super) fn pane_names(&self) -> std::collections::HashMap<u64, String> {
        let drawn = self.windows.iter().flat_map(|w| w.panes.iter()).map(|p| (p, false));
        let refs: Vec<marspot::pane_name::PaneRef> = drawn
            .chain(self.shadows.iter().map(|p| (p, true)))
            .filter_map(|(p, shadow)| {
                let sid = p.shelld_session_id()?;
                let cwd = self.pane_cwds.get(&sid).cloned().unwrap_or_default();
                Some(marspot::pane_name::PaneRef::new(sid, cwd).with_shadow(shadow))
            })
            .collect();
        marspot::pane_name::assign(&refs).into_iter().collect()
    }

    /// The pane whose L3 is session `sid`, drawn or not.
    pub(super) fn pane_by_sid_mut(&mut self, sid: u64) -> Option<&mut Pane> {
        self.panes_mut()
            .find(|p| p.session().l3_session_id() == Some(sid))
    }

    /// Take pane `idx` out of window `wi`, leaving a placeholder.
    ///
    /// RFC-006 §3 — a move-out leaves a DORMANT PLACEHOLDER, not a
    /// hole: the user is arranging space, and the arrangement they
    /// left behind holds still.  Nothing shifts, so no index
    /// arithmetic; only state that pointed AT the moved pane is
    /// dropped.  (Close keeps its compacting semantics — close
    /// means "done with this space"; move means "keep my layout".)
    pub(super) fn take_out_of_window(&mut self, wi: usize, idx: usize) -> Pane {
        let (pc, pr) = {
            let g = win!(self, wi).panes[idx].session().grid();
            (g.cols(), g.rows())
        };
        let pane = std::mem::replace(&mut win!(self, wi).panes[idx], Pane::new_dormant(pc, pr));
        let w = &mut win!(self, wi);
        if w.focused_idx == idx {
            // Focus the nearest live pane; a placeholder is space,
            // not a focus target.
            w.focused_idx = (0..w.panes.len())
                .filter(|&i| !w.panes[i].is_dormant())
                .min_by_key(|&i| i.abs_diff(idx))
                .unwrap_or(idx);
        }
        if let Some(sel) = w.selection
            && sel.session_idx == idx
        {
            w.selection = None;
            w.selection_dragging = false;
        }
        pane
    }

    /// An off-loop spawn finished: the pending slot takes the L3, or
    /// falls back to a vacant slot whose revive-on-keystroke path is
    /// exactly the retry this needs.
    pub(super) fn adopt_spawn_outcome(&mut self, sid: u64, outcome: SpawnOutcome) {
        // the pane was closed while the spawn ran, when there was no L3
        // to tell: it is told now.  A pane nobody closed is in a window
        // that was put away, and its session waits for that window
        let closed = self.pane_control.shadows.closed.take(sid);
        let Some(pane) = self
            .panes_mut()
            .find(|p| p.shelld_session_id() == Some(sid))
        else {
            match outcome.0 {
                _ if !closed => {}
                Ok(_) => self.retire_pane_session(sid, true),
                Err(_) => marspot::session_close::close_dead_session(sid),
            }
            self.spawn_landed(sid, Err("the pane went away while it started".into()));
            return;
        };
        let landed = outcome.0.as_ref().map(|_| ()).map_err(Clone::clone);
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
        self.spawn_landed(sid, landed);
    }
}

#[cfg(test)]
#[path = "panes_mut_tests.rs"]
mod panes_mut_tests;

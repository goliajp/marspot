//! The helper panel's move to a window, and what its row says when the
//! move failed; the deny list's watcher; the dev seam's spots.

use std::path::{Path, PathBuf};

use marspot::shell_proto::{DoneCode, PanePlace, PlaceWhere};
use marspot::ui::strings::{Msg, t};

use super::super::shadow_place::L2_REQ_UNTRACKED;
use super::*;

/// The request id of a move asked for on the panel: one of the core's
/// own, so its answer goes to [`CoreApp::panel_move_answered`] and not
/// to the shell.
pub(super) const PANEL_MOVE: u32 = L2_REQ_UNTRACKED + 1;

/// The files to watch for the deny list at `path`: the name itself, so
/// a symlink pointed elsewhere is seen, and where it leads now.
pub(super) fn deny_watched(path: &Path) -> Vec<PathBuf> {
    let file = marspot::control_deny::file_of(path);
    if file == path { vec![file] } else { vec![path.to_path_buf(), file] }
}

impl CoreApp {
    /// Into the window of its parent.
    pub(super) fn move_helper_to_window(&mut self, sid: u64, parent: u64) {
        if self.pane_control.shadows.family.unmoved.remove(&sid) {
            self.helper_rows_changed();
        }
        let m = PanePlace { req_id: PANEL_MOVE, sid, to: PlaceWhere::Window, host_sid: parent, force: false };
        let route = self.place_pane(m);
        lx_event!("HELPER_PANEL_MOVE", "the user moved a shadow pane to a window", session = sid, route = format!("{route:?}"));
    }

    /// The core answered request `req_id` itself: a move asked on the
    /// panel that failed is said on the pane's row.
    pub(crate) fn panel_move_answered(&mut self, req_id: u32, sid: u64, code: DoneCode) {
        if req_id != PANEL_MOVE || code == DoneCode::Ok || self.shadow_index(sid).is_none() {
            return;
        }
        if self.pane_control.shadows.family.unmoved.insert(sid) {
            self.helper_rows_changed();
        }
    }

    /// The second line of `sid`'s row, of `doing`.
    pub(super) fn doing(&self, sid: u64, doing: String) -> String {
        if self.pane_control.shadows.family.unmoved.contains(&sid) {
            format!("{} · {doing}", t(Msg::HelperNotMoved))
        } else {
            doing
        }
    }

    /// The deny list's watcher, which wakes the loop when it changes.
    pub(super) fn watch_deny(&self) -> Option<Watcher> {
        let wake = self.event_tx.clone();
        let deny = Watcher::spawn(&deny_watched(&marspot::control_deny::path()), move || {
            DENY_CHANGED.store(true, Ordering::Release);
            drop(wake.send(CoreEvent::L3Ready));
        });
        if let Err(e) = &deny {
            lx_warn!("core.helper_panel.deny_unwatched", &format!("{e}; shown as it was when the panel opened"));
        }
        deny.ok()
    }

    /// Dev seam (`MARSPOT_DEV_PANELS`): where the first pane's `+N` is,
    /// then the first button of each kind on the open panel.
    pub(crate) fn dev_helper_spots(&mut self, wi: usize) -> [Option<(f64, f64)>; 6] {
        let mut out = [None; 6];
        if self.shadows.is_empty() || !self.dev_panels.as_ref().is_some_and(|s| s.wants_helpers()) {
            return out;
        }
        if let Some(c) = win!(self, wi).layout.cells.first() {
            let (x, y, w) = (c.x, c.y_top + 4.0, c.w as usize);
            out[0] = (0..w)
                .map(|dx| (x + dx as f64, y))
                .find(|p| matches!(self.chrome_at(wi, p.0, p.1), Some(ChromeTarget::Helpers(0))));
        }
        if win!(self, wi).helpers_panel.is_some() {
            let r = self.helpers_rect(wi);
            let at = |i: usize, n: f64| i as f64 * 4.0 + n + 2.0;
            for (iy, ix) in (0..(r.h / 4.0) as usize).flat_map(|iy| (0..(r.w / 4.0) as usize).map(move |ix| (iy, ix))) {
                let p = (at(ix, r.x), at(iy, r.y_top));
                let spot = match self.helper_ask_at(wi, p) {
                    Ask::Move(0) => 1,
                    Ask::Close(0) => 2,
                    Ask::CloseAll(0) => 3,
                    Ask::Control(0) => 4,
                    Ask::View(0) => 5,
                    _ => continue,
                };
                out[spot].get_or_insert(p);
            }
        }
        out
    }

    pub(super) fn helper_ask_at(&mut self, wi: usize, p: (f64, f64)) -> Ask {
        let surface = win!(self, wi).helpers_panel.as_ref().map(|s| Rc::clone(&s.surface));
        let hit = surface.and_then(|s| s.borrow().kept.surface.hit(p));
        hit.map_or(Ask::None, |h| helpers_view::asked(h.action))
    }
}

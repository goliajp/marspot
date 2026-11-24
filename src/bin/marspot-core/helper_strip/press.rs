//! A press on a strip: a helper's row shows that helper in the pane's
//! place, `main` the pane itself again, and the row that says how many
//! more there are opens the helper panel.

use super::compose::Spot;
use super::*;

impl CoreApp {
    /// Whether a press landed on a pane's strip.  It focuses the pane,
    /// and drops the selection as a press off one does.
    pub(in super::super) fn strip_press(&mut self, wi: usize, x_phys: f64, y_phys: f64) -> bool {
        let Some((idx, line)) = self.strip_at(wi, x_phys, y_phys) else { return false };
        let w = &mut win!(self, wi);
        w.selection_dragging = false;
        w.needs_render |= w.selection.take().is_some();
        self.focus_pane(wi, idx);
        let Some(host) = win!(self, wi).panes.get(idx).and_then(|p| p.shelld_session_id()) else { return true };
        let Some(h) = self.strips.hosts.get(&host) else { return true };
        match h.drawn.at(line) {
            Some(Spot::Main) => {
                self.view_helper(host, None);
            }
            Some(Spot::Row(i)) => {
                let helper = self.pane_control.strips.of(host).and_then(|r| h.sid_at(r, i));
                if let Some(helper) = helper.filter(|s| win!(self, wi).panes[idx].peeked() != Some(*s)) {
                    self.view_helper(host, Some(helper));
                }
            }
            Some(Spot::More) => self.open_helpers(wi, host),
            None => {}
        }
        true
    }
}

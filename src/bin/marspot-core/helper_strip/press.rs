//! A press on a strip: a helper's row shows that helper in the pane's
//! place, `main` the pane itself again, and the row that says how many
//! more there are opens the helper panel; one on the row in view goes
//! back to the pane itself.  And what stops a helper.

use super::compose::{Pick, Spot};
use super::keys::At;
use super::*;
use marspot::input::LogicalKey;

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
                // the row in view goes back to the pane itself: under
                // Claude's own list the strip may draw no `main`
                let helper = self.pane_control.strips.of(host).and_then(|r| h.sid_at(r, i));
                let peeked = win!(self, wi).panes[idx].peeked();
                if let Some(helper) = helper {
                    self.view_helper(host, Some(helper).filter(|s| peeked != Some(*s)));
                }
            }
            Some(Spot::More) => self.open_helpers(wi, host),
            None => {}
        }
        true
    }

    /// `Enter` in the strip of `at` on `selected`: the row in view, as
    /// `main`, goes back to the pane itself.
    pub(super) fn strip_enter(&mut self, at: At, selected: Pick, key: &LogicalKey) -> bool {
        self.pane_control.shadows.peeks.held.swallowed(key);
        self.leave_strip_keys();
        let helper = match selected {
            Pick::Main => None,
            Pick::Row(i) => self.pane_control.strips.of(at.host).and_then(|r| self.strips.hosts.get(&at.host)?.sid_at(r, i)),
        };
        let to = helper.filter(|s| *s != at.shown);
        if to.is_some() || at.shown != at.host {
            self.view_helper(at.host, to);
        }
        true
    }

    /// Stop a helper, as the helper panel's button does.
    pub(super) fn stop_helper(&mut self, sid: u64) {
        use marspot::shell_proto::{encode_helper_panel_act, CwdControl, HelperPanelAct, PanelAct};
        let act = HelperPanelAct { sid, act: PanelAct::CloseHelper, want: CwdControl::Flip, parent: 0 };
        self.pending_to_shell.push((MsgType::HelperPanelAct, encode_helper_panel_act(&act)));
    }
}

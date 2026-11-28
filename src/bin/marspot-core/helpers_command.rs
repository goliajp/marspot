//! The menu bar's Helpers command: the helper panel, from the keyboard
//! or the menu bar, whatever the window shows.  The strip, the badge's
//! menu and the sidebar's Helpers row each open the panel only in some
//! states (a strip with no overflow row, a collapsed sidebar, a badge
//! with no menu); this works in all of them.

use super::*;

impl CoreApp {
    /// Open the helper panel in window `wi` for the pane in use, or
    /// close it when it is open.  The panel lists the helpers of the
    /// pane at the top of the focused pane's family (the focused pane
    /// itself when it is a top one); with none there, every pane's.
    pub(super) fn helpers_command(&mut self, wi: usize) {
        if win!(self, wi).panels.open() == Some(SurfaceKind::Helpers) {
            self.toggle_panel(wi, SurfaceKind::Helpers);
            return;
        }
        if !self.pane_control.shadows.family.counted() {
            self.shadows_changed();
        }
        let fam = &self.pane_control.shadows.family;
        let root = self.focused_session(wi).map(|sid| fam.root(sid));
        let of = root.filter(|r| fam.count(*r) > 0).unwrap_or(0);
        self.open_helpers(wi, of);
        lx_event!("HELPERS_COMMAND", "the helper panel opened from the menu bar or its chord",
            window_id = win!(self, wi).window_id, of = of);
    }
}

#[cfg(test)]
#[path = "helpers_command_tests.rs"]
mod tests;

//! The size of a helper's terminal: that of the pane it is listed
//! under, with the strip it will be listed in taken out.

use super::height::rows_for;
use super::*;
use crate::shadows::{DEFAULT_COLS, DEFAULT_ROWS};
use marspot::layout::STRIP_MIN_TOTAL_ROWS;
use marspot::shell_proto::PaneSpawn;

impl CoreApp {
    /// The size a new pane is opened at.  A helper's, when none is
    /// given, is the core's to choose; any other's is the old default.
    pub(in super::super) fn spawn_size(&self, m: &PaneSpawn) -> (u16, u16) {
        match (m.cols, m.rows) {
            (0, 0) if m.helper => self.helper_size(m.parent_sid).unwrap_or((DEFAULT_COLS, DEFAULT_ROWS)),
            (0, 0) => (DEFAULT_COLS, DEFAULT_ROWS),
            (c, r) => (c, r),
        }
    }

    /// The size a helper opened without one gets: that of the terminal
    /// of the pane it is under, once the strip has its row too.
    pub(in super::super) fn helper_size(&self, parent: u64) -> Option<(u16, u16)> {
        if !self.strips.on {
            return None;
        }
        self.helper_block(self.pane_control.shadows.family.root(parent), 1)
    }

    /// The terminal of `root`'s pane as it will be with `more` helpers
    /// listed besides those that are: the strip does not grow under the
    /// helpers already sized for it.
    pub(super) fn helper_block(&self, root: u64, more: usize) -> Option<(u16, u16)> {
        let (wi, i) = self.find_window_pane_by_sid(root)?;
        let c = self.windows[wi].layout.cells.get(i)?;
        let total = c.rows + c.strip_rows;
        let record = self.pane_control.strips.of(root);
        let host = self.strips.hosts.get(&root);
        let listed = record.map_or(0, |r| r.rows.iter().filter(|r| host.is_none_or(|h| h.shows(r.sid))).count());
        let inline = self.strip_inline(root, Some(&self.windows[wi].panes[i]), record);
        let to_come = rows_for(listed + more, host.is_some_and(|h| h.merge.merged()), inline);
        // on the main screen the strip does not give rows back
        let to_come = if inline { to_come.max(c.strip_rows) } else { to_come };
        let strip = if total < STRIP_MIN_TOTAL_ROWS || to_come >= total { 0 } else { to_come };
        Some((c.cols, total - strip))
    }

}

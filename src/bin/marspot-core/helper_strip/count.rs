//! The `+N` after a pane's title: how many of its strip's rows are
//! panes still open, counted from the record the strip is drawn from so
//! that the two cannot disagree.

use super::*;

impl CoreApp {
    /// How many shadow panes the title of pane `sid` counts.  Until the
    /// shell has said what its strips list, and from a shell that never
    /// does, it is the shadows' own count.
    pub(in super::super) fn shadows_counted(&self, sid: u64) -> u32 {
        let records = &self.pane_control.strips;
        let Some(record) = records.of(sid) else {
            return if records.all_told() { 0 } else { self.pane_control.shadows.family.count(sid) };
        };
        let host = self.strips.hosts.get(&sid);
        record.rows.iter().filter(|r| !r.gone && host.is_none_or(|h| h.shows(r.sid))).count() as u32
    }

    /// Take what the shell said of a pane's helpers.  The title's `+N`
    /// is drawn from it, so a window whose count changed wants a frame:
    /// with the strip turned off nothing else would draw one.
    pub(in super::super) fn take_helper_strip(&mut self, m: marspot::shell_proto::HelperStrip) {
        let (root, all_told) = (m.root_sid, m.is_all_told());
        let before = self.shadows_counted(root);
        self.pane_control.strips.take(m);
        if all_told {
            for w in &mut self.windows {
                w.needs_render = true;
            }
        } else if self.shadows_counted(root) != before
            && let Some((wi, _)) = self.find_window_pane_by_sid(root)
        {
            win!(self, wi).needs_render = true;
        }
    }
}

/// Every pane the title of `root` counts is a row the strip reaches by
/// its place, and no other: what each strip test checks after the shell
/// spoke.
#[cfg(test)]
pub(super) fn assert_counted_as_reached(app: &CoreApp, root: u64) {
    let record = app.pane_control.strips.of(root);
    let reached = match (record, app.strips.hosts.get(&root)) {
        (Some(r), Some(h)) => (0..h.shown_rows(r)).filter_map(|i| h.sid_at(r, i)).filter(|sid| r.rows.iter().any(|x| x.sid == *sid && !x.gone)).count(),
        (Some(r), None) => r.rows.iter().filter(|x| !x.gone).count(),
        (None, _) => 0,
    };
    assert_eq!(app.shadows_counted(root) as usize, reached, "the title's count and the strip's rows of {root}");
}

#[cfg(test)]
#[path = "plain_tests.rs"]
mod plain_tests;

#[cfg(test)]
#[path = "mark_tests.rs"]
mod mark_tests;

//! The sidebar as a tree of panes, at both display scales: children
//! indented under their parent with a tree line, a hollow dot for a
//! pane in no window, a parent of another window as a grey row above
//! its child shown here, and a long name cut with its code whole.
//! Baselines `bench/ui/snapshots/chrome_sidebar_tree{,.2x}.png`, and
//! `chrome_sidebar_minimized{,.2x}.png` beside them.

use super::super::density_kit;
use super::{pane, snapshot, ROWS};
use crate::render::{SidebarEntry, SidebarTree};
use crate::session::SessionState;

fn row(label: &'static str, state: SessionState, code: u16, tree: SidebarTree, hollow: bool) -> SidebarEntry<'static> {
    SidebarEntry { tree, hollow, ..SidebarEntry::top(label, state, Some(code)) }
}

/// Four panes in the window; devops has a child in another window and
/// one in none, alt is the child of a pane of another window.
#[test]
pub(super) fn chrome_snapshot_sidebar_tree() {
    use SessionState::{Active, Exited, Idle};
    use SidebarTree::{Child, LastChild, Parent, Top};
    let rows = vec![
        SidebarEntry { selected: true, ..row("smix", Active, 4145, Top, false) },
        row("devops", Idle, 5528, Top, false),
        row("helper-with-a-name-longer-than-any-row-can-hold", Active, 7001, Child, false),
        row("madpdf", Idle, 8590, LastChild, true),
        row("insight-mono", Exited, 8654, Top, false),
        row("lab", Idle, 2210, Parent, false),
        row("alt", Idle, 6004, LastChild, false),
    ];
    let panes = [
        pane("smix", "", Active),
        pane("devops", "", Idle),
        pane("insight-mono", "", Exited),
        pane("alt", "", Idle),
    ];
    ROWS.with(|r| *r.borrow_mut() = Some(rows));
    snapshot("chrome_sidebar_tree", (1600, 1000), (2, 2), &panes, 0, |_, _, _| {});
}

#[test]
fn chrome_snapshot_sidebar_tree_2x() {
    density_kit::at_scale(2.0, chrome_snapshot_sidebar_tree);
}

/// Another window is minimized: the parent it shows, a grey row here,
/// and the child it shows are drawn hollow, the panes of this window
/// solid.  Baselines `chrome_sidebar_minimized{,.2x}.png`.
#[test]
pub(super) fn chrome_snapshot_sidebar_minimized() {
    use SessionState::{Active, Idle};
    use SidebarTree::{LastChild, Parent, Top};
    let rows = vec![
        row("lab", Idle, 2210, Parent, true),
        SidebarEntry { selected: true, ..row("smix", Active, 4145, LastChild, false) },
        row("devops", Idle, 5528, Top, false),
        row("madpdf", Active, 8590, LastChild, true),
    ];
    let panes = [pane("smix", "", Active), pane("devops", "", Idle)];
    ROWS.with(|r| *r.borrow_mut() = Some(rows));
    snapshot("chrome_sidebar_minimized", (1200, 640), (2, 1), &panes, 0, |_, _, _| {});
}

#[test]
fn chrome_snapshot_sidebar_minimized_2x() {
    density_kit::at_scale(2.0, chrome_snapshot_sidebar_minimized);
}

//! Pressing sidebar rows that are no slot of their window.

use super::*;
use crate::boot_assembly_tests::Sandbox;
use crate::draw::sidebar_tree::RowPane::{Other, Slot};
use crate::window_state_tests::{app_in, win};
use marspot::render::SidebarTree::{Child, LastChild, Parent, Top};

fn vacant(sid: u64) -> Pane {
    Pane::new_vacant(sid, 60, 16)
}

/// Window 1 holds 1 and 2, window 2 holds 3; 4 is in no window.  3 and
/// 4 are children of 2.
fn two_windows(sb: &Sandbox) -> CoreApp {
    let mut app = app_in(sb.dir(), vec![win(1, vec![vacant(1), vacant(2)]), win(2, vec![vacant(3)])]);
    app.shadows.push(vacant(4));
    app.pane_control.shadows.family.adopted(3, 2);
    app.pane_control.shadows.family.adopted(4, 2);
    // a cell for each pane of window 1: one past its cells is off screen
    win!(app, 0).grid_cols = 2;
    for w in &mut app.windows {
        w.sidebar_collapsed = false;
    }
    app.rebuild_layout(0);
    app.rebuild_layout(1);
    app
}

fn rows(app: &mut CoreApp, wi: usize) -> Vec<(super::RowPane, marspot::render::SidebarTree, bool)> {
    app.sidebar_fresh(wi);
    app.sidebar_rows(wi).iter().map(|r| (r.pane, r.tree, r.hollow)).collect()
}

fn raised(app: &CoreApp) -> Vec<u32> {
    let raise = app.pending_to_shell.iter().filter(|(t, _)| *t == MsgType::WindowRaise);
    raise.map(|(_, p)| marspot::shell_proto::decode_window_raise(p).unwrap()).collect()
}

#[test]
fn sidebar_rows_list_children_anywhere_and_the_layout_has_a_row_for_each() {
    let sb = Sandbox::new();
    let mut app = two_windows(&sb);
    assert_eq!(rows(&mut app, 0), vec![(Slot(0), Top, false), (Slot(1), Top, false), (Other(3), Child, false), (Other(4), LastChild, true)]);
    assert_eq!(win!(app, 0).layout.close_session_rects.len(), 4, "a close spot per row, not per slot");
    assert_eq!(rows(&mut app, 1), vec![(Other(2), Parent, false), (Slot(0), LastChild, false)]);
    assert_eq!(win!(app, 1).layout.close_session_rects.len(), 2);
}

#[test]
fn sidebar_row_of_another_window_raises_it_and_focuses_the_pane() {
    let sb = Sandbox::new();
    let mut app = two_windows(&sb);
    rows(&mut app, 0);
    app.sidebar_row_pressed(0, 2);
    assert_eq!(raised(&app), vec![2]);
    assert_eq!((app.key_window, win!(app, 1).focused_idx), (1, 0));

    // the grey row of window 2 goes back to the parent in window 1
    app.pending_to_shell.clear();
    rows(&mut app, 1);
    app.sidebar_row_pressed(1, 0);
    assert_eq!(raised(&app), vec![1]);
    assert_eq!((app.key_window, win!(app, 0).focused_idx), (0, 1));
}

#[test]
fn sidebar_row_of_a_hidden_pane_shows_it_beside_the_pane_in_use() {
    let sb = Sandbox::new();
    let mut app = two_windows(&sb);
    win!(app, 0).focused_idx = 0;
    rows(&mut app, 0);
    app.sidebar_row_pressed(0, 3);
    assert!(app.shadows.is_empty(), "4 is in a window now");
    assert_eq!(app.find_window_pane_by_sid(4), Some((0, 1)), "next to the pane in use");
    assert_eq!(win!(app, 0).focused_idx, 1);
    assert_eq!(app.pane_control.shadows.family.parents.get(&4), Some(&2), "still 2's child");
    assert!(raised(&app).is_empty(), "nothing to raise: it is shown here");
    assert_eq!(
        rows(&mut app, 0),
        vec![(Slot(0), Top, false), (Slot(2), Top, false), (Other(3), Child, false), (Slot(1), LastChild, false)],
        "solid now, still under its parent"
    );
}

#[test]
fn sidebar_close_of_a_hidden_child_closes_it_and_a_grey_row_closes_nothing() {
    let sb = Sandbox::new();
    let mut app = two_windows(&sb);
    rows(&mut app, 1);
    app.close_row(1, 0);
    assert!(app.find_window_pane_by_sid(2).is_some(), "the grey row's parent is not closed");
    rows(&mut app, 0);
    app.close_row(0, 3);
    assert!(app.shadows.is_empty(), "the hidden child is closed");
    assert_eq!(rows(&mut app, 0).len(), 3);
}

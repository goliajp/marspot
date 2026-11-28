//! A pane shown only in a minimized window is drawn hollow.

use super::*;
use crate::boot_assembly_tests::Sandbox;
use crate::draw::sidebar_tree::RowPane::{self, Other, Slot};
use crate::window_state_tests::{app_in, win};
use marspot::render::SidebarTree::{self, LastChild, Parent, Top};

fn vacant(sid: u64) -> Pane {
    Pane::new_vacant(sid, 60, 16)
}

fn rows(app: &mut CoreApp, wi: usize) -> Vec<(RowPane, SidebarTree, bool)> {
    app.sidebar_fresh(wi);
    app.sidebar_rows(wi).iter().map(|r| (r.pane, r.tree, r.hollow)).collect()
}

/// Window 1 holds 1 and 2, window 2 holds 3 and 5; 3 is 2's child and
/// 1 is 5's.
fn two_windows(sb: &Sandbox) -> CoreApp {
    let mut app = app_in(sb.dir(), vec![win(1, vec![vacant(1), vacant(2)]), win(2, vec![vacant(3), vacant(5)])]);
    app.pane_control.shadows.family.adopted(3, 2);
    app.pane_control.shadows.family.adopted(1, 5);
    for wi in 0..2 {
        win!(app, wi).grid_cols = 2;
        win!(app, wi).sidebar_collapsed = false;
        app.rebuild_layout(wi);
    }
    app
}

/// The shell's frame, through the core's own decoding.
fn minimized(app: &mut CoreApp, window_id: u32, on: bool) {
    let frame = marspot::shell_proto::encode_window_minimized(window_id, on);
    match crate::shell_reader::decode_frame(MsgType::WindowMinimized, &frame) {
        Some(CoreEvent::WindowMinimized(w, m)) => app.window_minimized(w, m),
        _ => panic!("the frame decodes to a minimized event"),
    }
}

#[test]
fn a_pane_shown_only_in_a_minimized_window_is_hollow_until_it_comes_back() {
    let sb = Sandbox::new();
    let mut app = two_windows(&sb);
    let solid = vec![(Other(5), Parent, false), (Slot(0), LastChild, false), (Slot(1), Top, false), (Other(3), LastChild, false)];
    assert_eq!(rows(&mut app, 0), solid);

    minimized(&mut app, 2, true);
    assert_eq!(
        rows(&mut app, 0),
        vec![(Other(5), Parent, true), (Slot(0), LastChild, false), (Slot(1), Top, false), (Other(3), LastChild, true)],
        "the grey parent and the child shown in window 2 are on no screen"
    );
    assert_eq!(
        rows(&mut app, 1),
        vec![(Other(2), Parent, false), (Slot(0), LastChild, true), (Slot(1), Top, true), (Other(1), LastChild, false)],
        "in its own list too, while the panes of window 1 stay solid"
    );

    minimized(&mut app, 2, false);
    assert_eq!(rows(&mut app, 0), solid, "solid again once it is back");
}

#[test]
fn minimized_windows_are_kept_by_id_once_each_until_they_close() {
    let sb = Sandbox::new();
    let mut app = two_windows(&sb);
    minimized(&mut app, 2, true);
    minimized(&mut app, 2, true);
    assert_eq!(app.pane_control.shadows.minimized, [2], "said twice, kept once");
    // a window the core has not taken on yet is kept for when it does
    minimized(&mut app, 9, true);
    assert_eq!(app.pane_control.shadows.minimized, [2, 9]);
    app.close_window(2);
    assert_eq!(app.pane_control.shadows.minimized, [9], "a closed window is forgotten");
    for id in 100..100 + MINIMIZED_CAP as u32 + 5 {
        minimized(&mut app, id, true);
    }
    assert_eq!(app.pane_control.shadows.minimized.len(), MINIMIZED_CAP);
}

#[test]
fn a_pane_in_a_minimized_window_is_not_shown_so_it_is_offered_to_replace_with() {
    let sb = Sandbox::new();
    let mut app = two_windows(&sb);
    assert!(app.not_shown(8).is_empty());
    minimized(&mut app, 2, true);
    assert_eq!(app.not_shown(8), [3, 5], "the panes of the minimized window, and only those");
    assert!(!app.on_visible_screen(3), "its dot is hollow");
    assert!(app.on_visible_screen(1));
}

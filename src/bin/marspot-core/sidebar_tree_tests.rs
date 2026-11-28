use super::*;
use std::collections::HashMap;

use RowPane::{Other, Slot};
use SidebarTree::{Child, LastChild, Parent, Top};

/// The rows of a window holding `here`, with every pane in `all`, of
/// these parents, on screen when in `shown`.
fn rows(here: &[Option<u64>], all: &[u64], parents: &[(u64, u64)], shown: &[u64]) -> Vec<(RowPane, SidebarTree, bool)> {
    let parents: HashMap<u64, u64> = parents.iter().copied().collect();
    build(here, all, |s| parents.get(&s).copied(), |s| shown.contains(&s))
        .into_iter()
        .map(|r| (r.pane, r.tree, r.hollow))
        .collect()
}

#[test]
fn sidebar_tree_lists_each_child_under_its_parent_shown_anywhere_or_not() {
    // window: 1, 2, 3; 5 is 2's child in no window, 6 is 2's child in
    // another window, 3 is 2's child shown here
    let here = [Some(1), Some(2), Some(3)];
    let all = [1, 2, 3, 5, 6, 9];
    let got = rows(&here, &all, &[(5, 2), (6, 2), (3, 2)], &[1, 2, 3, 6, 9]);
    assert_eq!(
        got,
        vec![
            (Slot(0), Top, false),
            (Slot(1), Top, false),
            (Slot(2), Child, false),
            (Other(5), Child, true),
            (Other(6), LastChild, false),
        ],
        "9, a top pane of another window, is not listed"
    );
}

#[test]
fn sidebar_tree_puts_a_grey_parent_row_above_children_whose_parent_is_elsewhere() {
    // 7 and 8 are children of 4, which is in another window; 9 is a
    // child of 4 shown in no window, so not here
    let here = [Some(1), Some(7), Some(2), Some(8)];
    let all = [1, 2, 4, 7, 8, 9];
    let got = rows(&here, &all, &[(7, 4), (8, 4), (9, 4)], &[1, 2, 4, 7, 8]);
    assert_eq!(
        got,
        vec![
            (Slot(0), Top, false),
            (Other(4), Parent, false),
            (Slot(1), Child, false),
            (Slot(3), LastChild, false),
            (Slot(2), Top, false),
        ]
    );
}

#[test]
fn sidebar_tree_marks_a_pane_off_screen_hollow_and_keeps_placeholders() {
    // 2 is in the window but past its cells; slot 1 is a placeholder;
    // 4's parent 3 is hidden
    let here = [Some(1), None, Some(2), Some(4)];
    let got = rows(&here, &[1, 2, 3, 4], &[(4, 3)], &[1, 4]);
    assert_eq!(
        got,
        vec![(Slot(0), Top, false), (Slot(1), Top, false), (Slot(2), Top, true), (Other(3), Parent, true), (Slot(3), LastChild, false)]
    );
}

#[test]
fn sidebar_tree_stops_at_its_cap() {
    let here: Vec<Option<u64>> = (1..=40).map(Some).collect();
    // every pane of the window has four children in no window
    let kids: Vec<(u64, u64)> = (1..=40u64).flat_map(|p| (0..4).map(move |k| (1000 + p * 10 + k, p))).collect();
    let all: Vec<u64> = (1..=40).chain(kids.iter().map(|k| k.0)).collect();
    let got = rows(&here, &all, &kids, &[]);
    assert_eq!(got.len(), ROW_CAP);
    assert_eq!(got[0], (Slot(0), Top, true));
    assert_eq!(got[4], (Other(1013), LastChild, true));
}

/// A frame with nothing new keeps the rows it had; a child that comes
/// is listed at once.
#[test]
fn sidebar_tree_is_built_again_only_when_what_it_lists_changes() {
    use crate::boot_assembly_tests::Sandbox;
    use crate::window_state_tests::{app_in, win};
    let sb = Sandbox::new();
    let vacant = |sid| Pane::new_vacant(sid, 60, 16);
    let mut app = app_in(sb.dir(), vec![win(1, vec![vacant(1), vacant(2)])]);
    app.rebuild_layout(0);
    let kept = |app: &CoreApp| app.pane_control.shadows.trees.0.iter().map(|t| (t.1, t.2.as_ptr())).collect::<Vec<_>>();
    app.refresh_sidebar_tree(0);
    let first = kept(&app);
    for _ in 0..3 {
        app.refresh_sidebar_tree(0);
    }
    assert_eq!(kept(&app), first, "worked out again with nothing changed");
    app.shadows.push(vacant(3));
    app.pane_control.shadows.family.adopted(3, 1);
    app.refresh_sidebar_tree(0);
    assert_eq!(app.sidebar_rows(0).len(), 3);
    assert_eq!(app.pane_control.shadows.trees.0.len(), 1, "one entry per window");
}

//! Window 2 as the user had it: a 4×1 grid holding seven panes, the
//! last three (madpdf, madpdf, alt-registry) in slots past the grid's
//! cells, listed in the sidebar and drawn nowhere.

use super::*;
use crate::boot_assembly_tests::Sandbox;
use crate::draw::sidebar_tree::RowPane;
use crate::context_menu::slot_replace::{REPLACE_TAG_BASE, Replace};
use crate::window_state_tests::{app_in, win};

const SMIX: u64 = 4145;
const SEVEN: [u64; 7] = [SMIX, 5528, 8654, 6004, 8590, 7009, 9898];
const UNDRAWN: [u64; 3] = [8590, 7009, 9898];

fn vacant(sid: u64) -> Pane {
    Pane::new_vacant(sid, 60, 16)
}

/// Each row of window `wi`'s sidebar: its pane and whether it is hollow.
fn dots(app: &mut CoreApp, wi: usize) -> Vec<(u64, bool)> {
    app.sidebar_fresh(wi);
    let rows: Vec<_> = app.sidebar_rows(wi).to_vec();
    rows.iter()
        .map(|r| match r.pane {
            RowPane::Slot(i) => (win!(app, wi).panes[i].shelld_session_id().unwrap_or(0), r.hollow),
            RowPane::Other(s) => (s, r.hollow),
        })
        .collect()
}

fn sids(app: &CoreApp, wi: usize) -> Vec<u64> {
    win!(app, wi).panes.iter().filter_map(Pane::shelld_session_id).collect()
}

/// Window 1 holds one pane; window 2 is 4×1 with seven, so slots 4, 5
/// and 6 have no cell.
fn user_layout(sb: &Sandbox) -> CoreApp {
    let mut app = app_in(sb.dir(), vec![win(1, vec![vacant(1)]), win(2, SEVEN.iter().map(|s| vacant(*s)).collect())]);
    win!(app, 1).grid_cols = 4;
    win!(app, 1).grid_rows = 1;
    for wi in 0..2 {
        win!(app, wi).sidebar_collapsed = false;
        app.rebuild_layout(wi);
    }
    app
}

#[test]
fn drawn_slots_past_the_cells_are_hollow_and_the_drawn_four_solid() {
    let sb = Sandbox::new();
    let mut app = user_layout(&sb);
    assert_eq!(win!(app, 1).layout.cells.len(), 4);
    let want: Vec<(u64, bool)> = SEVEN.iter().map(|s| (*s, UNDRAWN.contains(s))).collect();
    assert_eq!(dots(&mut app, 1), want);
    for s in UNDRAWN {
        assert!(!app.on_visible_screen(s), "{s} has a slot but no cell");
    }
}

#[test]
fn drawn_replace_with_on_smix_offers_the_three_undrawn_and_swaps_slots() {
    let sb = Sandbox::new();
    let mut app = user_layout(&sb);
    let menu = app.replace_row();
    assert_eq!(app.pane_control.shadows.menu_hidden, UNDRAWN, "the three panes not drawn are offered");
    assert_eq!(menu.submenu.len(), 2 + UNDRAWN.len(), "new shell, a divider and the three");

    // the pick of madpdf (#8590) on smix's slot
    let tag = REPLACE_TAG_BASE;
    assert!(app.pick_replace(1, ContextRegion::Pane(0), tag));
    assert_eq!(sids(&app, 1), [8590, 5528, 8654, 6004, SMIX, 7009, 9898], "the two changed places, no pane left the window");
    assert_eq!(sids(&app, 0), [1]);
    assert_eq!(app.focused_session(1), Some(8590), "the keyboard follows the slot");
    assert!(app.shadows.is_empty(), "nothing went to the hidden panes");
    let d = dots(&mut app, 1);
    assert!(d.contains(&(8590, false)) && d.contains(&(SMIX, true)), "the dots swapped: {d:?}");
}

#[test]
fn drawn_replace_from_another_window_swaps_across_windows() {
    let sb = Sandbox::new();
    let mut app = user_layout(&sb);
    app.replace_slot(0, 0, Replace::Hidden(9898));
    assert_eq!(sids(&app, 0), [9898]);
    assert_eq!(sids(&app, 1), [SMIX, 5528, 8654, 6004, 8590, 7009, 1]);
    assert!(app.on_visible_screen(9898) && !app.on_visible_screen(1));
}

#[test]
fn drawn_press_on_an_undrawn_row_gives_it_a_cell_and_keeps_the_drawn_ones() {
    let sb = Sandbox::new();
    let mut app = user_layout(&sb);
    app.sidebar_fresh(1);
    let row = app.sidebar_rows(1).iter().position(|r| r.pane == RowPane::Slot(5)).expect("7009's row");
    app.sidebar_row_pressed(1, row);
    assert_eq!(sids(&app, 1), [SMIX, 7009, 5528, 8654, 6004, 8590, 9898], "beside the pane in use");
    assert_eq!((win!(app, 1).grid_cols, win!(app, 1).grid_rows), (5, 1), "the grid grew a column");
    assert_eq!(app.focused_session(1), Some(7009));
    for s in [SMIX, 7009, 5528, 8654, 6004] {
        assert!(app.on_visible_screen(s), "{s} is drawn");
    }
}

#[test]
fn drawn_press_in_another_window_brings_the_undrawn_pane_here() {
    let sb = Sandbox::new();
    let mut app = user_layout(&sb);
    app.reach_pane(0, 9898);
    assert_eq!(sids(&app, 0), [1, 9898]);
    assert_eq!(sids(&app, 1), [SMIX, 5528, 8654, 6004, 8590, 7009]);
    assert!(app.on_visible_screen(9898));
}

#[test]
fn drawn_taking_the_focused_last_undrawn_pane_leaves_its_window_a_focus() {
    let sb = Sandbox::new();
    let mut app = user_layout(&sb);
    win!(app, 1).focused_idx = 6;
    app.reach_pane(0, 9898);
    assert_eq!(sids(&app, 1), [SMIX, 5528, 8654, 6004, 8590, 7009]);
    assert_eq!(win!(app, 1).focused_idx, 5, "the window keeps a pane to type into");
    assert_eq!(app.focused_session(1), Some(7009));
}

#[test]
fn drawn_a_pane_put_into_a_full_grid_is_drawn() {
    let sb = Sandbox::new();
    let mut app = user_layout(&sb);
    let w = &win!(app, 1);
    let first_undrawn = w.panes.len().min(w.grid_cols * w.grid_rows);
    app.insert_drawn(1, first_undrawn, vacant(4242));
    assert_eq!(win!(app, 1).panes[4].shelld_session_id(), Some(4242));
    assert!(app.on_visible_screen(4242));
    assert_eq!(app.focused_session(1), Some(SMIX), "putting a pane in takes no keyboard");
}

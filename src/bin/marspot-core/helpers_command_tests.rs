//! The Helpers command opens the helper panel in every state of the
//! window, and the badge's menu offers the panel even when its plugin
//! has no rows.

use super::*;
use crate::boot_assembly_tests::Sandbox;
use crate::context_menu::menu_pick::tests::app_of;
use marspot::commands::{for_key, row_of};
use marspot::input::KeyState;

/// Window 1 holds 1 and 2; 3 and 4, in no window, are 1's children.
/// The sidebar is collapsed, so its Helpers row is not on screen.
fn with_helpers(sb: &Sandbox) -> CoreApp {
    let mut app = app_of(sb, &[1, 2]);
    for c in [3, 4] {
        app.shadows.push(Pane::new_vacant(c, 60, 16));
        app.pane_control.shadows.family.adopted(c, 1);
    }
    app.shadows_changed();
    win!(app, 0).sidebar_collapsed = true;
    app.rebuild_layout(0);
    app
}

/// The command's chord, as the menu bar replays it when its item is
/// clicked.
fn chord(app: &mut CoreApp) {
    let c = row_of(Command::Helpers).chord.expect("the command has a chord");
    let (event, mods) = (c.key_event(), c.modifiers());
    assert_eq!(for_key(&event, mods).map(|r| r.command), Some(Command::Helpers));
    for state in [KeyState::Pressed, KeyState::Released] {
        app.key(0, MarspotKeyEvent { state, ..event.clone() }, mods);
    }
}

fn panel_of(app: &CoreApp) -> Option<u64> {
    win!(app, 0).helpers_panel.as_ref().map(|p| p.of)
}

#[test]
fn helpers_command_opens_the_panel_for_the_focused_panes_family_and_closes_it() {
    let sb = Sandbox::new();
    let mut app = with_helpers(&sb);
    win!(app, 0).focused_idx = 0;
    chord(&mut app);
    assert_eq!(win!(app, 0).panels.open(), Some(SurfaceKind::Helpers), "with the sidebar collapsed");
    assert_eq!(panel_of(&app), Some(1), "of the pane in use");
    chord(&mut app);
    assert_eq!(win!(app, 0).panels.open(), None, "the second time closes it");

    // a pane with no helpers of its own: every pane's
    win!(app, 0).focused_idx = 1;
    chord(&mut app);
    assert_eq!(panel_of(&app), Some(0));
    app.toggle_panel(0, SurfaceKind::Helpers);

    // a child shown in the window: its parent's
    app.pane_control.shadows.family.adopted(2, 1);
    app.shadows_changed();
    chord(&mut app);
    assert_eq!(panel_of(&app), Some(1));
}

#[test]
fn helpers_command_opens_the_panel_over_another_panel_and_with_no_helpers_at_all() {
    let sb = Sandbox::new();
    let mut app = app_of(&sb, &[1]);
    app.toggle_panel(0, SurfaceKind::Shortcuts);
    chord(&mut app);
    assert_eq!(win!(app, 0).panels.open(), Some(SurfaceKind::Helpers), "taking another panel's place");
    assert_eq!(panel_of(&app), Some(0), "and saying none are running");
}

#[test]
fn badge_menu_with_no_plugin_rows_still_offers_the_helpers() {
    let sb = Sandbox::new();
    let mut app = with_helpers(&sb);
    app.open_pane_badge_menu(0, 1, 10.0, 10.0, Vec::new());
    let items = win!(app, 0).context_menu.as_ref().expect("a menu, of the core's own row").items.clone();
    assert_eq!(items.iter().map(|i| i.action_tag).collect::<Vec<_>>(), [helper_panel::MENU_TAG]);
    app.pick_badge_row(0, 1, helper_panel::MENU_TAG);
    assert_eq!(panel_of(&app), Some(1));

    // a pane with no helpers and nothing to go back from: no menu
    app.toggle_panel(0, SurfaceKind::Helpers);
    app.open_pane_badge_menu(0, 2, 10.0, 10.0, Vec::new());
    assert!(win!(app, 0).context_menu.is_none());
}

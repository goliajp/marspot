//! The keyboard on an open context menu.

use super::*;
use crate::boot_assembly_tests::Sandbox;
use crate::window_state_tests::{app_in, win};
use marspot::input::{KeyState, LogicalKey, NamedKey};

pub(crate) fn app_of(sb: &Sandbox, sids: &[u64]) -> CoreApp {
    let panes = sids.iter().map(|s| Pane::new_vacant(*s, 60, 16)).collect();
    let mut app = app_in(sb.dir(), vec![win(1, panes)]);
    win!(app, 0).sidebar_collapsed = false;
    win!(app, 0).grid_cols = sids.len().max(1);
    app.rebuild_layout(0);
    app
}

/// Where row `i` of window 0's sidebar is.
pub(crate) fn row_point(app: &CoreApp, i: usize) -> (f64, f64) {
    let l = &win!(app, 0).layout;
    let h = marspot_term::layout::sidebar_row_h();
    (l.sidebar_w * 0.25, l.top_inset + l.sidebar_top_pad_phys + (i as f64 + 0.5) * h)
}

pub(crate) fn press(app: &mut CoreApp, logical: LogicalKey) {
    for state in [KeyState::Pressed, KeyState::Released] {
        app.key(0, MarspotKeyEvent { state, logical: logical.clone(), ..Default::default() }, Modifiers::default());
    }
}

#[test]
fn menu_keys_move_down_the_menu_and_enter_picks_the_row() {
    let sb = Sandbox::new();
    let mut app = app_of(&sb, &[1, 2]);
    let (x, y) = row_point(&app, 1);
    app.mouse_right_down(0, x, y, Modifiers::default());
    assert!(win!(app, 0).context_menu.is_some(), "the row's menu opened");
    // a key that types something stays with the menu
    press(&mut app, LogicalKey::Char('x'));
    assert!(win!(app, 0).context_menu.is_some());
    press(&mut app, LogicalKey::Named(NamedKey::ArrowDown));
    press(&mut app, LogicalKey::Named(NamedKey::Enter));
    assert!(win!(app, 0).context_menu.is_none(), "picked, and closed");
    assert_eq!(app.find_window_pane_by_sid(2), None, "its first row closed the pane");
    assert_eq!(win!(app, 0).panes.len(), 1);

    app.mouse_right_down(0, x - 1.0, row_point(&app, 0).1, Modifiers::default());
    assert!(win!(app, 0).context_menu.is_some());
    press(&mut app, LogicalKey::Named(NamedKey::Escape));
    assert!(win!(app, 0).context_menu.is_none(), "Esc closes it");
    assert_eq!(win!(app, 0).panes.len(), 1, "and does nothing else");
}

fn parent_asked(app: &CoreApp) -> Vec<(u64, u64)> {
    let acts = app.pending_to_shell.iter().filter(|(t, _)| *t == MsgType::HelperPanelAct);
    acts.map(|(_, p)| marspot::shell_proto::decode_helper_panel_act(p).unwrap())
        .filter(|a| a.act == marspot::shell_proto::PanelAct::SetParent)
        .map(|a| (a.sid, a.parent))
        .collect()
}

/// From a sidebar row's menu: a top-level pane is made another's child
/// through the menu beside, a child is made top-level, and a pane with
/// children is offered neither; each pick goes to the shell's rule check.
#[test]
fn menu_kin_rows_ask_the_shell_to_change_whose_child_a_pane_is() {
    let sb = Sandbox::new();
    let mut app = app_of(&sb, &[1, 2]);
    let (x, y) = row_point(&app, 1);
    app.mouse_right_down(0, x, y, Modifiers::default());
    let items = win!(app, 0).context_menu.as_ref().unwrap().items.clone();
    let belongs = items.iter().find(|i| i.label == strings::t(Msg::MenuBelongsTo)).expect("the row");
    assert_eq!(belongs.submenu.len(), 1, "pane 1 is the one to belong to");
    for k in [NamedKey::ArrowDown, NamedKey::ArrowDown, NamedKey::ArrowRight, NamedKey::Enter] {
        press(&mut app, LogicalKey::Named(k));
    }
    assert_eq!(parent_asked(&app), vec![(2, 1)]);
    assert!(win!(app, 0).context_menu.is_none());

    // the shell said yes: 2 is 1's child, and 1's own menu cannot make 1 a child
    app.pane_control.shadows.family.adopted(2, 1);
    app.shadows_changed();
    app.pending_to_shell.clear();
    let rows = app.sidebar_rows(0).len();
    assert_eq!(rows, 2, "2 is listed under 1");
    app.mouse_right_down(0, x, row_point(&app, 0).1, Modifiers::default());
    let items = win!(app, 0).context_menu.as_ref().unwrap().items.clone();
    let off = items.iter().find(|i| i.label == strings::t(Msg::MenuBelongsTo)).expect("the row, off");
    assert!(!off.enabled && off.tooltip == strings::t(Msg::MenuBelongsToBlocked));
    press(&mut app, LogicalKey::Named(NamedKey::Escape));

    app.mouse_right_down(0, x, y, Modifiers::default());
    let items = win!(app, 0).context_menu.as_ref().unwrap().items.clone();
    let at = items.iter().position(|i| i.label == strings::t(Msg::MenuMakeTopLevel)).expect("the way back");
    app.pick_menu_tag(0, items[at].action_tag);
    assert_eq!(parent_asked(&app), vec![(2, 0)]);
}

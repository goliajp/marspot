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

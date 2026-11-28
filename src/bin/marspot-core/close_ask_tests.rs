//! The question before closing a pane with child panes.

use super::*;
use marspot::input::{LogicalKey, NamedKey};
use crate::boot_assembly_tests::Sandbox;
use crate::context_menu::menu_pick::tests::{app_of, press};

/// Window 1 holds 1 and 2; 3, in no window, is 1's only child.
fn with_hidden_child(sb: &Sandbox) -> CoreApp {
    let mut app = app_of(sb, &[1, 2]);
    app.shadows.push(Pane::new_vacant(3, 60, 16));
    app.pane_control.shadows.family.adopted(3, 1);
    app.shadows_changed();
    app.sidebar_fresh(0);
    app
}

fn asked(app: &CoreApp) -> Option<(u64, u64)> {
    app.notices.close.as_ref().map(|a| (a.sid, a.over))
}

#[test]
fn close_of_a_pane_whose_only_child_is_hidden_asks_and_esc_changes_nothing() {
    let sb = Sandbox::new();
    let mut app = with_hidden_child(&sb);
    // row 0 is pane 1, row 1 its hidden child, row 2 pane 2
    app.close_row(0, 0);
    assert_eq!(asked(&app), Some((1, 1)), "asked, over the pane itself");
    let text = app.notices.close.as_ref().unwrap().text.clone();
    assert!(text.starts_with(t(Msg::CloseChildrenAsk)), "{text:?}");
    assert_eq!(app.notices.over(Some(1), false), Some(text.as_str()), "shown over pane 1 whether in use or not");
    assert_eq!(app.notices.over(Some(2), true), None);
    assert!(app.find_window_pane_by_sid(1).is_some(), "nothing closed yet");

    press(&mut app, LogicalKey::Char('x'));
    assert!(asked(&app).is_some(), "a key that types stays with the question");
    press(&mut app, LogicalKey::Named(NamedKey::Escape));
    assert_eq!(asked(&app), None);
    assert!(app.find_window_pane_by_sid(1).is_some() && app.shadow_index(3).is_some(), "nothing changed");
    assert_eq!(app.pane_control.shadows.family.parents.get(&3), Some(&1));

    app.close_row(0, 0);
    press(&mut app, LogicalKey::Named(NamedKey::Enter));
    assert_eq!(asked(&app), None);
    assert_eq!(app.find_window_pane_by_sid(1), None, "Enter closed it");
}

#[test]
fn close_question_buttons_answer_and_a_press_elsewhere_leaves_the_pane() {
    let sb = Sandbox::new();
    let mut app = with_hidden_child(&sb);
    let mid = |r: marspot_term::layout::Rect| (r.x + r.w * 0.5, r.y_top + r.h * 0.5);

    app.close_row(0, 0);
    let [_, stay] = app.close_ask_buttons(0, 1).expect("the buttons are over pane 1");
    let (x, y) = mid(stay);
    assert!(app.close_ask_press(0, x, y), "the press is the question's");
    assert!(asked(&app).is_none() && app.find_window_pane_by_sid(1).is_some(), "the second button leaves it");

    app.close_row(0, 0);
    assert!(!app.close_ask_press(0, 1.0, 1.0), "a press elsewhere goes on");
    assert!(asked(&app).is_none() && app.find_window_pane_by_sid(1).is_some());

    app.close_row(0, 0);
    let [go, _] = app.close_ask_buttons(0, 1).expect("the buttons");
    let (x, y) = mid(go);
    assert!(app.close_ask_press(0, x, y));
    assert_eq!(app.find_window_pane_by_sid(1), None, "the first button closes it");
}

#[test]
fn close_of_a_pane_without_children_asks_nothing() {
    let sb = Sandbox::new();
    let mut app = with_hidden_child(&sb);
    app.close_row(0, 2);
    assert_eq!(asked(&app), None);
    assert_eq!(app.find_window_pane_by_sid(2), None, "closed at once");
}

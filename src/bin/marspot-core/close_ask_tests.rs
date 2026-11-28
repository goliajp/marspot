//! The question before closing a pane with child panes.

use super::*;
use marspot::input::{LogicalKey, NamedKey};
use crate::boot_assembly_tests::Sandbox;
use crate::context_menu::menu_pick::tests::{app_of, press};
use crate::window_state_tests::{app_in, win};
use marspot::input::KeyState;

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
    let id = win!(app, 0).window_id;
    assert_eq!(app.notices.over(id, Some(1), false), Some(text.as_str()), "shown over pane 1 whether in use or not");
    assert_eq!(app.notices.over(id, Some(2), true), None);
    assert_eq!(app.notices.over(id + 1, Some(1), false), None, "and in its own window only");
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

/// Whether the question took a key pressed in window `wi`; one it does
/// not take goes on to that window as if it were not up.
fn takes_key(app: &mut CoreApp, wi: usize, logical: LogicalKey) -> bool {
    app.close_ask_key(wi, &MarspotKeyEvent { state: KeyState::Pressed, logical, ..Default::default() })
}

/// Window 1 holds 1 and 2, window 2 holds 3 and 5; 3 and 4, in no
/// window, are children of 1.
fn parent_in_another_window(sb: &Sandbox) -> CoreApp {
    let v = |s| Pane::new_vacant(s, 60, 16);
    let mut app = app_in(sb.dir(), vec![win(1, vec![v(1), v(2)]), win(2, vec![v(3), v(5)])]);
    app.shadows.push(v(4));
    app.pane_control.shadows.family.adopted(3, 1);
    app.pane_control.shadows.family.adopted(4, 1);
    app.shadows_changed();
    for wi in 0..2 {
        win!(app, wi).grid_cols = 2;
        app.rebuild_layout(wi);
    }
    app
}

#[test]
fn close_asked_from_another_window_stands_there_and_takes_only_its_keys() {
    let sb = Sandbox::new();
    let mut app = parent_in_another_window(&sb);
    let (a, b) = (win!(app, 0).window_id, win!(app, 1).window_id);
    // asked from window 2, where pane 1 is not shown: over the pane in use there
    assert!(app.close_asked(1, 1));
    assert_eq!(asked(&app), Some((1, 3)));
    assert!(app.notices.over(b, Some(3), true).is_some(), "drawn where it was asked");
    assert_eq!(app.notices.over(a, Some(1), true), None, "not in the window that shows the pane");
    assert_eq!(app.close_ask_buttons(0, 3), None);

    assert!(!takes_key(&mut app, 0, LogicalKey::Named(NamedKey::Enter)), "a key in the other window is not the question's");
    assert!(asked(&app).is_some() && app.find_window_pane_by_sid(1).is_some(), "and closes nothing");
    assert!(takes_key(&mut app, 1, LogicalKey::Char('x')), "a key of its own window stays with it");
    assert!(asked(&app).is_some());
    assert!(takes_key(&mut app, 1, LogicalKey::Named(NamedKey::Escape)));
    assert_eq!(asked(&app), None);
    assert!(app.find_window_pane_by_sid(1).is_some());
}

#[test]
fn close_question_comes_down_when_what_it_is_about_goes() {
    let sb = Sandbox::new();
    // the pane it stands over leaves the window
    let mut app = parent_in_another_window(&sb);
    assert!(app.close_asked(1, 1));
    app.close_anywhere(3, false);
    app.notices_tick();
    assert_eq!(asked(&app), None, "its pane closed");
    assert!(!takes_key(&mut app, 1, LogicalKey::Named(NamedKey::Enter)), "a key afterwards is the window's");
    assert!(app.find_window_pane_by_sid(1).is_some(), "and closes nothing");

    // the pane to close ends
    let mut app = parent_in_another_window(&sb);
    assert!(app.close_asked(1, 1));
    app.close_anywhere(1, false);
    assert!(!takes_key(&mut app, 1, LogicalKey::Named(NamedKey::Enter)));
    assert_eq!(asked(&app), None, "a key finds it gone even before the tick");

    // its window closes
    let mut app = parent_in_another_window(&sb);
    assert!(app.close_asked(1, 1));
    app.windows.remove(1);
    app.notices_tick();
    assert_eq!(asked(&app), None, "its window closed");
    assert!(!takes_key(&mut app, 0, LogicalKey::Named(NamedKey::Enter)));
    assert!(app.find_window_pane_by_sid(1).is_some());
}

#[test]
fn close_question_names_three_children_and_counts_the_rest() {
    let sb = Sandbox::new();
    let mut app = with_hidden_child(&sb);
    for c in [4, 5, 6] {
        app.shadows.push(Pane::new_vacant(c, 60, 16));
        app.pane_control.shadows.family.adopted(c, 1);
    }
    app.shadows_changed();
    assert!(app.close_asked(0, 1));
    let text = app.notices.close.as_ref().unwrap().text.clone();
    let more = strings::text(Arg::MoreChildren(1));
    assert!(text.contains(&format!("\n{more}")), "{text:?}");
    assert!(!text.contains("\n…\n") && !text.ends_with("\n…"), "not a bare ellipsis: {text:?}");
}

//! The keys of a strip, while the focus, the selection and the rows
//! change under it.

use super::host_tests::{gone, say, Session};
use super::keys_tests::{after_probe, claude_screen, ev, has_keys, press, under_claude, DOWN, ENTER, UP};
use super::*;
use crate::boot_assembly_tests::Sandbox;
use crate::window_state_tests::win;
use marspot::input::{KeyState, LogicalKey};

/// What the shell was told since the last look: for which window a
/// plain `x` is the strip's, or no longer.
fn told(app: &mut CoreApp) -> Vec<(u32, bool)> {
    let frames = std::mem::take(&mut app.pending_to_shell);
    frames
        .iter()
        .filter(|f| f.0 == MsgType::StripKeys)
        .map(|f| marspot::shell_proto::decode_strip_keys(&f.1).map(|m| (m.window_id, m.on)).unwrap())
        .collect()
}

/// A second window, its pane under an agent with two helpers listed
/// and an empty input box on its screen.
fn second_window(app: &mut CoreApp) -> Session {
    let c = app.windows[0].layout.cells[0];
    let (pane, mut s) = Session::of(c.cols, c.rows + c.strip_rows);
    app.windows.push(win(2, vec![pane]));
    app.rebuild_layout(1);
    say(app, s.sid, vec![gone(s.sid + 1, true), gone(s.sid + 2, true)]);
    app.place_strips(1);
    let c = app.windows[1].layout.cells[0];
    s.shows(c.cols, c.rows, claude_screen(c.cols, "", "").as_bytes());
    app.windows[1].panes[0].pump();
    app.place_strips(1);
    app.pending_to_shell.clear();
    s
}

fn take(app: &mut CoreApp) {
    press(app, DOWN);
    after_probe(app);
    assert!(has_keys(app));
}

#[test]
fn helper_strip_keys_taken_in_another_window_give_the_first_its_x_back() {
    let sb = Sandbox::new();
    let (mut app, _s, _h) = under_claude(&sb, 2, &claude_screen(60, "", ""));
    let _b = second_window(&mut app);
    let (a, b) = (app.windows[0].window_id, app.windows[1].window_id);
    take(&mut app);
    assert_eq!(told(&mut app), [(a, true)]);
    app.key(1, ev(DOWN, KeyState::Pressed, false), Modifiers::default());
    std::thread::sleep(keys::PROBE + Duration::from_millis(10));
    app.windows[1].panes[0].pump();
    app.strips_tick();
    assert_eq!(app.strips.keys.focus.as_ref().map(|f| f.at.window_id), Some(b), "the second window has the keys");
    assert_eq!(told(&mut app), [(a, false), (b, true)]);
}

#[test]
fn helper_strip_keys_go_to_the_pane_focused_since_they_were_taken() {
    let sb = Sandbox::new();
    let (mut app, mut s, _h) = under_claude(&sb, 2, &claude_screen(60, "", ""));
    app.windows[0].panes.push(Pane::new_vacant(2, 60, 16));
    app.rebuild_layout(0);
    let c = app.windows[0].layout.cells[0];
    super::host_tests::frame(&mut app, &mut s, claude_screen(c.cols, "", "").as_bytes());
    take(&mut app);
    app.focus_pane(0, 1);
    assert!(!app.strip_key(0, &ev(ENTER, KeyState::Pressed, false), Modifiers::default()), "Enter is the other pane's");
    assert!(!has_keys(&app));
    assert_eq!(app.windows[0].panes[0].peeked(), None, "nothing viewed");
}

#[test]
fn helper_strip_x_is_let_past_the_input_method_only_while_it_acts() {
    let sb = Sandbox::new();
    let (mut app, _s, helpers) = under_claude(&sb, 2, &claude_screen(60, "", ""));
    let a = app.windows[0].window_id;
    take(&mut app);
    assert_eq!(told(&mut app), [(a, true)], "a helper's row is selected");
    press(&mut app, UP);
    assert!(has_keys(&app));
    assert_eq!(told(&mut app), [(a, false)], "main is selected");
    press(&mut app, DOWN);
    assert_eq!(told(&mut app), [(a, true)]);
    press(&mut app, DOWN);
    assert_eq!(told(&mut app), [], "nothing new to say");
    press(&mut app, ENTER);
    assert_eq!(app.windows[0].panes[0].peeked(), Some(helpers[1].sid));
    assert_eq!(told(&mut app), [(a, false)], "the keys went back");
}

#[test]
fn helper_strip_a_row_selected_past_the_rows_left_is_the_last_of_them() {
    let sb = Sandbox::new();
    let (mut app, s, helpers) = under_claude(&sb, 2, &claude_screen(60, "", ""));
    take(&mut app);
    press(&mut app, DOWN);
    say(&mut app, s.sid, vec![super::host_tests::row(helpers[0].sid, true)]);
    assert!(has_keys(&app));
    press(&mut app, ENTER);
    assert_eq!(app.windows[0].panes[0].peeked(), Some(helpers[0].sid), "Enter views the row now selected");
}

#[test]
fn helper_strip_a_paste_while_the_agent_has_its_time_stops_the_probe() {
    let sb = Sandbox::new();
    let (mut app, _s, _h) = under_claude(&sb, 2, &claude_screen(60, "", ""));
    press(&mut app, DOWN);
    assert!(app.strips.keys.due().is_some());
    let cmd = Modifiers { super_: true, ..Default::default() };
    app.key(0, ev(LogicalKey::Char('v'), KeyState::Pressed, false), cmd);
    assert!(app.strips.keys.due().is_none());
    after_probe(&mut app);
    assert!(!has_keys(&app));
}

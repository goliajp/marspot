//! What is typed while a view waits, and the strip, the pointer, the
//! wheel and what is being composed during one.

use std::sync::mpsc::Receiver;

use super::super::shadows::tests::{app_with_parent, done_for, frame, sandbox, wait_for};
use super::tests::{app_with_helper, place, press, size_of, text_of, turn, view};
use super::wait::RESIZED_WITHIN;
use super::*;
use crate::boot_assembly_tests::Sandbox;
use crate::window_state_tests::win;
use marspot::input::LogicalKey;
use marspot::shell_proto::{PlaceWhere, USER_TYPING_TEXT, encode_pane_place};

/// Session `sid` stopped, so that it has no new frame, until dropped.
struct Stopped(i32);

impl Stopped {
    fn new(sid: u64) -> Self {
        let pid = marspot_term::session_registry::read_session_entry(sid).unwrap().pid;
        assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
        Self(pid)
    }
}

impl Drop for Stopped {
    fn drop(&mut self) {
        unsafe { libc::kill(self.0, libc::SIGCONT) };
    }
}

fn app_viewing(sb: &Sandbox) -> (CoreApp, Receiver<CoreEvent>, u64, u64) {
    let (mut app, rx, host, sid) = app_with_helper(sb);
    view(&mut app, &rx, 20, sid);
    (app, rx, host, sid)
}

fn says(app: &mut CoreApp, rx: &Receiver<CoreEvent>, sid: u64, bytes: &str, seen: &str) {
    app.inject_input(sid, bytes.as_bytes());
    wait_for(seen, || {
        turn(app, rx);
        text_of(app.shown(0, 0)).contains(seen)
    });
}

/// The middle of a cell of window 0's only pane.
fn at(app: &CoreApp, col: u16, row: u16) -> (f64, f64) {
    let l = &app.windows[0].layout;
    let (cw, ch) = app.renderer.cell_dims();
    let g = marspot::layout::cell_grid(&l.cells[0], l.cell_title_h, l.padding, cw, ch);
    (g.x + (col as f64 + 0.5) * g.w, g.row_top(row) + g.h * 0.5)
}

#[test]
fn peek_asked_for_is_given_up_when_the_user_types_meanwhile() {
    let sb = sandbox();
    let (mut app, rx, host, sid) = app_with_helper(&sb);
    let _stopped = Stopped::new(sid);
    let typed: [&dyn Fn(&mut CoreApp); 2] =
        [&|app| press(app, LogicalKey::Char('t')), &|app| app.preedit(0, "か".into())];
    for (req_id, typing) in (30..).zip(typed) {
        assert_eq!(app.route_pane_control(place(req_id, sid, PlaceWhere::Peek)), Route::Forwarded);
        typing(&mut app);
        app.advance_views(Instant::now() + RESIZED_WITHIN);
        let done = done_for(&app, req_id).unwrap();
        assert_eq!((done.code, done.text.as_str()), (DoneCode::Other, USER_TYPING_TEXT));
        assert!(app.windows[0].panes[0].peek.is_none() && unwatched::is(sid));
    }
    // forced, and the user's own
    let m = PanePlace { req_id: 40, sid, to: PlaceWhere::Peek, host_sid: 0, force: true };
    assert_eq!(app.route_pane_control(frame(MsgType::PanePlace, encode_pane_place(&m))), Route::Forwarded);
    press(&mut app, LogicalKey::Char('t'));
    app.advance_views(Instant::now() + RESIZED_WITHIN);
    assert_eq!(done_for(&app, 40).unwrap().code, DoneCode::Ok);
    assert!(app.viewer_of(sid).is_some());
    app.view_helper(host, None);
    assert!(app.view_helper(host, Some(sid)));
    press(&mut app, LogicalKey::Char('t'));
    app.advance_views(Instant::now() + RESIZED_WITHIN);
    assert!(app.viewer_of(sid).is_some());
    drop(rx);
}

#[test]
fn peek_ends_the_press_of_its_own_slot_only() {
    let sb = sandbox();
    let (mut app, rx, _, sid) = app_with_helper(&sb);
    app.windows.insert(1, win(2, vec![Pane::new_vacant(4242, 80, 24)]));
    let other = Some((1, 0, 3, 4));
    (app.mouse_report_press, app.mouse_report_last_cell) = (other, other);
    view(&mut app, &rx, 20, sid);
    assert_eq!((app.mouse_report_press, app.mouse_report_last_cell), (other, other));
    let own = Some((0, 0, 3, 4));
    (app.mouse_report_press, app.mouse_report_last_cell) = (own, own);
    assert!(app.leave_focused_view(0));
    assert_eq!((app.mouse_report_press, app.mouse_report_last_cell), (None, None));
    // the window in front of it goes: the press is its window's still
    app.mouse_report_press = other;
    app.close_window(1);
    assert_eq!(app.mouse_report_press, Some((0, 0, 3, 4)));
}

#[test]
fn peek_is_left_by_closing_the_pane_shown() {
    let sb = sandbox();
    let (mut app, _rx, host, sid) = app_viewing(&sb);
    app.dispatch_context_action(0, ContextMenuAction::ClosePane, ContextRegion::Pane(0));
    assert_eq!(app.shown(0, 0).shelld_session_id(), Some(host));
    assert!(app.shown(0, 0).is_l3() && unwatched::is(sid), "neither was closed");
    assert!(app.shadow_index(sid).is_some());
}

#[test]
fn peek_of_links_and_their_menu_is_the_shown_sessions() {
    let sb = sandbox();
    let (mut app, rx, host, sid) = app_viewing(&sb);
    says(&mut app, &rx, sid, "https://shown.example/x", "shown.example/x");
    let row = (0..app.shown(0, 0).session().grid().rows())
        .find(|r| app.shown(0, 0).session().grid().cell(0, *r).ch == 'h')
        .unwrap();
    let (x, y) = at(&app, 3, row);
    assert_eq!(app.hit_test_link_at_xy(0, x, y).map(|l| l.text), Some("https://shown.example/x".into()));
    app.mouse_right_down(0, x, y, Modifiers::default());
    assert!(app.windows[0].context_menu.take().is_some_and(|m| m.link.is_some()));
    app.view_helper(host, None);
    assert!(app.hit_test_link_at_xy(0, x, y).is_none(), "the host shows none there");
}

#[test]
fn peek_wheel_goes_to_the_shown_session_and_not_off_its_strip() {
    let sb = sandbox();
    let (mut app, rx, host, sid) = app_viewing(&sb);
    let keys = || WheelKeys { enter: Vec::new(), up: b"U".to_vec(), down: b"D".to_vec(), marker: b"open".to_vec() };
    app.pane_wheel_keys.insert(sid, keys());
    says(&mut app, &rx, sid, "open", "open");
    app.scroll(0, 400.0, true);
    assert_eq!(app.pane_wheel_open.get(&sid), Some(&true), "its keys, by its own screen");

    app.strips.set_rows(host, 4);
    app.rebuild_layout(0);
    let size = app.windows[0].layout.cells[0];
    wait_for("the shown session has the rows left", || {
        turn(&mut app, &rx);
        size_of(app.shown(0, 0)) == (size.cols, size.rows)
    });
    app.place_strips(0);
    let top = app.strips.view(Some(host)).expect("the strip is the host's").top_row;
    let (x, y) = at(&app, 2, top + 1);
    app.strips.pointer_at(app.windows[0].window_id, x, y);
    assert!(app.strip_takes_wheel(0, 0), "the shown program's wheel");
    app.pane_wheel_keys.clear();
    app.pane_wheel_keys.insert(host, keys());
    assert!(!app.strip_takes_wheel(0, 0), "the hidden host's says nothing");
    app.scroll(0, 400.0, true);
    assert!(!app.pane_wheel_open.contains_key(&host));
}

#[test]
fn peek_strip_follows_the_shown_screen_and_what_is_composed_on_it() {
    let sb = sandbox();
    let (mut app, rx, host, sid) = app_with_helper(&sb);
    app.strips.set_rows(host, 4);
    app.rebuild_layout(0);
    view(&mut app, &rx, 20, sid);
    says(&mut app, &rx, sid, "one\rtwo\rthree\rfour", "four");
    app.place_strips(0);
    let top = |app: &CoreApp| app.strips.view(Some(host)).unwrap().top_row;
    let last = app.shown(0, 0).session().grid().last_used_row();
    assert_eq!(top(&app), last + 1);
    assert!(last > app.windows[0].panes[0].session().grid().last_used_row());
    let rows = app.windows[0].layout.cells[0].rows;
    assert_eq!(app.selection_floor(0, 0, rows - 1), last);
    app.preedit(0, "か".repeat(app.windows[0].layout.cells[0].cols as usize));
    app.place_strips(0);
    assert!(top(&app) > last + 1, "under what is composed at the shown cursor");
    app.preedit(0, String::new());
    // the host's own scroll position is not what the strip is under
    app.windows[0].panes[0].set_view_offset(3);
    assert_eq!(app.selection_floor(0, 0, rows - 1), top(&app) - 1);
    app.shown_mut(0, 0).set_view_offset(3);
    assert_eq!(app.selection_floor(0, 0, rows - 1), rows - 1);
    app.place_strips(0);
    assert_eq!(top(&app), rows, "under the shown session's history");
}

#[test]
fn peek_an_ended_view_leaves_the_host_alone() {
    let sb = sandbox();
    let (mut app, _rx, host) = app_with_parent(&sb);
    app.renderer.set_window_focused(true);
    app.notify_pane_focus_changes();
    assert_eq!(app.terminal_focus, Some(host));
    let last = Box::new(Pane::new_vacant(4242, 60, 16));
    app.windows[0].panes[0].peek = Some(Peek::Ended { last, name: "x".into() });
    app.notify_pane_focus_changes();
    assert_eq!(app.terminal_focus, None, "keys are not the host's");
    assert_eq!(app.focused_session(0), Some(host), "the slot is");
    app.leave_focused_view(0);
    app.notify_pane_focus_changes();
    assert_eq!(app.terminal_focus, Some(host));
}

/// The frame a resized helper is shown with is its screen at the new
/// size, text and cursor, never a blank one: the view waits for the
/// session's first frame of that size, and the session reflows before
/// it publishes.
#[test]
fn peek_the_first_frame_after_the_helpers_resize_has_its_screen() {
    let sb = sandbox();
    let (mut app, rx, _host, sid) = app_with_helper(&sb);
    app.inject_input(sid, b"hello");
    let i = app.shadow_index(sid).unwrap();
    wait_for("the helper's own frame has the text", || {
        app.shadows[i].pump();
        text_of(&app.shadows[i]).contains("hello")
    });
    let cell = app.windows[0].layout.cells[0];
    assert_ne!(size_of(&app.shadows[i]), (cell.cols, cell.rows), "the view has to resize it");
    app.route_pane_control(place(20, sid, PlaceWhere::Peek));
    wait_for("the view is shown", || {
        turn(&mut app, &rx);
        app.windows[0].panes[0].peeked() == Some(sid)
    });
    let shown = app.shown(0, 0);
    assert_eq!(size_of(shown), (cell.cols, cell.rows), "shown on its frame of the host's size, not on the deadline");
    assert!(text_of(shown).contains("hello"), "the first frame shown is not blank");
    assert_eq!(shown.session().grid().cursor(), (5, 0));
}

//! Viewing shadow panes in place, against real `marspot-session`
//! processes.

use marspot_term::mailbox::Receiver;

use super::super::held_keys::Went;
use super::super::shadows::tests::{app_with_parent, done_for, drive, frame, open_shadow, sandbox, wait_done, wait_for};
use super::wait::{RESIZED_WITHIN, has_size};
use super::*;
use crate::boot_assembly_tests::Sandbox;
use marspot::commands::{CMD, Chord, Key};
use marspot::input::{KeyState, LogicalKey, NamedKey};
use marspot::shell_proto::{
    CAP_PEEK, PANE_SESSION_CAP_FREEZE_GRID, PANE_SESSION_CAP_LOCK_KEYS, PaneSize, PlaceWhere, RetireMode,
    decode_pane_control_caps, encode_pane_place, encode_pane_retire, encode_pane_size,
};

pub(super) fn place(req_id: u32, sid: u64, to: PlaceWhere) -> Frame {
    frame(MsgType::PanePlace, encode_pane_place(&PanePlace { req_id, sid, to, host_sid: 0, force: false }))
}

/// A window with a live pane laid out, and a shadow pane below it.
pub(super) fn app_with_helper(sb: &Sandbox) -> (CoreApp, Receiver<CoreEvent>, u64, u64) {
    let (mut app, rx, host) = app_with_parent(sb);
    (app.windows[0].grid_cols, app.windows[0].grid_rows) = (1, 1);
    app.rebuild_layout(0);
    let sid = open_shadow(&mut app, &rx, 7, host);
    (app, rx, host, sid)
}

pub(super) fn turn(app: &mut CoreApp, rx: &Receiver<CoreEvent>) {
    drive(app, rx);
    app.pump_all();
}

pub(super) fn view(app: &mut CoreApp, rx: &Receiver<CoreEvent>, req_id: u32, sid: u64) {
    app.route_pane_control(place(req_id, sid, PlaceWhere::Peek));
    wait_for("the view is answered", || {
        turn(app, rx);
        done_for(app, req_id).is_some()
    });
    assert_eq!(done_for(app, req_id).unwrap().code, DoneCode::Ok);
    assert!(app.viewer_of(sid).is_some());
}

pub(super) fn text_of(p: &Pane) -> String {
    let g = p.session().grid();
    (0..g.rows()).flat_map(|r| (0..g.cols()).map(move |c| (c, r))).map(|(c, r)| g.cell(c, r).ch).collect()
}

pub(super) fn size_of(p: &Pane) -> (u16, u16) {
    (p.session().grid().cols(), p.session().grid().rows())
}

pub(super) fn ev(logical: LogicalKey, state: KeyState, repeat: bool) -> MarspotKeyEvent {
    let text = match &logical {
        LogicalKey::Char(c) => Some(c.to_string()),
        _ => None,
    };
    MarspotKeyEvent { state, logical, text: text.map(Into::into), repeat, ..Default::default() }
}

pub(super) fn press(app: &mut CoreApp, logical: LogicalKey) {
    app.key(0, ev(logical, KeyState::Pressed, false), Modifiers::default());
}

pub(super) fn release(app: &mut CoreApp, logical: LogicalKey) {
    app.key(0, ev(logical, KeyState::Released, false), Modifiers::default());
}

pub(super) fn cmd_w(app: &mut CoreApp) {
    let chord = Chord { key: Key::Char('w'), mods: CMD };
    app.key(0, chord.key_event(), chord.modifiers());
}

#[test]
fn peek_is_announced() {
    let mut pc = super::super::pane_control::PaneControl::default();
    pc.caps_after_hello();
    let caps = pc.reports.frames().find(|f| f.msg_type == MsgType::PaneControlCaps).unwrap();
    assert_ne!(decode_pane_control_caps(&caps.payload).unwrap() & CAP_PEEK, 0);
}

#[test]
fn peek_shows_the_helper_at_the_hosts_size_and_the_host_as_it_was() {
    let sb = sandbox();
    let (mut app, rx, host, sid) = app_with_helper(&sb);
    wait_for("the host has its layout's size", || {
        turn(&mut app, &rx);
        has_size(&app.windows[0].panes[0], &app.windows[0].layout.cells[0])
    });
    let cell = app.windows[0].layout.cells[0];
    assert_ne!((cell.cols, cell.rows), (120, 40), "the sizes have to differ for a resize to be seen");
    app.windows[0].panes[0].set_view_offset(3);

    assert_eq!(app.route_pane_control(place(20, sid, PlaceWhere::Peek)), Route::Forwarded, "not before its frame");
    assert!(app.windows[0].panes[0].peek.is_none(), "the host is shown meanwhile");
    assert!(app.idle_wait().is_some_and(|w| w <= RESIZED_WITHIN), "the loop wakes at the deadline");
    wait_for("the view is answered", || {
        turn(&mut app, &rx);
        done_for(&app, 20).is_some()
    });
    assert_eq!(done_for(&app, 20).unwrap().code, DoneCode::Ok);
    assert_eq!(app.windows[0].panes[0].peeked(), Some(sid));
    assert_eq!(size_of(app.shown(0, 0)), (cell.cols, cell.rows));
    assert!(!unwatched::is(sid));
    assert_eq!(app.focused_session(0), Some(sid));
    assert!(app.view_label(0, 0).is_some_and(|l| l.contains("⌘W")), "{:?}", app.view_label(0, 0));

    app.notify_pane_focus_changes();
    let focused = app.pending_to_shell.iter().rfind(|f| f.0 == MsgType::PaneFocused).unwrap();
    assert_eq!(focused.1, marspot::shell_proto::encode_pane_focused(sid));

    // asked for again it is there already; its size is the window's
    assert_eq!(app.route_pane_control(place(21, sid, PlaceWhere::Peek)), Route::Answered(DoneCode::Ok));
    let size = encode_pane_size(&PaneSize { req_id: 22, sid, cols: 50, rows: 20 });
    app.route_pane_control(frame(MsgType::PaneSize, size));
    assert_eq!(done_for(&app, 22).unwrap().text, "not_resizable");

    assert_eq!(app.route_pane_control(place(23, sid, PlaceWhere::Unpeek)), Route::Answered(DoneCode::Ok));
    let pane = app.shown(0, 0);
    assert_eq!((pane.shelld_session_id(), pane.view_offset()), (Some(host), 3));
    assert_eq!(size_of(pane), (cell.cols, cell.rows), "the host was never resized");
    assert!(unwatched::is(sid));

    // the same size: at once
    assert_eq!(app.route_pane_control(place(24, sid, PlaceWhere::Peek)), Route::Answered(DoneCode::Ok));
    assert!(app.view_helper(host, None));
    assert!(app.windows[0].panes[0].peek.is_none());
}

#[test]
fn peek_follows_the_layout_while_it_is_viewed() {
    let sb = sandbox();
    let (mut app, rx, _, sid) = app_with_helper(&sb);
    view(&mut app, &rx, 20, sid);
    app.windows[0].w_phys -= 200.0;
    app.rebuild_layout(0);
    let cell = app.windows[0].layout.cells[0];
    wait_for("both have the new size", || {
        turn(&mut app, &rx);
        has_size(&app.windows[0].panes[0], &cell) && has_size(app.shown(0, 0), &cell)
    });
}

#[test]
fn peek_of_a_session_that_does_not_answer_is_shown_at_the_deadline() {
    let sb = sandbox();
    let (mut app, rx, _, sid) = app_with_helper(&sb);
    let pid = marspot_term::session_registry::read_session_entry(sid).unwrap().pid;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
    assert_eq!(app.route_pane_control(place(20, sid, PlaceWhere::Peek)), Route::Forwarded);
    turn(&mut app, &rx);
    assert!(app.viewer_of(sid).is_none());
    app.advance_views(Instant::now() + RESIZED_WITHIN);
    assert!(app.viewer_of(sid).is_some());
    assert_eq!(done_for(&app, 20).unwrap().code, DoneCode::Ok);
    assert_eq!(unsafe { libc::kill(pid, libc::SIGCONT) }, 0);
}

#[test]
fn peek_is_refused_away_from_its_root_and_without_a_cell() {
    let sb = sandbox();
    let (mut app, rx, host, sid) = app_with_helper(&sb);
    let nested = open_shadow(&mut app, &rx, 8, sid);
    let asked = |app: &mut CoreApp, req_id: u32, sid: u64, host_sid: u64| {
        app.route_pane_control(frame(
            MsgType::PanePlace,
            encode_pane_place(&PanePlace { req_id, sid, to: PlaceWhere::Peek, host_sid, force: false }),
        ));
        done_for(app, req_id).map(|d| (d.code, d.text))
    };
    let other = |why: &str| Some((DoneCode::Other, why.to_string()));
    assert_eq!(asked(&mut app, 30, nested, sid), other("the parent pane is in no window"));
    assert_eq!(asked(&mut app, 31, host, 0), other("only a shadow pane is viewed in place"));
    assert_eq!(asked(&mut app, 32, 0x7777_0001, 0), Some((DoneCode::NoSuchSession, String::new())));
    app.windows[0].panes.push(Pane::new_vacant(4242, 80, 24));
    assert_eq!(asked(&mut app, 33, sid, 4242), other("the parent pane is not in its window's grid"));
    let cells = std::mem::take(&mut app.windows[0].layout.cells);
    assert_eq!(asked(&mut app, 34, sid, 0), other("the parent pane is not in its window's grid"));
    app.windows[0].layout.cells = vec![cells[0], cells[0]];
    assert_eq!(asked(&mut app, 35, sid, 4242), other("the pane is not below that parent"));
    assert!(unwatched::is(sid) && app.windows[0].panes[0].peek.is_none());
    // below a helper: in the pane at the top
    app.windows[0].layout.cells = cells;
    view(&mut app, &rx, 36, nested);
    assert_eq!(app.viewer_of(nested), Some((0, 0)));
    // another takes its place
    view(&mut app, &rx, 37, sid);
    assert!(unwatched::is(nested) && !unwatched::is(sid));
}

#[test]
fn peek_keys_go_to_the_session_they_were_pressed_for() {
    let sb = sandbox();
    let (mut app, rx, host, sid) = app_with_helper(&sb);
    view(&mut app, &rx, 20, sid);
    for c in ['h', 'e', 'y'] {
        press(&mut app, LogicalKey::Char(c));
        release(&mut app, LogicalKey::Char(c));
    }
    wait_for("the helper's terminal shows what was typed", || {
        turn(&mut app, &rx);
        text_of(app.shown(0, 0)).contains("hey")
    });
    assert!(!text_of(&app.windows[0].panes[0]).contains("hey"));

    // held over the switch back: the repeat is nobody's, the release the helper's
    press(&mut app, LogicalKey::Char('z'));
    assert_eq!(app.pane_control.shadows.peeks.held.of(&LogicalKey::Char('z')), Some(Went::To(sid)));
    cmd_w(&mut app);
    assert!(app.windows[0].panes[0].peek.is_none(), "Cmd-W leaves the view");
    assert_eq!(app.windows[0].panes.len(), 1, "and closes nothing");
    app.key(0, ev(LogicalKey::Char('z'), KeyState::Pressed, true), Modifiers::default());
    release(&mut app, LogicalKey::Char('z'));
    assert_eq!(app.pane_control.shadows.peeks.held.of(&LogicalKey::Char('z')), None);
    assert!(app.release_follows_press(&ev(LogicalKey::Char('w'), KeyState::Released, false), Modifiers::default()), "W was nobody's");
    press(&mut app, LogicalKey::Char('k'));
    wait_for("the host's terminal shows the next key", || {
        turn(&mut app, &rx);
        text_of(app.shown(0, 0)).contains('k')
    });
    assert_eq!(app.focused_session(0), Some(host));
    assert!(!text_of(app.shown(0, 0)).contains('z'), "the repeat did not reach the host");
}

#[test]
fn peek_redraws_for_what_is_shown_and_not_for_the_hidden_host() {
    let sb = sandbox();
    let (mut app, rx, host, sid) = app_with_helper(&sb);
    view(&mut app, &rx, 20, sid);
    app.inject_input(host, b"hidden");
    app.windows[0].needs_render = false;
    let mut seen = 0;
    wait_for("the host's frame was read", || {
        drive(&mut app, &rx);
        seen += app.pump_all();
        text_of(&app.windows[0].panes[0]).contains("hidden")
    });
    assert!(seen > 0, "a frame was taken");
    assert!(!app.windows[0].needs_render, "for nothing on the screen");
    app.inject_input(sid, b"shown");
    wait_for("the helper's frame was read", || {
        turn(&mut app, &rx);
        text_of(app.shown(0, 0)).contains("shown")
    });
    assert!(app.windows[0].needs_render);
}

#[test]
fn peek_holds_and_freezes_are_those_of_the_session_shown() {
    let sb = sandbox();
    let (mut app, rx, host, sid) = app_with_helper(&sb);
    view(&mut app, &rx, 20, sid);
    app.pane_session_begin(host, PANE_SESSION_CAP_FREEZE_GRID | PANE_SESSION_CAP_LOCK_KEYS);
    assert_eq!(app.focused_pane_active_session(0), None);
    press(&mut app, LogicalKey::Char('f'));
    wait_for("the helper is read and typed into while its host is held", || {
        turn(&mut app, &rx);
        text_of(app.shown(0, 0)).contains('f')
    });
    app.pane_session_begin(sid, PANE_SESSION_CAP_LOCK_KEYS);
    assert_eq!(app.focused_pane_active_session(0), Some(sid));
    app.pending_to_shell.clear();
    press(&mut app, LogicalKey::Char('g'));
    assert!(app.pending_to_shell.iter().any(|f| f.0 == MsgType::PaneSessionKey));
}

#[test]
fn peek_selection_and_scrolling_are_the_shown_sessions() {
    let sb = sandbox();
    let (mut app, rx, _, sid) = app_with_helper(&sb);
    let sel = Selection { session_idx: 0, anchor: (0, 0), focus: (3, 0), mode: SelectionMode::Linewise };
    app.windows[0].selection = Some(sel);
    view(&mut app, &rx, 20, sid);
    assert!(app.windows[0].selection.is_none(), "it was over the host's text");
    app.windows[0].selection = Some(sel);
    app.shown_mut(0, 0).set_view_offset(5);
    app.snap_focused_to_live(0);
    assert_eq!(app.shown(0, 0).view_offset(), 0);
    app.handle_cmd_f(0);
    assert!(app.shown(0, 0).search.is_some() && app.windows[0].panes[0].search.is_none());
    cmd_w(&mut app);
    assert!(app.windows[0].selection.is_none());
    assert!(app.shown(0, 0).search.is_none(), "the helper's search stays with it");
}

#[test]
fn peek_of_a_helper_that_ends_keeps_its_last_frame_until_it_is_left() {
    let sb = sandbox();
    let (mut app, rx, host, sid) = app_with_helper(&sb);
    view(&mut app, &rx, 20, sid);
    app.inject_input(sid, b"last words\r");
    wait_for("its words are on the screen", || {
        turn(&mut app, &rx);
        text_of(app.shown(0, 0)).contains("last words")
    });
    app.inject_input(sid, b"q\r");
    wait_for("it ended", || {
        turn(&mut app, &rx);
        app.shadows.is_empty()
    });
    assert!(matches!(app.windows[0].panes[0].peek, Some(Peek::Ended { .. })));
    assert!(text_of(app.shown(0, 0)).contains("last words"));
    assert_eq!(app.focused_session(0), Some(host), "the slot answers for itself");
    assert!(app.view_label(0, 0).is_some_and(|l| l.contains(t(Msg::HelperEnded))));
    app.renderer.set_window_focused(true);
    app.notify_pane_focus_changes();
    assert_eq!(app.terminal_focus, None, "nor is the hidden host told it has the keyboard");
    app.handle_cmd_f(0);
    assert!(app.shown(0, 0).search.is_none());

    press(&mut app, LogicalKey::Char('a'));
    assert!(app.windows[0].panes[0].peek.is_some(), "an ordinary key does nothing");
    assert!(app.shown(0, 0).is_vacant(), "and starts no session");
    app.key(0, ev(LogicalKey::Named(NamedKey::Enter), KeyState::Pressed, true), Modifiers::default());
    assert!(app.windows[0].panes[0].peek.is_some(), "one held since before it ended leaves nothing");
    press(&mut app, LogicalKey::Named(NamedKey::Enter));
    assert!(app.windows[0].panes[0].peek.is_none());
    // the same Enter, still held, is not the host's
    app.key(0, ev(LogicalKey::Named(NamedKey::Enter), KeyState::Pressed, true), Modifiers::default());
    assert_eq!(app.pane_control.shadows.peeks.held.of(&LogicalKey::Named(NamedKey::Enter)), Some(Went::Nowhere));
    release(&mut app, LogicalKey::Named(NamedKey::Enter));
    assert_eq!(app.shown(0, 0).shelld_session_id(), Some(host));
    assert!(app.shown(0, 0).is_l3());
    assert!(!text_of(app.shown(0, 0)).contains('a'));
}

#[test]
fn peek_of_a_helper_closed_or_moved_away_ends_too() {
    let sb = sandbox();
    let (mut app, rx, host, sid) = app_with_helper(&sb);
    let moved = open_shadow(&mut app, &rx, 8, host);
    view(&mut app, &rx, 20, sid);
    app.route_pane_control(frame(MsgType::PaneRetire, encode_pane_retire(&marspot::shell_proto::PaneRetire {
        req_id: 21,
        sid,
        mode: RetireMode::Delete,
        shadow_only: false,
    })));
    assert!(matches!(app.windows[0].panes[0].peek, Some(Peek::Ended { .. })));
    press(&mut app, LogicalKey::Named(NamedKey::ArrowUp));
    assert!(app.windows[0].panes[0].peek.is_none());

    view(&mut app, &rx, 22, moved);
    app.inject_input(moved, b"carried");
    wait_for("its text is on the screen", || {
        turn(&mut app, &rx);
        text_of(app.shown(0, 0)).contains("carried")
    });
    app.route_pane_control(place(23, moved, PlaceWhere::Window));
    assert_eq!(wait_done(&mut app, &rx, 23).code, DoneCode::Ok);
    assert!(text_of(app.shown(0, 0)).contains("carried"), "the last frame stays where it was viewed");
    let (wi, idx) = app.find_window_pane_by_sid(moved).expect("in the window");
    assert!(text_of(&app.windows[wi].panes[idx]).contains("carried"), "and the pane in the window has its own");
    assert!(!unwatched::is(moved));
    cmd_w(&mut app);
    assert!(app.windows[0].panes[0].peek.is_none(), "Cmd-W leaves an ended view");
}

#[test]
fn peek_ends_with_its_host() {
    let sb = sandbox();
    let (mut app, rx, host, sid) = app_with_helper(&sb);
    app.windows[0].panes.push(Pane::new_pending(4242, 80, 24));
    view(&mut app, &rx, 20, sid);
    app.close_session(0, 0);
    assert!(app.find_window_pane_by_sid(host).is_none() && app.viewer_of(sid).is_none());
    assert!(unwatched::is(sid), "its frames wake nobody again");
}

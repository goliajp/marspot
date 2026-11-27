//! The guard under Claude Code's input box as a real session publishes
//! it.

use super::*;
use crate::boot_assembly_tests::Sandbox;
use crate::helper_strip::host_tests::{app, frame, Session};
use crate::pane_control::Route;
use marspot::shell_proto::{encode_pane_type, DoneCode, PaneType};

fn claude_screen(typed: &str) -> String {
    let rule = "─".repeat(60);
    format!("{rule}\r\n❯ {typed}\r\n{rule}\r\n  ? for shortcuts\x1b[2;{}H", 3 + typed.chars().count())
}

fn key(app: &mut CoreApp, logical: LogicalKey) {
    for state in [KeyState::Pressed, KeyState::Released] {
        let text = match &logical {
            LogicalKey::Char(c) => Some(c.to_string().into()),
            _ => None,
        };
        app.key(0, MarspotKeyEvent { state, logical: logical.clone(), text, ..Default::default() }, Modifiers::default());
    }
}

const ENTER: LogicalKey = LogicalKey::Named(NamedKey::Enter);
const ESC: LogicalKey = LogicalKey::Named(NamedKey::Escape);
const TAB: LogicalKey = LogicalKey::Named(NamedKey::Tab);

fn showing(sb: &Sandbox, typed: &str, mode: CcModel) -> (CoreApp, Session) {
    let (mut app, mut s) = app(sb);
    marspot::settings::set_for_test(marspot::settings::Settings { cc_model: mode, ..Default::default() });
    frame(&mut app, &mut s, claude_screen(typed).as_bytes());
    app.pending_to_shell.clear();
    (app, s)
}

fn confirmed(app: &mut CoreApp) -> Vec<u64> {
    let frames = std::mem::take(&mut app.pending_to_shell);
    frames.iter().filter(|f| f.0 == MsgType::ModelConfirmed).map(|f| marspot::shell_proto::decode_model_confirmed(&f.1).unwrap()).collect()
}

#[test]
fn enter_on_model_is_held_until_y_and_then_sent_with_the_shell_told() {
    let sb = Sandbox::new();
    let (mut app, mut s) = showing(&sb, "/model opus", CcModel::Managed);
    key(&mut app, ENTER);
    assert_eq!(s.keys(&app.windows[0].panes[0]), 0, "the Enter is held");
    assert_eq!(app.notices.over(Some(s.sid), true), Some(t(Msg::ModelGuardNotice)));
    assert_eq!(app.notices.over(Some(s.sid + 1), true), None, "over its own pane only");
    key(&mut app, LogicalKey::Char('x'));
    assert_eq!(s.keys(&app.windows[0].panes[0]), 0, "other keys are swallowed while it asks");
    assert!(confirmed(&mut app).is_empty());
    key(&mut app, LogicalKey::Char('y'));
    assert_eq!(s.keys(&app.windows[0].panes[0]), 2, "the held Enter, pressed and let go");
    assert_eq!(confirmed(&mut app), [s.sid]);
    assert!(app.notices.guard.is_none());
    assert_eq!(app.notices.over(Some(s.sid), true), None);
}

#[test]
fn esc_drops_the_held_enter_and_the_box_keeps_what_it_holds() {
    let sb = Sandbox::new();
    let (mut app, mut s) = showing(&sb, "/model", CcModel::Managed);
    key(&mut app, ENTER);
    key(&mut app, ESC);
    assert_eq!(s.keys(&app.windows[0].panes[0]), 0, "neither the Enter nor the Esc reached claude");
    assert!(app.notices.guard.is_none() && confirmed(&mut app).is_empty());
    key(&mut app, LogicalKey::Char('y'));
    assert_eq!(s.keys(&app.windows[0].panes[0]), 2, "after it, y is a key again");
}

#[test]
fn other_text_and_the_manual_setting_let_enter_through() {
    let sb = Sandbox::new();
    for (typed, mode) in [("/models", CcModel::Managed), ("hello /model", CcModel::Managed), ("/model opus", CcModel::Manual)] {
        let (mut app, mut s) = showing(&sb, typed, mode);
        key(&mut app, ENTER);
        assert_eq!(s.keys(&app.windows[0].panes[0]), 2, "{typed:?} {mode:?}");
        assert!(app.notices.guard.is_none(), "{typed:?} {mode:?}");
    }
    let (mut app, mut s) = app(&sb);
    frame(&mut app, &mut s, b"$ /model opus");
    key(&mut app, ENTER);
    assert_eq!(s.keys(&app.windows[0].panes[0]), 2, "a shell's /model is not Claude Code's");
}

fn typed(app: &mut CoreApp, sid: u64, text: &str, enter: bool) -> Route {
    let t = PaneType { req_id: 9, sid, paste: true, enter, only_at_shell: false, bytes: text.as_bytes().to_vec(), watch: None };
    app.route_pane_control(Frame::new(MsgType::PaneType, encode_pane_type(&t).unwrap()))
}

#[test]
fn a_program_cannot_send_model_and_is_told_why() {
    let sb = Sandbox::new();
    let (mut app, s) = showing(&sb, "", CcModel::Managed);
    assert_eq!(typed(&mut app, s.sid, "/model fable", true), Route::Answered(DoneCode::Other));
    assert_eq!(app.notices.shell_text(), Some(t(Msg::ModelSendRefused)));
    assert_ne!(typed(&mut app, s.sid, "say hi", true), Route::Answered(DoneCode::Other));
    assert_ne!(typed(&mut app, s.sid, "/model fable", false), Route::Answered(DoneCode::Other), "typing it alone sends nothing");
    assert!(app.model_guard_refuses(s.sid, "/model\r", false, false, "test"), "injected bytes");
    // a paste the screen does not show yet, then the Enter for it
    app.forward_pane_paste(s.sid, "/model opus");
    assert!(app.model_guard_refuses(s.sid, "\r", false, false, "test"));
    assert!(!app.model_guard_refuses(s.sid, "\r", false, false, "test"), "the paste stood for one Enter");
    marspot::settings::set_for_test(marspot::settings::Settings { cc_model: CcModel::Manual, ..Default::default() });
    assert!(!app.model_guard_refuses(s.sid, "/model\r", false, false, "test"), "manual refuses nothing");
}

#[test]
fn a_box_holding_model_is_not_sent_by_a_programs_enter() {
    let sb = Sandbox::new();
    let (mut app, s) = showing(&sb, "/model", CcModel::Managed);
    assert!(app.model_guard_refuses(s.sid, "\r", false, false, "test"));
    assert!(!app.model_guard_refuses(s.sid, "", false, false, "test"));
}

fn typed_raw(app: &mut CoreApp, sid: u64, text: &str, enter: bool) -> Route {
    let t = PaneType { req_id: 9, sid, paste: false, enter, only_at_shell: false, bytes: text.as_bytes().to_vec(), watch: None };
    app.route_pane_control(Frame::new(MsgType::PaneType, encode_pane_type(&t).unwrap()))
}

#[test]
fn a_program_cannot_send_model_from_a_later_line_or_after_editing_the_box() {
    let sb = Sandbox::new();
    let (mut app, s) = showing(&sb, "draft", CcModel::Managed);
    let refused = Route::Answered(DoneCode::Other);
    assert_eq!(typed_raw(&mut app, s.sid, "hi\r/model\r", false), refused, "the box is empty after the first line");
    assert_eq!(typed_raw(&mut app, s.sid, "\x15/model", true), refused, "Ctrl-U, then /model and Enter");
    assert_eq!(typed_raw(&mut app, s.sid, "\x7f\x7f\x7f\x7f\x7f/model\r", false), refused, "backspaces");
    assert!(app.model_guard_refuses(s.sid, "\x1b[200~/model\x1b[201~\r", false, false, "test"), "a bracketed paste injected raw");
    assert_ne!(typed_raw(&mut app, s.sid, "hi\rthere\r", false), refused);
}

#[test]
fn model_in_one_program_call_and_the_enter_in_the_next_is_refused() {
    let sb = Sandbox::new();
    let (mut app, s) = showing(&sb, "", CcModel::Managed);
    let refused = Route::Answered(DoneCode::Other);
    assert_ne!(typed_raw(&mut app, s.sid, "/model", false), refused);
    assert_eq!(typed_raw(&mut app, s.sid, "\r", false), refused, "the screen has not redrawn: the box holds /model");
    assert_ne!(typed_raw(&mut app, s.sid, "/mo", false), refused);
    assert_eq!(typed_raw(&mut app, s.sid, "", true), refused, "an Enter alone");
    assert_ne!(typed_raw(&mut app, s.sid, "del\x01", false), refused);
    assert_eq!(typed_raw(&mut app, s.sid, "/mo\r", false), refused, "a byte that could not be followed is carried");
    assert_ne!(typed_raw(&mut app, s.sid, "hello", false), refused);
    assert_ne!(typed_raw(&mut app, s.sid, "\r", false), refused);
    assert_ne!(typed_raw(&mut app, s.sid, "/model", false), refused);
    app.notices.unsent.iter_mut().for_each(|u| u.2 = Instant::now());
    assert_ne!(typed_raw(&mut app, s.sid, "\r", false), refused, "what is past its time no longer stands for the box");
}

#[test]
fn tab_or_enter_on_a_start_of_model_is_held() {
    let sb = Sandbox::new();
    for (typed, k) in [("/mo", TAB), ("/mod", TAB), ("/model", TAB), ("/mode", ENTER)] {
        let (mut app, mut s) = showing(&sb, typed, CcModel::Managed);
        key(&mut app, k.clone());
        assert_eq!(s.keys(&app.windows[0].panes[0]), 0, "{typed:?} is held");
        assert!(app.notices.guard.is_some(), "{typed:?}");
        key(&mut app, LogicalKey::Char('y'));
        assert_eq!(s.keys(&app.windows[0].panes[0]), 2, "{typed:?}: the held key, pressed and let go");
    }
    let (mut app, mut s) = showing(&sb, "/m", CcModel::Managed);
    key(&mut app, TAB);
    assert_eq!(s.keys(&app.windows[0].panes[0]), 2, "two letters are not held");
}

#[test]
fn a_shadow_panes_box_is_guarded_as_a_drawn_ones() {
    let sb = Sandbox::new();
    let (mut app, s) = showing(&sb, "", CcModel::Managed);
    let pane = std::mem::replace(&mut app.windows[0].panes[0], Pane::new_vacant(1, 60, 16));
    app.shadows.push(pane);
    assert!(app.find_window_pane_by_sid(s.sid).is_none(), "in no window");
    assert_eq!(typed(&mut app, s.sid, "/model fable", true), Route::Answered(DoneCode::Other));
}

#[test]
fn a_scrolled_back_pane_still_holds_its_enter() {
    let sb = Sandbox::new();
    let (mut app, mut s) = showing(&sb, "/model", CcModel::Managed);
    app.windows[0].panes[0].set_view_offset(3);
    key(&mut app, ENTER);
    assert_eq!(s.keys(&app.windows[0].panes[0]), 0, "the live box holds /model");
    assert!(app.notices.guard.is_some());
}

#[test]
fn the_windows_shortcuts_pass_while_it_asks() {
    let sb = Sandbox::new();
    let (mut app, _s) = showing(&sb, "/model", CcModel::Managed);
    key(&mut app, ENTER);
    let cmd = Modifiers { super_: true, ..Default::default() };
    let w = MarspotKeyEvent { state: KeyState::Pressed, logical: LogicalKey::Char('w'), ..Default::default() };
    assert!(!app.model_guard_key(0, &w, cmd), "a Cmd shortcut is not the question's");
    assert!(app.notices.guard.is_some(), "and the question stays up");
}

fn commit(app: &mut CoreApp, text: &str) {
    let ev = MarspotKeyEvent { state: KeyState::Pressed, logical: LogicalKey::Other, text: Some(text.into()), ..Default::default() };
    app.key(0, ev, Modifiers::default());
}

#[test]
fn a_y_composed_by_an_input_method_answers_and_its_commit_is_dropped() {
    let sb = Sandbox::new();
    let (mut app, mut s) = showing(&sb, "/model", CcModel::Managed);
    key(&mut app, ENTER);
    for other in ["n", "x"] {
        // as if a composition had been shown before the question went up
        app.windows[0].ime_preedit = "zh".into();
        app.preedit(0, other.into());
        assert!(app.windows[0].ime_preedit.is_empty(), "{other:?} is swallowed and not shown");
        assert!(app.notices.guard.is_some(), "{other:?} leaves it asking");
    }
    app.preedit(0, String::new());
    assert_eq!(s.keys(&app.windows[0].panes[0]), 0);
    app.preedit(0, "y".into());
    assert!(app.notices.guard.is_none());
    assert!(app.windows[0].ime_preedit.is_empty(), "the composition is not shown");
    assert_eq!(s.keys(&app.windows[0].panes[0]), 2, "only the held Enter, pressed and let go");
    assert_eq!(confirmed(&mut app), [s.sid]);
    // the input method still holds its y: more of it, its end and its commit reach nobody
    app.preedit(0, "ye".into());
    app.preedit(0, String::new());
    commit(&mut app, "也");
    assert!(app.windows[0].ime_preedit.is_empty());
    assert_eq!(s.keys(&app.windows[0].panes[0]), 0, "nothing typed into the box");
    // the next composition is the user's again
    app.preedit(0, "n".into());
    assert_eq!(app.windows[0].ime_preedit, "n");
    app.preedit(0, String::new());
    commit(&mut app, "你");
    assert_eq!(s.keys(&app.windows[0].panes[0]), 1, "a commit after the answered one is typed");
}

#[test]
fn committed_full_width_y_answers_and_other_text_does_not() {
    let sb = Sandbox::new();
    for yes in ["\u{FF59}", "\u{FF39}", "Y"] {
        let (mut app, mut s) = showing(&sb, "/model", CcModel::Managed);
        key(&mut app, ENTER);
        commit(&mut app, "好");
        assert!(app.notices.guard.is_some(), "other committed text is swallowed");
        assert_eq!(s.keys(&app.windows[0].panes[0]), 0);
        commit(&mut app, yes);
        assert!(app.notices.guard.is_none(), "{yes:?}");
        assert_eq!(s.keys(&app.windows[0].panes[0]), 2, "{yes:?}: the held Enter only");
        assert_eq!(confirmed(&mut app), [s.sid]);
    }
}

#[test]
fn esc_drops_it_after_a_swallowed_composition_and_programs_wait_on_it() {
    let sb = Sandbox::new();
    let (mut app, mut s) = showing(&sb, "/model", CcModel::Managed);
    key(&mut app, ENTER);
    assert!(app.model_guard_refuses(s.sid, "more", true, false, "test"), "a program cannot type under the question");
    app.preedit(0, "n".into());
    app.preedit(0, String::new());
    key(&mut app, ESC);
    assert!(app.notices.guard.is_none() && confirmed(&mut app).is_empty());
    assert_eq!(s.keys(&app.windows[0].panes[0]), 0);
    app.preedit(0, "y".into());
    assert_eq!(app.windows[0].ime_preedit, "y", "with nothing held a composition is shown");
}

/// Answered with a composed y, then the composition cancelled.
fn answered_then_cancelled(sb: &Sandbox) -> (CoreApp, Session) {
    let (mut app, mut s) = showing(sb, "/model", CcModel::Managed);
    key(&mut app, ENTER);
    app.preedit(0, "y".into());
    assert_eq!(s.keys(&app.windows[0].panes[0]), 2);
    // Esc or Backspace ends the composition with nothing committed
    app.preedit(0, String::new());
    (app, s)
}

#[test]
fn a_cancelled_answering_composition_does_not_eat_the_next_text() {
    let sb = Sandbox::new();
    let (mut app, mut s) = answered_then_cancelled(&sb);
    let esc_up = MarspotKeyEvent { state: KeyState::Released, logical: ESC, ..Default::default() };
    app.key(0, esc_up, Modifiers::default());
    app.preedit(0, String::new());
    commit(&mut app, "，");
    assert_eq!(s.keys(&app.windows[0].panes[0]), 1, "the comma is typed into the pane");
    assert!(app.notices.composed_yes.is_none());

    let (mut app, mut s) = answered_then_cancelled(&sb);
    let cmd = Modifiers { super_: true, ..Default::default() };
    app.key(0, MarspotKeyEvent { state: KeyState::Pressed, logical: LogicalKey::Char('b'), ..Default::default() }, cmd);
    commit(&mut app, "a");
    assert_eq!(s.keys(&app.windows[0].panes[0]), 1, "a shortcut in between ends it too");

    let (mut app, mut s) = answered_then_cancelled(&sb);
    app.notices.composed_yes = app.notices.composed_yes.map(|(w, _)| (w, Some(Instant::now() - Duration::from_secs(5))));
    commit(&mut app, "a");
    assert_eq!(s.keys(&app.windows[0].panes[0]), 1, "a commit long after the end is the user's");
}

#[test]
fn a_plain_committed_y_answers() {
    // with a plain keyboard layout a y arrives as committed text, not as a key of its own
    let sb = Sandbox::new();
    let (mut app, mut s) = showing(&sb, "/model", CcModel::Managed);
    key(&mut app, ENTER);
    commit(&mut app, "y");
    assert!(app.notices.guard.is_none());
    assert_eq!(s.keys(&app.windows[0].panes[0]), 2, "the held Enter only");
    assert_eq!(confirmed(&mut app), [s.sid]);
}

#[test]
fn a_paste_and_a_file_drop_wait_while_it_asks() {
    let sb = Sandbox::new();
    let (mut app, mut s) = showing(&sb, "/model", CcModel::Managed);
    key(&mut app, ENTER);
    app.forward_pane_paste(s.sid, "more");
    app.file_drop(0, 1.0, 1.0, &["/tmp/a b".to_string()]);
    assert_eq!(s.sent(&app.windows[0].panes[0], MsgType::Paste), 0, "neither reaches the pane");
    assert!(app.notices.guard.is_some(), "and the question stays up");
    key(&mut app, ESC);
    app.forward_pane_paste(s.sid, "more");
    app.file_drop(0, 1.0, 1.0, &["/tmp/a".to_string()]);
    assert_eq!(s.sent(&app.windows[0].panes[0], MsgType::Paste), 2, "after it both are pasted");
}

#[test]
fn another_window_composes_while_it_asks_in_the_first() {
    let sb = Sandbox::new();
    let (mut app, _s) = showing(&sb, "/model", CcModel::Managed);
    key(&mut app, ENTER);
    app.windows.push(crate::window_state_tests::win(2, vec![Pane::new_vacant(1, 60, 16)]));
    app.rebuild_layout(1);
    app.preedit(1, "y".into());
    assert_eq!(app.windows[1].ime_preedit, "y", "the other window shows its composition");
    assert!(app.notices.guard.is_some(), "and does not answer the first's question");
    assert!(app.notices.composed_yes.is_none());
}

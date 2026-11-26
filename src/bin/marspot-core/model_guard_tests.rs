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
    assert_ne!(typed(&mut app, s.sid, "/model fable", false), Route::Answered(DoneCode::Other), "typing it alone sends nothing");
    assert_ne!(typed(&mut app, s.sid, "say hi", true), Route::Answered(DoneCode::Other));
    assert!(app.model_guard_refuses(s.sid, "/model\r", true, "test"), "injected bytes");
    // a paste the screen does not show yet, then the Enter for it
    app.forward_pane_paste(s.sid, "/model opus");
    assert!(app.model_guard_refuses(s.sid, "\r", true, "test"));
    assert!(!app.model_guard_refuses(s.sid, "\r", true, "test"), "the paste stood for one Enter");
    marspot::settings::set_for_test(marspot::settings::Settings { cc_model: CcModel::Manual, ..Default::default() });
    assert!(!app.model_guard_refuses(s.sid, "/model\r", true, "test"), "manual refuses nothing");
}

#[test]
fn a_box_holding_model_is_not_sent_by_a_programs_enter() {
    let sb = Sandbox::new();
    let (mut app, s) = showing(&sb, "/model", CcModel::Managed);
    assert!(app.model_guard_refuses(s.sid, "\r", true, "test"));
    assert!(!app.model_guard_refuses(s.sid, "\r", false, "test"));
}

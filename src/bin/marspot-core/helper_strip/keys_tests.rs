//! The keyboard in a strip, under an agent's screen as a real session
//! publishes it.

use super::host_tests::{frame, lines, Session};
use super::view_tests::with_helpers;
use super::*;
use crate::boot_assembly_tests::Sandbox;
use marspot::input::{KeyState, LogicalKey, NamedKey};
use marspot::shell_proto::{decode_helper_panel_act, encode_strip_keys, PanelAct, StripKeys};

pub(super) const DOWN: LogicalKey = LogicalKey::Named(NamedKey::ArrowDown);
pub(super) const UP: LogicalKey = LogicalKey::Named(NamedKey::ArrowUp);
pub(super) const ENTER: LogicalKey = LogicalKey::Named(NamedKey::Enter);
const ESC: LogicalKey = LogicalKey::Named(NamedKey::Escape);

pub(super) fn ev(logical: LogicalKey, state: KeyState, repeat: bool) -> MarspotKeyEvent {
    let text = match &logical {
        LogicalKey::Char(c) => Some(c.to_string()),
        _ => None,
    };
    MarspotKeyEvent { state, logical, text, repeat, ..Default::default() }
}

pub(super) fn press(app: &mut CoreApp, key: LogicalKey) {
    app.key(0, ev(key, KeyState::Pressed, false), Modifiers::default());
}

/// Claude Code's input box, empty, the cursor in it, and `below` under
/// its footer.
pub(super) fn claude_screen(cols: u16, typed: &str, below: &str) -> String {
    let rule = "─".repeat(cols as usize);
    format!("{rule}\r\n❯ {typed}\r\n{rule}\r\n  ? for shortcuts{below}\x1b[2;{}H", 3 + typed.len())
}

/// A pane under Claude with `n` helpers, its screen `screen`.
pub(super) fn under_claude(sb: &Sandbox, n: usize, screen: &str) -> (CoreApp, Session, Vec<Session>) {
    let (mut app, mut s, helpers) = with_helpers(sb, n);
    frame(&mut app, &mut s, screen.as_bytes());
    app.pending_to_shell.clear();
    (app, s, helpers)
}

fn cols(app: &CoreApp) -> u16 {
    app.windows[0].layout.cells[0].cols
}

/// Wait out the probe and look, as the loop does after a pump.
pub(super) fn after_probe(app: &mut CoreApp) {
    std::thread::sleep(keys::PROBE + Duration::from_millis(10));
    app.windows[0].panes[0].pump();
    app.strips_tick();
    app.place_strips(0);
}

fn told_shell(app: &mut CoreApp) -> Vec<bool> {
    let frames = std::mem::take(&mut app.pending_to_shell);
    frames.iter().filter(|f| f.0 == MsgType::StripKeys).map(|f| marspot::shell_proto::decode_strip_keys(&f.1).unwrap().on).collect()
}

pub(super) fn has_keys(app: &CoreApp) -> bool {
    app.strips.keys.focus.is_some()
}

#[test]
fn helper_strip_down_in_an_empty_box_the_agent_has_no_use_for_takes_the_keys() {
    let sb = Sandbox::new();
    let screen = claude_screen(60, "", "");
    let (mut app, s, _h) = under_claude(&sb, 2, &screen);
    press(&mut app, DOWN);
    assert!(!has_keys(&app), "not before the agent had its time");
    after_probe(&mut app);
    assert!(has_keys(&app));
    assert_eq!(told_shell(&mut app), [true]);
    let l = lines(&app, s.sid);
    assert!(l[2].starts_with("❯ ◯ codex"), "the first helper is selected: {l:?}");
    press(&mut app, DOWN);
    app.place_strips(0);
    assert!(lines(&app, s.sid)[3].starts_with("❯ ◯ codex"));
    press(&mut app, DOWN);
    app.place_strips(0);
    assert!(lines(&app, s.sid)[3].starts_with("❯"), "the last stays selected");
    press(&mut app, UP);
    press(&mut app, UP);
    app.place_strips(0);
    assert!(lines(&app, s.sid)[1].starts_with("❯ ⏺ main"), "{:?}", lines(&app, s.sid));
    press(&mut app, UP);
    assert!(!has_keys(&app), "up on the first row goes back");
    assert_eq!(told_shell(&mut app), [false]);
}

#[test]
fn helper_strip_down_the_agent_uses_or_into_a_box_with_text_takes_nothing() {
    let sb = Sandbox::new();
    let (mut app, mut s, _h) = under_claude(&sb, 1, &claude_screen(60, "hi", ""));
    press(&mut app, DOWN);
    after_probe(&mut app);
    assert!(!has_keys(&app), "text in the box");
    let c = cols(&app);
    frame(&mut app, &mut s, claude_screen(c, "", "").as_bytes());
    press(&mut app, DOWN);
    // the agent answers with a menu under its box
    s.shows(c, app.windows[0].layout.cells[0].rows, claude_screen(c, "", "\r\n❯ /help").as_bytes());
    after_probe(&mut app);
    assert!(!has_keys(&app), "its screen moved");
    press(&mut app, DOWN);
    press(&mut app, LogicalKey::Char('a'));
    after_probe(&mut app);
    assert!(!has_keys(&app), "another key before it was due");
}

#[test]
fn helper_strip_a_repeat_neither_starts_nor_stops_the_probe_and_is_swallowed_once_taken() {
    let sb = Sandbox::new();
    let (mut app, _s, _h) = under_claude(&sb, 2, &claude_screen(60, "", ""));
    app.key(0, ev(DOWN, KeyState::Pressed, true), Modifiers::default());
    after_probe(&mut app);
    assert!(!has_keys(&app), "a repeat starts nothing");
    press(&mut app, DOWN);
    app.key(0, ev(DOWN, KeyState::Pressed, true), Modifiers::default());
    after_probe(&mut app);
    assert!(has_keys(&app), "nor does it stop one");
    app.key(0, ev(DOWN, KeyState::Pressed, true), Modifiers::default());
    assert_eq!(app.strips.keys.focus.as_ref().unwrap().selected, compose::Pick::Row(0), "swallowed");
    press(&mut app, ENTER);
    assert!(
        app.release_follows_press(&ev(ENTER, KeyState::Released, false), Modifiers::default()),
        "the release of a key the strip took is nobody's"
    );
}

#[test]
fn helper_strip_keys_go_back_when_the_screen_moves_before_the_next_key() {
    let sb = Sandbox::new();
    let (mut app, mut s, _h) = under_claude(&sb, 1, &claude_screen(60, "", ""));
    press(&mut app, DOWN);
    after_probe(&mut app);
    assert!(has_keys(&app));
    let c = cols(&app);
    s.shows(c, app.windows[0].layout.cells[0].rows, claude_screen(c, "", "\r\n❯ late").as_bytes());
    app.windows[0].panes[0].pump();
    app.strips_tick();
    assert!(!has_keys(&app), "taken for a while only");
}

#[test]
fn helper_strip_enter_views_esc_and_other_keys_go_back() {
    let sb = Sandbox::new();
    let (mut app, _s, helpers) = under_claude(&sb, 2, &claude_screen(60, "", ""));
    let take = |app: &mut CoreApp| {
        press(app, DOWN);
        after_probe(app);
        assert!(has_keys(app));
    };
    take(&mut app);
    press(&mut app, DOWN);
    press(&mut app, ENTER);
    assert_eq!(app.windows[0].panes[0].peeked(), Some(helpers[1].sid));
    assert!(!has_keys(&app), "the keys are the helper's");
    app.view_helper(app.windows[0].panes[0].shelld_session_id().unwrap(), None);
    take(&mut app);
    press(&mut app, ESC);
    assert!(!has_keys(&app));
    take(&mut app);
    assert!(!app.strip_key(0, &ev(LogicalKey::Char('a'), KeyState::Pressed, false), Modifiers::default()), "the agent's");
    assert!(!has_keys(&app));
}

#[test]
fn helper_strip_x_stops_a_helper_and_then_clears_its_row_once_the_shell_lets_it_through() {
    let sb = Sandbox::new();
    let (mut app, s, helpers) = under_claude(&sb, 2, &claude_screen(60, "", ""));
    press(&mut app, DOWN);
    after_probe(&mut app);
    assert!(!app.strip_key(0, &ev(LogicalKey::Char('x'), KeyState::Pressed, false), Modifiers::default()), "an old shell: x is typed");
    assert!(!has_keys(&app));
    app.shell_takes_strip_x(&encode_strip_keys(&StripKeys { window_id: app.windows[0].window_id, on: true }));
    press(&mut app, DOWN);
    after_probe(&mut app);
    app.pending_to_shell.clear();
    press(&mut app, LogicalKey::Char('x'));
    let acts: Vec<_> = app
        .pending_to_shell
        .iter()
        .filter(|f| f.0 == MsgType::HelperPanelAct)
        .map(|f| decode_helper_panel_act(&f.1).map(|a| (a.sid, a.act)).unwrap())
        .collect();
    assert_eq!(acts, [(helpers[0].sid, PanelAct::CloseHelper)], "at work: stopped");
    assert!(has_keys(&app));
    let mut record = app.pane_control.strips.of(s.sid).unwrap().clone();
    record.rows[0].state = marspot::shell_proto::StripState::Stopped;
    record.rows[0].running_since_ms = None;
    app.pane_control.strips.take(record);
    app.strips_tick();
    press(&mut app, LogicalKey::Char('x'));
    app.place_strips(0);
    let l = lines(&app, s.sid);
    assert_eq!(l.iter().filter(|l| l.contains("codex")).count(), 1, "its row is cleared: {l:?}");
}

#[test]
fn helper_strip_down_on_the_last_row_of_claudes_own_list_goes_on_into_the_strip() {
    let sb = Sandbox::new();
    let c = 60;
    let pad = " ".repeat(c - 28);
    let own = |sel: [&str; 2]| claude_screen(c as u16, "", &format!("\r\n\r\n{}⏺ main\r\n{}◯ general-purpose  job{pad}1s", sel[0], sel[1]));
    let (mut app, mut s, _h) = under_claude(&sb, 1, &own(["❯ ", "  "]));
    let mut record = app.pane_control.strips.of(s.sid).unwrap().clone();
    record.parent_agent = marspot::shell_proto::StripAgent::Claude;
    app.pane_control.strips.take(record);
    app.strips_tick();
    frame(&mut app, &mut s, own(["❯ ", "  "]).as_bytes());
    press(&mut app, DOWN);
    after_probe(&mut app);
    assert!(!has_keys(&app), "it uses the key: main is selected");
    frame(&mut app, &mut s, own(["  ", "❯ "]).as_bytes());
    press(&mut app, DOWN);
    after_probe(&mut app);
    assert!(has_keys(&app), "the last of its rows");
    assert!(lines(&app, s.sid)[0].starts_with("❯ ◯ codex"), "{:?}", lines(&app, s.sid));
    press(&mut app, UP);
    assert!(!has_keys(&app), "up gives the keys back");
}

#[test]
fn helper_strip_a_gemini_pane_has_no_keyboard() {
    let sb = Sandbox::new();
    let (mut app, s, _h) = under_claude(&sb, 1, &claude_screen(60, "", ""));
    let mut record = app.pane_control.strips.of(s.sid).unwrap().clone();
    record.parent_agent = marspot::shell_proto::StripAgent::Gemini;
    app.pane_control.strips.take(record);
    app.strips_tick();
    press(&mut app, DOWN);
    after_probe(&mut app);
    assert!(!has_keys(&app));
}

#[test]
fn helper_strip_keys_go_back_when_the_setting_is_turned_off() {
    let sb = Sandbox::new();
    let (mut app, _s, _h) = under_claude(&sb, 1, &claude_screen(60, "", ""));
    press(&mut app, DOWN);
    after_probe(&mut app);
    assert!(has_keys(&app));
    told_shell(&mut app);
    super::host_tests::turned_on(false);
    app.strips_tick();
    assert!(!has_keys(&app));
    assert_eq!(told_shell(&mut app), [false], "the shell lets x through no longer");
}

#[test]
fn helper_strip_keys_taken_look_at_each_frame_once() {
    let sb = Sandbox::new();
    let (mut app, _s, _h) = under_claude(&sb, 1, &claude_screen(60, "", ""));
    press(&mut app, DOWN);
    after_probe(&mut app);
    assert!(has_keys(&app));
    let turns = super::alloc_tests::counted(|| {
        for _ in 0..10 {
            app.strip_keys_tick(Instant::now());
        }
    });
    assert_eq!(turns, 0, "loop turns with no new frame read nothing");
    assert!(has_keys(&app));
}

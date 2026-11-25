//! A strip under Claude's own list on the alternate screen: as high as
//! its rows, once that list came a second before or went three before.

use super::compose::Pick;
use super::host_tests::{app, given, gone, lines, row, Session};
use super::keys_tests::{claude_screen, has_keys, press, DOWN, ENTER, UP};
use super::view_tests::click;
use super::*;
use crate::boot_assembly_tests::Sandbox;
use marspot::grid_shm::FLAG_ALT_SCREEN;
use marspot::shell_proto::{HelperStrip, StripAgent};

/// Claude's input box and footer, with its own list under them or not.
fn claude(cols: u16, list: bool) -> String {
    let rule = "─".repeat(cols as usize);
    let own = format!("\r\n\r\n  ⏺ main\r\n  ◯ general-purpose  job{}1s", " ".repeat(cols as usize - 28));
    format!("{rule}\r\n❯\r\n{rule}\r\n  foot{}", if list { own.as_str() } else { "" })
}

fn shown(app: &mut CoreApp, s: &mut Session, list: bool) {
    let c = app.windows[0].layout.cells[0];
    s.shows_with(c.cols, c.rows, claude(c.cols, list).as_bytes(), FLAG_ALT_SCREEN);
    app.windows[0].panes[0].pump();
    app.strips_tick();
    app.place_strips(0);
}

fn merge_due(app: &CoreApp, sid: u64) -> Option<Instant> {
    app.strips.hosts.get(&sid).and_then(|h| h.merge.due())
}

#[test]
fn helper_strip_under_claudes_own_list_takes_only_its_rows_and_follows_the_list() {
    let sb = Sandbox::new();
    let (mut app, mut s) = app(&sb);
    let c = app.windows[0].layout.cells[0];
    shown(&mut app, &mut s, false);
    let record = HelperStrip { batch: 1, rows: vec![gone(4, true)], parent_agent: StripAgent::Claude, ..HelperStrip::empty(s.sid) };
    app.pane_control.strips.take(record);
    app.strips_tick();
    assert_eq!(given(&app), 3, "alone: a blank row and main of its own");
    shown(&mut app, &mut s, true);
    assert_eq!(given(&app), 3, "the list may be gone again in a moment");
    let came = Instant::now();
    let due = merge_due(&app, s.sid).expect("the list is waited on");
    assert!(due <= came + Duration::from_secs(1), "a second after it came: {:?}", due - came);
    std::thread::sleep(Duration::from_millis(1_010));
    app.strips_tick();
    assert_eq!(given(&app), 1, "no head of its own, nothing under it");
    assert_eq!(app.helper_size(s.sid), Some((c.cols, c.rows - 2)), "a new helper is sized as the strip is");
    shown(&mut app, &mut s, false);
    let went = Instant::now();
    assert_eq!(given(&app), 1, "a prompt or a menu of claude's hides its list for a while");
    let due = merge_due(&app, s.sid).expect("the list is waited on");
    let after = due.saturating_duration_since(went);
    assert!(after > Duration::from_secs(2) && after <= Duration::from_secs(3), "three seconds after it went: {after:?}");
    let l = lines(&app, s.sid);
    assert_eq!(l.len(), 1, "{l:?}");
    assert!(l[0].starts_with("  ◯ codex"), "the row stays while the height waits, with no head: {l:?}");
}

/// A pane on Claude's alternate screen whose own list `n` helpers go on
/// from, the first of them viewed in its place.
fn viewing_under_claudes_list(sb: &Sandbox, n: usize) -> (CoreApp, Session, Vec<Session>) {
    let (mut app, mut s) = app(sb);
    let c = app.windows[0].layout.cells[0];
    shown(&mut app, &mut s, true);
    let mut helpers = Vec::new();
    for _ in 0..n {
        let (pane, h) = Session::of(c.cols, c.rows - n as u16);
        app.pane_control.shadows.family.adopted(h.sid, s.sid);
        app.shadows.push(pane);
        helpers.push(h);
    }
    let rows = helpers.iter().map(|h| row(h.sid, true)).collect();
    app.pane_control.strips.take(HelperStrip { batch: 1, rows, parent_agent: StripAgent::Claude, ..HelperStrip::empty(s.sid) });
    app.strips_tick();
    assert_eq!(given(&app), n as u16, "as many rows as it lists");
    shown(&mut app, &mut s, true);
    assert!(app.view_helper(s.sid, Some(helpers[0].sid)));
    let c = app.windows[0].layout.cells[0];
    helpers[0].shows(c.cols, c.rows, b"$ ");
    app.advance_views(Instant::now());
    assert_eq!(app.windows[0].panes[0].peeked(), Some(helpers[0].sid));
    app.strips_tick();
    assert_eq!(given(&app), n as u16, "viewing a helper resizes nothing");
    app.place_strips(0);
    (app, s, helpers)
}

/// The strip's lines with the first of `n` helpers viewed.
fn viewed_under_claudes_list(n: usize) -> Vec<String> {
    let sb = Sandbox::new();
    let (app, s, _helpers) = viewing_under_claudes_list(&sb, n);
    lines(&app, s.sid)
}

#[test]
fn helper_strip_a_helper_viewed_under_claudes_list_keeps_its_rows_before_any_head() {
    let l = viewed_under_claudes_list(1);
    assert_eq!(l.len(), 1, "{l:?}");
    assert!(l[0].starts_with("  ⏺ codex"), "the helper's row, not a blank line: {l:?}");
    let l = viewed_under_claudes_list(2);
    assert_eq!(l.len(), 2, "{l:?}");
    assert!(l[0].starts_with("  ⏺ codex") && l[1].starts_with("  ◯ codex"), "both helpers, no main: {l:?}");
}

#[test]
fn helper_strip_a_click_on_the_row_in_view_under_claudes_list_goes_back_to_the_pane() {
    let sb = Sandbox::new();
    let (mut app, s, _helpers) = viewing_under_claudes_list(&sb, 1);
    click(&mut app, s.sid, 0);
    assert_eq!(app.windows[0].panes[0].peeked(), None, "no main to click: the row in view is the way back");
    app.strips_tick();
    assert_eq!(given(&app), 1, "going back resizes nothing");
}

#[test]
fn helper_strip_up_from_the_first_row_while_viewing_under_claudes_list_selects_main() {
    let sb = Sandbox::new();
    let (mut app, s, mut helpers) = viewing_under_claudes_list(&sb, 1);
    // the helper's next frame, at the size it is viewed at: an empty box
    let c = app.windows[0].layout.cells[0];
    helpers[0].shows(c.cols, c.rows, claude_screen(c.cols, "", "").as_bytes());
    app.focused_shown_mut(0).expect("the helper in view").pump();
    press(&mut app, DOWN);
    std::thread::sleep(keys::PROBE + Duration::from_millis(10));
    app.focused_shown_mut(0).expect("the helper in view").pump();
    app.strips_tick();
    assert!(has_keys(&app), "the strip has the keys under the helper in view");
    assert_eq!(app.strips.keys.selected(app.windows[0].window_id, s.sid), Some(Pick::Row(0)));
    press(&mut app, UP);
    assert!(has_keys(&app), "up from the first row stays in the strip while a helper is viewed");
    assert_eq!(app.strips.keys.selected(app.windows[0].window_id, s.sid), Some(Pick::Main), "main, though not drawn");
    app.strips_tick();
    assert_eq!(app.strips.keys.selected(app.windows[0].window_id, s.sid), Some(Pick::Main), "kept as the keys are looked at");
    press(&mut app, ENTER);
    assert_eq!(app.windows[0].panes[0].peeked(), None, "enter on main goes back to the pane");
    assert_eq!(given(&app), 1, "going back resizes nothing");
}

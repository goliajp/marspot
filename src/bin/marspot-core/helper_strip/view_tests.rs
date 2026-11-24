//! A helper viewed in its pane's place from the strip.

use super::host_tests::{app, frame, given, lines, row, say, Session};
use super::tests::at_row;
use super::*;
use crate::boot_assembly_tests::Sandbox;

/// A window whose pane has helpers listed, each a session of the size
/// the strip leaves: the pane's session and the helpers'.
pub(super) fn with_helpers(sb: &Sandbox, n: usize) -> (CoreApp, Session, Vec<Session>) {
    let (mut app, mut s) = app(sb);
    let c = app.windows[0].layout.cells[0];
    let total = c.rows + c.strip_rows;
    let rows = super::height::rows_for(n);
    let mut helpers = Vec::new();
    for _ in 0..n {
        let (pane, h) = Session::of(c.cols, total - rows);
        app.pane_control.shadows.family.adopted(h.sid, s.sid);
        app.shadows.push(pane);
        helpers.push(h);
    }
    say(&mut app, s.sid, helpers.iter().map(|h| row(h.sid, true)).collect());
    assert_eq!(given(&app), rows);
    frame(&mut app, &mut s, b"$ ");
    (app, s, helpers)
}

/// Press and let go on line `line` of the strip.
pub(super) fn click(app: &mut CoreApp, sid: u64, line: u16) {
    let top = app.strips.view(Some(sid)).expect("a strip on screen").top_row;
    let (x, y) = at_row(app, top + line);
    app.mouse_down(0, x, y, Modifiers::default());
    app.mouse_up(0, 1, x, y);
}

#[test]
fn helper_strip_a_click_on_a_helper_views_it_and_one_on_main_comes_back() {
    let sb = Sandbox::new();
    let (mut app, s, helpers) = with_helpers(&sb, 2);
    click(&mut app, s.sid, 3);
    assert_eq!(app.windows[0].panes[0].peeked(), Some(helpers[1].sid), "the second helper");
    app.place_strips(0);
    let l = lines(&app, s.sid);
    assert_eq!(l[1], "  ◯ main", "{l:?}");
    assert!(l[2].starts_with("  ◯ codex") && l[3].starts_with("  ⏺ codex"), "{l:?}");
    click(&mut app, s.sid, 2);
    assert_eq!(app.windows[0].panes[0].peeked(), Some(helpers[0].sid), "from one helper to another");
    click(&mut app, s.sid, 1);
    assert_eq!(app.windows[0].panes[0].peeked(), None, "main is the pane itself");
    app.place_strips(0);
    assert_eq!(lines(&app, s.sid)[1], "  ⏺ main");
    assert_eq!(app.windows[0].panels.open(), None, "no panel for any of it");
}

#[test]
fn helper_strip_a_click_on_how_many_more_opens_the_panel() {
    let sb = Sandbox::new();
    let (mut app, s, _helpers) = with_helpers(&sb, 6);
    assert_eq!(lines(&app, s.sid)[7], "  ↓ 1 more");
    click(&mut app, s.sid, 7);
    assert_eq!(app.windows[0].panels.open(), Some(SurfaceKind::Helpers));
    assert_eq!(app.windows[0].panes[0].peeked(), None);
}

#[test]
fn helper_strip_a_viewed_helper_has_a_strip_of_its_own_not_one_going_on_from_claudes_list() {
    let sb = Sandbox::new();
    let (mut app, mut s, mut helpers) = with_helpers(&sb, 1);
    let c = app.windows[0].layout.cells[0];
    let rule = "─".repeat(c.cols as usize);
    let own = format!("{rule}\r\n❯\r\n{rule}\r\n  foot\r\n\r\n  ⏺ main\r\n  ◯ general-purpose  job{}1s", " ".repeat(c.cols as usize - 28));
    let mut record = app.pane_control.strips.of(s.sid).unwrap().clone();
    record.parent_agent = marspot::shell_proto::StripAgent::Claude;
    record.batch += 1;
    app.pane_control.strips.take(record);
    app.strips_tick();
    frame(&mut app, &mut s, own.as_bytes());
    assert!(lines(&app, s.sid)[0].starts_with("  ◯ codex"), "under claude's own list");
    assert!(app.view_helper(s.sid, Some(helpers[0].sid)));
    // the strip took all its rows under claude: the helper is shown once
    // it has the pane's new size
    let c = app.windows[0].layout.cells[0];
    helpers[0].shows(c.cols, c.rows, b"$ ");
    app.advance_views(Instant::now());
    assert_eq!(app.windows[0].panes[0].peeked(), Some(helpers[0].sid));
    app.place_strips(0);
    let l = lines(&app, s.sid);
    assert_eq!(l[..2], ["", "  ◯ main"], "{l:?}");
    assert!(l[2].starts_with("  ⏺ codex"), "{l:?}");
}

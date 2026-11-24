//! The header line of a window with one pane while that pane shows a
//! helper in its place, against real `marspot-session` processes.

use super::super::shadows::tests::{sandbox, wait_for};
use super::tests::{app_with_helper, cmd_w, size_of, turn, view};
use super::wait::has_size;
use super::*;
use marspot::input::{KeyState, LogicalKey, NamedKey};

/// What window 0's header says, from titles made as the frame makes
/// them: the pane's own name, then what it shows.
fn line(app: &CoreApp) -> String {
    let titles: Vec<String> =
        (0..app.windows[0].panes.len()).map(|i| format!("host{}", app.view_label(0, i).unwrap_or_default())).collect();
    app.view_line(0, &titles).to_string()
}

#[test]
fn peek_line_names_the_helper_in_a_lone_pane_without_resizing_either() {
    let sb = sandbox();
    let (mut app, rx, host, sid) = app_with_helper(&sb);
    assert_eq!(app.windows[0].panes.len(), 1);
    assert_eq!(app.windows[0].layout.cell_title_h, 0.0, "a lone pane has no title strip");
    assert_eq!(line(&app), "", "nothing is said while the pane shows itself");
    wait_for("the host has its layout's size", || {
        turn(&mut app, &rx);
        has_size(&app.windows[0].panes[0], &app.windows[0].layout.cells[0])
    });
    let cell = app.windows[0].layout.cells[0];
    view(&mut app, &rx, 20, sid);
    let said = line(&app);
    assert!(said.starts_with("host › ") && said.ends_with("⌘W"), "{said:?}");
    assert_eq!(said, format!("host{}", app.view_label(0, 0).unwrap()), "the title strip's words");

    // the line takes no row: the layout and both sessions keep their size
    app.rebuild_layout(0);
    turn(&mut app, &rx);
    let after = app.windows[0].layout.cells[0];
    assert_eq!((after.cols, after.rows, after.strip_rows), (cell.cols, cell.rows, cell.strip_rows));
    assert_eq!(size_of(&app.windows[0].panes[0]), (cell.cols, cell.rows), "the host");
    assert_eq!(size_of(app.shown(0, 0)), (cell.cols, cell.rows), "the helper");

    cmd_w(&mut app);
    assert!(app.windows[0].panes[0].peek.is_none());
    assert_eq!(line(&app), "", "the line goes with the view");
    assert_eq!(app.focused_session(0), Some(host));
}

#[test]
fn peek_line_is_not_drawn_where_the_title_strip_says_it() {
    let sb = sandbox();
    let (mut app, rx, _host, sid) = app_with_helper(&sb);
    app.windows[0].panes.push(Pane::new_vacant(4242, 80, 24));
    (app.windows[0].grid_cols, app.windows[0].grid_rows) = (2, 1);
    app.rebuild_layout(0);
    assert!(app.windows[0].layout.cell_title_h > 0.0);
    view(&mut app, &rx, 20, sid);
    assert!(app.view_label(0, 0).is_some(), "the strip names the helper");
    assert_eq!(line(&app), "");
}

#[test]
fn peek_line_says_a_helper_that_went_has_ended_until_the_view_is_left() {
    let sb = sandbox();
    let (mut app, rx, _host, sid) = app_with_helper(&sb);
    view(&mut app, &rx, 20, sid);
    app.inject_input(sid, b"q\r");
    wait_for("it ended", || {
        turn(&mut app, &rx);
        app.shadows.is_empty()
    });
    let said = line(&app);
    assert!(said.contains(t(Msg::HelperEnded)), "{said:?}");
    app.key(0, super::tests::ev(LogicalKey::Named(NamedKey::Escape), KeyState::Pressed, false), Modifiers::default());
    assert!(app.windows[0].panes[0].peek.is_none());
    assert_eq!(line(&app), "");
}

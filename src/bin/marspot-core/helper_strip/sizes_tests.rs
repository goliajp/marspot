//! The size helpers are opened at, and kept at as their strip grows.

use super::host_tests::{app, given, row, say, Session};
use super::*;
use crate::boot_assembly_tests::Sandbox;
use marspot::shell_proto::PaneSpawn;

#[test]
fn helper_strip_a_helper_is_sized_for_the_strip_it_will_be_listed_in() {
    let sb = Sandbox::new();
    let (mut app, s) = app(&sb);
    let c = app.windows[0].layout.cells[0];
    let total = c.rows + c.strip_rows;
    assert_eq!(app.helper_size(s.sid), Some((c.cols, total - 3)), "a blank row, main and its own");
    let (a, mut sa) = Session::of(c.cols, total - 3);
    app.shadows.push(a);
    say(&mut app, s.sid, vec![row(sa.sid, true)]);
    assert_eq!(given(&app), 3);
    assert_eq!(app.helper_size(s.sid), Some((c.cols, total - 4)), "and the next one's row");
    std::thread::sleep(Duration::from_millis(510));
    app.strips_tick();
    assert_eq!(sa.resizes(&app.shadows[0]), [], "the strip grew as it was sized for");
}

#[test]
fn helper_strip_claude_on_its_main_screen_has_its_helpers_sized_for_the_rows_with_a_blank_one() {
    let sb = Sandbox::new();
    let (mut app, s) = app(&sb);
    let c = app.windows[0].layout.cells[0];
    app.pane_agent_tui.insert(s.sid, true);
    assert_eq!(app.helper_size(s.sid), Some((c.cols, c.rows - 4)), "a head, its row and a blank row");
}

#[test]
fn helper_strip_only_a_helper_opened_without_a_size_gets_one_chosen() {
    let sb = Sandbox::new();
    let (app, s) = app(&sb);
    let c = app.windows[0].layout.cells[0];
    let spawn = |helper, cols, rows| PaneSpawn {
        req_id: 1,
        shadow: true,
        no_focus: true,
        helper,
        parent_sid: s.sid,
        idle_s: 0,
        cols,
        rows,
        cwd: String::new(),
    };
    assert_eq!(app.spawn_size(&spawn(true, 0, 0)), (c.cols, c.rows - 3));
    assert_eq!(app.spawn_size(&spawn(false, 0, 0)), (120, 40), "a pane opened by pane.open");
    assert_eq!(app.spawn_size(&spawn(true, 90, 30)), (90, 30));
}

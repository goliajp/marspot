//! A shadow pane no helper runs in, under a pane with no helpers: it is
//! counted, listed, and reached from the strip as a helper is.

use super::super::host_tests::{app, frame, given, lines, row, say, Session};
use super::super::keys_tests::{after_probe, claude_screen, has_keys, press, DOWN, ENTER};
use super::super::view_tests::click;
use super::super::*;
use crate::boot_assembly_tests::Sandbox;
use marspot::shell_proto::{HelperStrip, StripAgent, StripRow, StripState};

/// What the shell says of a pane opened by `pane.open` that runs claude.
fn plain(sid: u64) -> StripRow {
    StripRow {
        agent: StripAgent::Claude,
        plain: true,
        worked_ms: 0,
        running_since_ms: None,
        ended_ms: None,
        tokens: None,
        name: "claude".into(),
        task: "madpdf".into(),
        state: StripState::Working,
        ..row(sid, false)
    }
}

/// Let the view of `p` asked for in pane 0's place begin: `p` takes
/// the size it was asked for, as a session does.
fn sized_to_view(app: &mut CoreApp, p: &mut Session) {
    let c = app.windows[0].layout.cells[0];
    assert_eq!(p.resizes(&app.shadows[0]), [(c.cols, c.rows)], "sized to be viewed, as a helper is");
    p.shows(c.cols, c.rows, b"$ ");
    app.shadows[0].pump();
    app.advance_views(Instant::now());
}

/// A window whose pane has one plain shadow pane, of the size
/// `pane.open` gives, and no helpers.
fn with_plain(sb: &Sandbox) -> (CoreApp, Session, Session) {
    let (mut app, s) = app(sb);
    let (pane, p) = Session::of(120, 40);
    app.pane_control.shadows.family.adopted(p.sid, s.sid);
    app.shadows.push(pane);
    app.shadows_changed();
    (app, s, p)
}

#[test]
fn helper_strip_a_plain_shadow_pane_is_counted_listed_and_viewed_by_a_click() {
    let sb = Sandbox::new();
    let (mut app, mut s, mut p) = with_plain(&sb);
    assert_eq!(app.shadows_counted(s.sid), 1, "before the shell says anything: the shadows");
    say(&mut app, s.sid, vec![plain(p.sid)]);
    app.pane_control.strips.take(HelperStrip::all_told());
    assert_eq!(given(&app), 3, "a blank row, main and its row");
    frame(&mut app, &mut s, b"$ ");
    assert!(app.strips.drawn(s.sid), "drawn: the title draws no +N");
    assert_eq!(app.shadows_counted(s.sid), 1);
    let l = lines(&app, s.sid);
    assert_eq!(l[1], "  ⏺ main");
    assert!(l[2].starts_with("  ◯ claude") && l[2].ends_with("madpdf"), "no figures: {l:?}");
    click(&mut app, s.sid, 2);
    sized_to_view(&mut app, &mut p);
    assert_eq!(app.windows[0].panes[0].peeked(), Some(p.sid));
    app.place_strips(0);
    click(&mut app, s.sid, 1);
    assert_eq!(app.windows[0].panes[0].peeked(), None);
}

#[test]
fn helper_strip_a_plain_shadow_pane_keeps_the_size_it_was_opened_with() {
    let sb = Sandbox::new();
    let (mut app, s, mut p) = with_plain(&sb);
    say(&mut app, s.sid, vec![plain(p.sid)]);
    std::thread::sleep(Duration::from_millis(510));
    app.strips_tick();
    assert_eq!(p.resizes(&app.shadows[0]), [], "not a helper's: no size follows its pane");
}

#[test]
fn helper_strip_a_plain_shadow_pane_is_reached_from_the_keyboard() {
    let sb = Sandbox::new();
    let (mut app, mut s, mut p) = with_plain(&sb);
    say(&mut app, s.sid, vec![plain(p.sid)]);
    let screen = claude_screen(app.windows[0].layout.cells[0].cols, "", "");
    frame(&mut app, &mut s, screen.as_bytes());
    app.pending_to_shell.clear();
    press(&mut app, DOWN);
    after_probe(&mut app);
    assert!(has_keys(&app), "the strip took the keys");
    press(&mut app, ENTER);
    sized_to_view(&mut app, &mut p);
    assert_eq!(app.windows[0].panes[0].peeked(), Some(p.sid));
}

#[test]
fn helper_strip_the_title_counts_the_open_panes_the_strip_lists() {
    let sb = Sandbox::new();
    let (mut app, s, p) = with_plain(&sb);
    let gone = StripRow { gone: true, state: StripState::Exited, ..row(77, false) };
    say(&mut app, s.sid, vec![plain(p.sid), row(78, true), gone]);
    assert_eq!(app.shadows_counted(s.sid), 2, "a row whose pane is gone is no shadow pane");
    say(&mut app, s.sid, vec![]);
    app.pane_control.strips.take(HelperStrip::all_told());
    assert_eq!(app.shadows_counted(s.sid), 0, "the shell lists none: none is counted, whatever the core has");
    assert_eq!(app.shadows_counted(s.sid + 1), 0);
}

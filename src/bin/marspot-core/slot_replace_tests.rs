//! "Replace with…" against real `marspot-session` processes.

use super::*;
use crate::draw::sidebar_tree::RowPane::{Other, Slot};
use crate::shadows::tests::{app_with_parent, drive, open_shadow, reaped, sandbox, wait_for};
use marspot::render::SidebarTree::{LastChild, Parent, Top};
use marspot_term::session_registry as reg;

fn rows(app: &mut CoreApp) -> Vec<(crate::draw::sidebar_tree::RowPane, marspot::render::SidebarTree, bool)> {
    app.sidebar_fresh(0);
    app.sidebar_rows(0).iter().map(|r| (r.pane, r.tree, r.hollow)).collect()
}

fn screen(app: &CoreApp, sid: u64) -> String {
    let pane = app.windows.iter().flat_map(|w| w.panes.iter()).find(|p| p.shelld_session_id() == Some(sid));
    pane.map(|p| p.screen_rows().into_iter().map(|r| r.text).collect::<Vec<_>>().join("\n")).unwrap_or_default()
}

/// A child shown in its parent's slot and back: whose child each is
/// never changes, the parent runs on while hidden and what it gets then
/// is on its screen when it is shown again, the keyboard follows the
/// slot, and the sidebar's dots swap.
#[test]
fn slot_replace_shows_a_hidden_child_and_back_and_changes_no_parent() {
    let sb = sandbox();
    let (mut app, rx, psid) = app_with_parent(&sb);
    win!(app, 0).sidebar_collapsed = false;
    app.rebuild_layout(0);
    let sid = open_shadow(&mut app, &rx, 7, psid);
    app.shadows_changed();
    CoreApp::note_pane_code(psid);
    assert_eq!(rows(&mut app), vec![(Slot(0), Top, false), (Other(sid), LastChild, true)]);

    let menu = app.replace_row();
    let labels: Vec<&str> = menu.submenu.iter().map(|i| if i.divider { "—" } else { i.label.as_str() }).collect();
    assert_eq!(labels[..2], [t(Msg::MenuNewShell), "—"], "a new shell first, then a divider: {labels:?}");
    let code = marspot_term::pane_code::known(psid).expect("the parent's code");
    assert!(labels[2].ends_with(&strings::text(Arg::ChildOf(code))), "the child says whose it is: {labels:?}");

    app.replace_slot(0, 0, Replace::Hidden(sid));
    assert_eq!(app.find_window_pane_by_sid(sid), Some((0, 0)));
    assert_eq!((app.shadow_index(psid).is_some(), app.focused_session(0)), (true, Some(sid)), "the keyboard is the shown pane's");
    assert_eq!(reg::read_session_entry(sid).unwrap().shadow_of, psid, "still the parent's child");
    assert_eq!(reg::read_session_entry(psid).unwrap().shadow_of, 0, "the parent is still top-level");
    let ppid = reg::read_session_entry(psid).unwrap().pid;
    assert!(!reaped(ppid), "the parent runs on in no window");
    assert_eq!(rows(&mut app), vec![(Other(psid), Parent, true), (Slot(0), LastChild, false)], "the dots swapped");

    // typed into while hidden: its terminal echoes it on its own screen
    app.pane_by_sid_mut(psid).unwrap().session_mut().forward_inject_input(b"hidden42");
    std::thread::sleep(Duration::from_millis(300));
    app.replace_slot(0, 0, Replace::Hidden(psid));
    assert_eq!(app.find_window_pane_by_sid(psid), Some((0, 0)), "and back");
    assert!(app.shadow_index(sid).is_some());
    assert_eq!(reg::read_session_entry(sid).unwrap().shadow_of, psid);
    assert_eq!(rows(&mut app), vec![(Slot(0), Top, false), (Other(sid), LastChild, true)], "as it was");
    wait_for("what the hidden pane got is on its screen", || {
        drive(&mut app, &rx);
        app.shown_mut(0, 0).pump();
        screen(&app, psid).contains("hidden42")
    });
}

/// With no pane hidden the row is only "New shell", which starts a
/// top-level shell in the slot, in the directory of the pane it
/// replaces, and keeps that pane running.
#[test]
fn slot_replace_new_shell_with_nothing_hidden_starts_where_the_pane_was() {
    let sb = sandbox();
    let (mut app, rx, psid) = app_with_parent(&sb);
    let menu = app.replace_row();
    assert_eq!(menu.submenu.len(), 1, "no divider with nothing hidden");
    assert_eq!(menu.submenu[0].label, t(Msg::MenuNewShell));
    let mut want = String::new();
    wait_for("the pane's shell has a directory", || {
        want = crate::spawn::live_cwd_of(psid);
        !want.is_empty()
    });

    app.replace_slot(0, 0, Replace::NewShell);
    let new = win!(app, 0).panes[0].shelld_session_id().expect("a session");
    assert_ne!(new, psid);
    assert_eq!(win!(app, 0).panes.len(), 1, "in the slot, not beside it");
    assert!(app.shadow_index(psid).is_some() && !reaped(reg::read_session_entry(psid).unwrap().pid));
    assert_eq!(app.focused_session(0), Some(new));
    wait_for("the new shell runs in the same directory", || {
        drive(&mut app, &rx);
        crate::spawn::live_cwd_of(new) == want
    });
    assert_eq!(reg::read_session_entry(new).unwrap().shadow_of, 0, "top-level");
}

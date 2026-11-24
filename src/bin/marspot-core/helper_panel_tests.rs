//! The helper panel in the core, against real `marspot-session`
//! processes.

use super::super::shadows::tests::{app_with_parent, drive, open_shadow, sandbox, wait_done, wait_for};
use super::*;
use marspot::shell_proto::{decode_helper_panel_act, encode_pane_retire, PaneRetire, RetireMode};

fn acts(app: &mut CoreApp) -> Vec<(u64, PanelAct, CwdControl)> {
    let frames = std::mem::take(&mut app.pending_to_shell);
    frames
        .iter()
        .filter(|(t, _)| *t == MsgType::HelperPanelAct)
        .map(|(_, p)| decode_helper_panel_act(p).map(|a| (a.sid, a.act, a.want)).unwrap())
        .collect()
}

/// What building a frame of the open panel does to it.
fn draw(app: &mut CoreApp) -> Rc<Vec<HelperGroup>> {
    let inset = app.windows[0].layout.top_inset;
    let hp = app.helpers_render(0).unwrap();
    hp.surface.borrow_mut().lay_out(app.renderer.font_mut(), &hp, inset);
    hp.groups
}

/// The first place of the open panel that asks `want`.
fn spot(app: &mut CoreApp, want: Ask) -> Option<(f64, f64)> {
    let r = app.helpers_rect(0);
    let steps = |from: f64, len: f64| (0..(len / 4.0) as usize).map(move |i| from + i as f64 * 4.0);
    let mut all = steps(r.y_top, r.h).flat_map(|y| steps(r.x, r.w).map(move |x| (x, y)));
    all.find(|p| app.helper_ask_at(0, *p) == want)
}

fn press_at(app: &mut CoreApp, (x, y): (f64, f64)) {
    let on = app.tree_panel_press_on(0, SurfaceKind::Helpers, x, y);
    assert_eq!(on, marspot::ui::components::surface_stack::PressOn::Content);
    app.panel_content_press(0, SurfaceKind::Helpers, x, y);
}

/// Draw, then press the first place of the open panel that asks `want`.
fn press(app: &mut CoreApp, want: Ask) {
    draw(app);
    let p = spot(app, want).unwrap_or_else(|| panic!("{want:?} is nowhere on the panel"));
    press_at(app, p);
}

#[test]
fn helper_panel_buttons_ask_the_shell_and_move_the_pane() {
    let sb = sandbox();
    let (mut app, rx, psid) = app_with_parent(&sb);
    (app.windows[0].w_phys, app.windows[0].h_phys) = (1600.0, 1000.0);
    let a = open_shadow(&mut app, &rx, 7, psid);
    let b = open_shadow(&mut app, &rx, 8, a);
    assert_eq!(app.pane_control.shadows.family.count(psid), 2, "its own and the one below that");

    app.open_helpers(0, psid);
    let groups = draw(&mut app);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].rows.iter().map(|r| (r.sid, r.parent)).collect::<Vec<_>>(), [(a, psid), (b, a)]);
    assert!(groups[0].rows.iter().all(|r| r.started_unix.is_some_and(|t| now_unix() - t < 120)), "{groups:?}");
    acts(&mut app);

    press(&mut app, Ask::Close(1));
    press(&mut app, Ask::CloseAll(0));
    press(&mut app, Ask::Control(0));
    assert_eq!(
        acts(&mut app),
        [
            (b, PanelAct::CloseHelper, CwdControl::Flip),
            (psid, PanelAct::CloseAllOfParent, CwdControl::Flip),
            (psid, PanelAct::ToggleCwdControl, CwdControl::Deny)
        ]
    );

    press(&mut app, Ask::Move(0));
    wait_for("the pane is in the window", || {
        drive(&mut app, &rx);
        app.find_window_pane_by_sid(a).is_some()
    });
    assert!(acts(&mut app).is_empty(), "moving is the core's own");
    assert_eq!(app.pane_control.shadows.family.count(psid), 0);
    assert_eq!(app.pane_control.shadows.family.count(a), 1, "what was below it is now its own");
    assert!(app.pane_control.reports.frames().all(|f| f.msg_type != MsgType::PaneDone), "nobody asked");
}

#[test]
fn helper_panel_follows_titles_closes_and_the_deny_list() {
    let sb = sandbox();
    let (mut app, rx, psid) = app_with_parent(&sb);
    let a = open_shadow(&mut app, &rx, 7, psid);
    app.open_helpers(0, 0);
    let rows = draw;
    assert_eq!(rows(&mut app)[0].control, Control::Allowed);

    app.windows[0].needs_render = false;
    app.set_pane_title(a, "codex · a task".into());
    assert!(app.windows[0].needs_render, "a shadow's title is on the panel");
    assert_eq!(rows(&mut app)[0].rows[0].doing, "codex · a task");

    let cwd = shell_of(psid).1;
    marspot::control_deny::set(&marspot::control_deny::path(), &cwd, Some(true)).unwrap();
    wait_for("the panel heard of the deny list", || {
        app.helper_panel_tick();
        rows(&mut app)[0].control == Control::Denied
    });
    acts(&mut app);
    press(&mut app, Ask::Control(0));
    assert_eq!(acts(&mut app), [(psid, PanelAct::ToggleCwdControl, CwdControl::Allow)]);
    let above = std::path::Path::new(&cwd).parent().unwrap().to_str().unwrap();
    marspot::control_deny::set(&marspot::control_deny::path(), above, Some(true)).unwrap();
    wait_for("the panel heard of the line above", || {
        app.helper_panel_tick();
        rows(&mut app)[0].control == Control::DeniedAbove
    });
    assert_eq!(spot(&mut app, Ask::Control(0)), None, "not this directory's to allow");
    app.helpers_do(0, Ask::Control(0));
    assert!(acts(&mut app).is_empty());

    let close = encode_pane_retire(&PaneRetire { req_id: 9, sid: a, mode: RetireMode::Delete, shadow_only: false });
    app.route_pane_control(Frame::new(MsgType::PaneRetire, close));
    assert_eq!(wait_done(&mut app, &rx, 9).code, marspot::shell_proto::DoneCode::Ok);
    assert!(rows(&mut app).is_empty(), "nothing left to list");
    assert_eq!(app.pane_control.shadows.family.count(psid), 0);
}

#[test]
fn helper_panel_opens_from_the_badge_menu() {
    let sb = sandbox();
    let (mut app, rx, psid) = app_with_parent(&sb);
    open_shadow(&mut app, &rx, 7, psid);
    let item = marspot::shell_proto::PaneBadgeMenuItem { label: "row".to_string(), tag: 3 };
    app.open_pane_badge_menu(0, psid, 10.0, 10.0, vec![item]);
    let items = app.windows[0].context_menu.as_ref().unwrap().items.clone();
    assert_eq!(items.last().unwrap().action_tag, MENU_TAG);
    acts(&mut app);
    app.pick_badge_row(0, psid, MENU_TAG);
    assert!(app.pending_to_shell.is_empty(), "the row is the core's");
    assert!(app.windows[0].context_menu.is_none());
    assert_eq!(app.windows[0].panels.open(), Some(SurfaceKind::Helpers));
    assert_eq!(app.windows[0].helpers_panel.as_ref().unwrap().of, psid);
}

/// The rows changed and no frame was built since: a press is on what is
/// on screen, the row that went, and on no other.
#[test]
fn helper_panel_a_press_is_on_the_rows_last_drawn() {
    let sb = sandbox();
    let (mut app, rx, psid) = app_with_parent(&sb);
    let a = open_shadow(&mut app, &rx, 7, psid);
    let b = open_shadow(&mut app, &rx, 8, psid);
    app.open_helpers(0, psid);
    assert!(draw(&mut app)[0].rows.len() == 2);
    let top = app.helpers_rect(0).y_top;
    let (close, go) = (spot(&mut app, Ask::Close(0)).unwrap(), spot(&mut app, Ask::Move(0)).unwrap());
    let retire = encode_pane_retire(&PaneRetire { req_id: 9, sid: a, mode: RetireMode::Delete, shadow_only: false });
    app.route_pane_control(Frame::new(MsgType::PaneRetire, retire));
    wait_done(&mut app, &rx, 9);
    acts(&mut app);

    app.tree_panel_hover(0, SurfaceKind::Helpers, close.0, close.1);
    press_at(&mut app, close);
    assert_eq!(acts(&mut app), [(a, PanelAct::CloseHelper, CwdControl::Flip)]);
    press_at(&mut app, go);
    for _ in 0..20 {
        drive(&mut app, &rx);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(app.find_window_pane_by_sid(b).is_none(), "the other row's pane did not move");

    // the next frame shows one row, with the panel's top where it was
    assert_eq!(draw(&mut app)[0].rows.iter().map(|r| r.sid).collect::<Vec<_>>(), [b]);
    assert_eq!(app.helpers_rect(0).y_top, top);
    press_at(&mut app, close);
    assert_eq!(acts(&mut app), [(b, PanelAct::CloseHelper, CwdControl::Flip)]);
    app.windows[0].h_phys += 200.0;
    draw(&mut app);
    assert_ne!(app.helpers_rect(0).y_top, top, "placed again in another window");
}

#[test]
fn helper_panel_a_title_of_an_ended_session_is_not_kept() {
    let sb = sandbox();
    let (mut app, _rx, psid) = app_with_parent(&sb);
    app.set_pane_title(psid, "a".into());
    app.set_pane_title(u64::MAX - 5, "b".into());
    assert_eq!(app.pane_titles.keys().collect::<Vec<_>>(), [&psid]);
    // a window's panes come after the core starts
    std::fs::create_dir_all(session_registry::session_dir(u64::MAX - 6)).unwrap();
    app.set_pane_title(u64::MAX - 6, "c".into());
    assert_eq!(app.pane_titles.len(), 2);
}

/// A move asked on the panel that failed is said on the pane's row,
/// until it is asked again.
#[test]
fn helper_panel_says_on_the_row_a_move_that_failed() {
    let sb = sandbox();
    let (mut app, rx, psid) = app_with_parent(&sb);
    let a = open_shadow(&mut app, &rx, 7, psid);
    app.set_pane_title(a, "codex · a task".into());
    app.open_helpers(0, psid);
    press(&mut app, Ask::Move(0));
    // the session did not answer in time
    app.expire_places(Instant::now() + Duration::from_secs(4));
    let failed = format!("{} · codex · a task", marspot::ui::strings::t(marspot::ui::strings::Msg::HelperNotMoved));
    assert_eq!(draw(&mut app)[0].rows[0].doing, failed);
    wait_for("its late answer is undone", || {
        drive(&mut app, &rx);
        app.pane_control.shadows.places.expired.is_empty()
    });
    assert!(app.find_window_pane_by_sid(a).is_none());
    assert_eq!(draw(&mut app)[0].rows[0].doing, failed, "still said");

    press(&mut app, Ask::Move(0));
    assert_eq!(draw(&mut app)[0].rows[0].doing, "codex · a task", "asked again");
    wait_for("the pane is in the window", || {
        drive(&mut app, &rx);
        app.find_window_pane_by_sid(a).is_some()
    });
}

/// A move the core cannot even ask of the session fails at once, and is
/// said on the row the same way.
#[test]
fn helper_panel_says_on_the_row_a_move_its_session_could_not_be_asked() {
    let sb = sandbox();
    let (mut app, rx, psid) = app_with_parent(&sb);
    let a = open_shadow(&mut app, &rx, 7, psid);
    app.set_pane_title(a, "codex · a task".into());
    app.open_helpers(0, psid);
    app.reconnecting.insert(a);
    press(&mut app, Ask::Move(0));
    let failed = format!("{} · codex · a task", marspot::ui::strings::t(marspot::ui::strings::Msg::HelperNotMoved));
    assert_eq!(draw(&mut app)[0].rows[0].doing, failed);
    assert!(app.pane_control.shadows.places.placing.is_empty(), "nothing waits on the session");
    app.reconnecting.remove(&a);
    press(&mut app, Ask::Move(0));
    wait_for("the pane is in the window", || {
        drive(&mut app, &rx);
        app.find_window_pane_by_sid(a).is_some()
    });
}

/// A close the panel asked of a shadow pane that the core reads after
/// the pane went to a window is refused, and the pane stays there.
#[test]
fn helper_panel_close_of_a_pane_moved_to_a_window_meanwhile_is_refused() {
    let sb = sandbox();
    let (mut app, rx, psid) = app_with_parent(&sb);
    let a = open_shadow(&mut app, &rx, 7, psid);
    let b = open_shadow(&mut app, &rx, 8, psid);
    app.open_helpers(0, psid);
    press(&mut app, Ask::Move(0));
    wait_for("the pane is in the window", || {
        drive(&mut app, &rx);
        app.find_window_pane_by_sid(a).is_some()
    });
    let retire = |req_id, sid| {
        let m = PaneRetire { req_id, sid, mode: RetireMode::Delete, shadow_only: true };
        Frame::new(MsgType::PaneRetire, encode_pane_retire(&m))
    };
    app.route_pane_control(retire(9, a));
    let done = wait_done(&mut app, &rx, 9);
    assert_eq!((done.code, done.text.as_str()), (marspot::shell_proto::DoneCode::Other, marspot::shell_proto::IN_A_WINDOW_TEXT));
    assert!(app.find_window_pane_by_sid(a).is_some(), "still in the window");
    assert!(session_registry::session_dir(a).exists(), "and its directory kept");
    app.route_pane_control(retire(10, b));
    assert_eq!(wait_done(&mut app, &rx, 10).code, marspot::shell_proto::DoneCode::Ok, "a shadow pane is closed");
    assert!(app.shadow_index(b).is_none());
}

/// A deny list that is a symlink is watched where the link leads now,
/// not where it led when the panel opened.
#[test]
fn helper_panel_deny_list_is_watched_where_its_link_leads_now() {
    let dir = std::env::temp_dir().join(format!("helper-panel-link-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("kept")).unwrap();
    let (b, link) = (dir.join("kept/b"), dir.join("control-deny.tsv"));
    std::fs::write(dir.join("kept/a"), "").unwrap();
    std::fs::write(&b, "").unwrap();
    std::os::unix::fs::symlink("kept/a", &link).unwrap();
    let (tx, heard) = std::sync::mpsc::channel();
    let _w = Watcher::spawn(&moves::deny_watched(&link), move || {
        let _ = tx.send(());
    }).unwrap();
    let next = dir.join("next");
    std::os::unix::fs::symlink("kept/b", &next).unwrap();
    std::fs::rename(&next, &link).unwrap();
    heard.recv_timeout(Duration::from_secs(5)).expect("the link was pointed elsewhere");
    while heard.recv_timeout(Duration::from_millis(200)).is_ok() {}
    std::fs::write(&b, "/w\n").unwrap();
    heard.recv_timeout(Duration::from_secs(5)).expect("the file it leads to now was written");
    std::fs::remove_dir_all(&dir).unwrap();
}

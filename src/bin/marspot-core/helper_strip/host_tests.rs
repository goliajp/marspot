//! Records turned into rows under real panes.

use super::tests::{at_row, text};
use super::*;
use crate::boot_assembly_tests::Sandbox;
use crate::window_state_tests::{app_in, win};
use marspot::grid_shm::{GridShmReader, GridShmWriter, SessionRegions};
use marspot::pane::L3Conn;
use marspot::shell_proto::{encode_ping, HelperStrip, Next, StripAgent, StripRow, StripState};
use marspot_term::grid::Grid;
use marspot_term::session_registry as reg;
use marspot_term::terminal::Terminal;
use std::os::unix::net::UnixStream;

pub(super) fn row(sid: u64, works: bool) -> StripRow {
    let now = now_ms();
    StripRow {
        sid,
        number: Some(sid as u16),
        state: if works { StripState::Working } else { StripState::Idle },
        agent: StripAgent::Codex,
        gone: false,
        had_tool: true,
        started_ms: now,
        worked_ms: 65_000,
        running_since_ms: works.then_some(now),
        ended_ms: (!works).then_some(now),
        tokens: Some(12_000),
        name: "codex".into(),
        model: String::new(),
        task: "look around".into(),
    }
}

/// A helper whose pane is gone: it needs no shadow to be ready.
pub(super) fn gone(sid: u64, works: bool) -> StripRow {
    StripRow { gone: true, ..row(sid, works) }
}

/// A session a core can be attached to, and its end of the connection.
pub(super) struct Session {
    pub sid: u64,
    writer: GridShmWriter,
    control: UnixStream,
}

impl Session {
    pub(super) fn of(cols: u16, rows: u16) -> (Pane, Self) {
        let sid = reg::allocate_next_session_id().unwrap() + 0xF5_0000_0000 + std::process::id() as u64;
        let mut writer = GridShmWriter::create(cols, rows).expect("shm region");
        writer.publish(&Grid::new(cols, rows), 0, 0);
        // SAFETY: dup of an fd the writer holds open
        let fd = unsafe { libc::dup(writer.fd()) };
        assert!(fd >= 0, "dup of the shm fd");
        let reader = GridShmReader::from_fd(fd).expect("shm reader");
        let (ours, control) = UnixStream::pair().expect("socketpair");
        let (_tx, rx) = std::sync::mpsc::channel();
        // our own pid: `reattach` never signals it
        let conn = L3Conn::reattach(std::process::id() as i32, ours, reader, SessionRegions::for_session(sid), rx, sid);
        (Pane::new_l3(conn), Session { sid, writer, control })
    }

    pub(super) fn shows(&mut self, cols: u16, rows: u16, bytes: &[u8]) {
        self.shows_with(cols, rows, bytes, 0);
    }

    pub(super) fn shows_with(&mut self, cols: u16, rows: u16, bytes: &[u8], flags: u32) {
        let mut t = Terminal::new(cols, rows);
        t.feed(bytes);
        self.writer.publish(t.grid(), 0, flags);
    }

    /// The sizes the core asked for since the last look.
    pub(super) fn resizes(&mut self, pane: &Pane) -> Vec<(u16, u16)> {
        pane.session().send_pane_control(Frame::new(MsgType::Ping, encode_ping(0)));
        self.control.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut got = Vec::new();
        loop {
            match Frame::read_next(&mut self.control).expect("a frame").expect("the core still there") {
                Next::Frame(f) if f.msg_type == MsgType::Ping => return got,
                Next::Frame(f) if f.msg_type == MsgType::GridResize => {
                    got.push(marspot::shell_proto::decode_grid_resize(&f.payload).unwrap())
                }
                _ => {}
            }
        }
    }
}

pub(super) fn turned_on(on: bool) {
    marspot::settings::set_for_test(marspot::settings::Settings { helper_strip: on, ..Default::default() });
}

/// A window of one attached pane, its size that of the window's cell.
pub(super) fn app(sb: &Sandbox) -> (CoreApp, Session) {
    turned_on(true);
    let mut probe = app_in(sb.dir(), vec![win(1, vec![Pane::new_vacant(1, 60, 16)])]);
    probe.rebuild_layout(0);
    let c = probe.windows[0].layout.cells[0];
    let (pane, session) = Session::of(c.cols, c.rows);
    let mut app = app_in(sb.dir(), vec![win(1, vec![pane])]);
    app.strips.on = true;
    app.rebuild_layout(0);
    (app, session)
}

pub(super) fn say(app: &mut CoreApp, root: u64, rows: Vec<StripRow>) {
    let batch = rows.len().min(1) as u64;
    app.pane_control.strips.take(HelperStrip { batch, rows, parent_agent: StripAgent::Codex, ..HelperStrip::empty(root) });
    app.strips_tick();
}

pub(super) fn given(app: &CoreApp) -> u16 {
    app.windows[0].layout.cells[0].strip_rows
}

pub(super) fn lines(app: &CoreApp, sid: u64) -> Vec<String> {
    let v = app.strips.view(Some(sid)).expect("a strip on screen");
    (0..v.grid.rows()).map(|r| text(v.grid, r).trim_end().to_string()).collect()
}

/// Draw as a frame would, the session having taken its new size.
pub(super) fn frame(app: &mut CoreApp, s: &mut Session, bytes: &[u8]) {
    let c = app.windows[0].layout.cells[0];
    s.shows(c.cols, c.rows, bytes);
    app.windows[0].panes[0].pump();
    app.place_strips(0);
}

#[test]
fn helpers_are_listed_and_the_pane_resized_once_a_step() {
    let sb = Sandbox::new();
    let (mut app, mut s) = app(&sb);
    let c = app.windows[0].layout.cells[0];
    let mut resizes = |app: &CoreApp| s.resizes(&app.windows[0].panes[0]);
    assert_eq!(resizes(&app), []);
    let sid = app.windows[0].panes[0].shelld_session_id().unwrap();
    say(&mut app, sid, vec![row(3, true)]);
    assert_eq!((given(&app), resizes(&app)), (0, vec![]), "its session is not there yet");
    say(&mut app, sid, vec![gone(3, true)]);
    assert_eq!(resizes(&app), [], "one that was listed is not ready by going");
    say(&mut app, sid, vec![gone(3, true), gone(4, true)]);
    assert_eq!((given(&app), resizes(&app)), (3, vec![(c.cols, c.rows - 3)]));
    say(&mut app, sid, vec![gone(4, true), gone(5, true)]);
    assert_eq!(resizes(&app), [(c.cols, c.rows - 4)]);
    say(&mut app, sid, vec![gone(4, false), gone(5, false)]);
    app.rebuild_layout(0);
    assert_eq!(resizes(&app), [], "ended ones keep their rows");
    frame(&mut app, &mut s, b"$ ");
    let l = lines(&app, sid);
    assert_eq!(l[..2], ["", "  ⏺ main"]);
    assert!(l[2].starts_with("  ◯ codex 0004  look around") && l[2].ends_with("1m 5s · ↓ 12.0k tokens"), "{l:?}");
    assert!(l[3].starts_with("  ◯ codex 0005"));
    say(&mut app, sid, vec![]);
    assert_eq!((given(&app), s.resizes(&app.windows[0].panes[0])), (0, vec![(c.cols, c.rows)]));
}

#[test]
fn a_record_that_changed_is_drawn() {
    let sb = Sandbox::new();
    let (mut app, mut s) = app(&sb);
    say(&mut app, s.sid, vec![gone(4, true)]);
    frame(&mut app, &mut s, b"$ ");
    app.windows[0].needs_render = false;
    app.strips_tick();
    assert!(!app.windows[0].needs_render);
    say(&mut app, s.sid, vec![gone(4, false)]);
    assert!(app.windows[0].needs_render);
}

#[test]
fn switched_off_nothing_changes_and_switching_gives_and_takes_rows() {
    let sb = Sandbox::new();
    let (mut app, mut s) = app(&sb);
    turned_on(false);
    app.strips_tick();
    say(&mut app, s.sid, vec![gone(4, true)]);
    app.strips.typed(s.sid);
    assert_eq!((given(&app), s.resizes(&app.windows[0].panes[0])), (0, vec![]));
    assert!(app.strips.typed.is_empty() && app.strips_wait(Instant::now()).is_none());
    assert_eq!(app.helper_size(s.sid), None);
    turned_on(true);
    app.strips_tick();
    assert_eq!(given(&app), 3);
    turned_on(false);
    app.strips_tick();
    assert_eq!((given(&app), s.resizes(&app.windows[0].panes[0]).len()), (0, 2));
}

#[test]
fn typing_puts_the_change_off() {
    let sb = Sandbox::new();
    let (mut app, s) = app(&sb);
    app.strips.typed(s.sid);
    say(&mut app, s.sid, vec![gone(4, true)]);
    assert_eq!(given(&app), 0);
    assert!(app.idle_wait().is_some_and(|w| w <= Duration::from_secs(1)), "and the loop wakes for it");
    app.strips.typed.insert(s.sid, Instant::now() - Duration::from_millis(1_001));
    app.strips_tick();
    assert_eq!(given(&app), 3);
}

#[test]
fn a_session_left_short_by_an_earlier_core_is_not_resized() {
    let sb = Sandbox::new();
    let (probe, first) = app(&sb);
    let c = probe.windows[0].layout.cells[0];
    drop((probe, first));
    let start = |short: u16, helper: bool| {
        let (pane, s) = Session::of(c.cols, c.rows - short);
        let mut app = app_in(sb.dir(), vec![win(1, vec![pane])]);
        app.strips.on = true;
        if helper {
            let sid = reg::allocate_next_session_id().unwrap();
            app.shadows.push(Pane::new_vacant(sid, 80, 24));
            app.pane_control.shadows.family.adopted(sid, s.sid);
        }
        app.rebuild_layout(0);
        (app, s)
    };
    let (mut app, mut s) = start(4, true);
    assert_eq!((given(&app), s.resizes(&app.windows[0].panes[0])), (4, vec![]));
    frame(&mut app, &mut s, b"$ ");
    assert!(app.strips.view(Some(s.sid)).is_none(), "blank until the shell has spoken");
    say(&mut app, s.sid, vec![gone(4, true), gone(5, false)]);
    app.pane_control.strips.take(HelperStrip::all_told());
    app.strips_tick();
    assert_eq!(s.resizes(&app.windows[0].panes[0]), [], "what it said fits");
    frame(&mut app, &mut s, b"$ ");
    assert!(app.strips.view(Some(s.sid)).is_some());

    let (mut app, mut s) = start(4, true);
    app.pane_control.strips.take(HelperStrip::all_told());
    app.strips_tick();
    assert_eq!(given(&app), 4);
    std::thread::sleep(Duration::from_millis(3_010));
    app.strips_tick();
    assert_eq!((given(&app), s.resizes(&app.windows[0].panes[0])), (0, vec![(c.cols, c.rows)]), "it has none");

    for (short, helper, rows) in [(4, false, 0), (1, true, 1), (2, true, 2), (9, true, 0), (8, false, 8)] {
        assert_eq!(given(&start(short, helper).0), rows, "{short} short, helpers: {helper}");
    }
}

#[test]
fn a_shell_that_never_says_all_is_told_is_waited_for_five_seconds_from_the_cores_start() {
    let sb = Sandbox::new();
    let (probe, first) = app(&sb);
    let c = probe.windows[0].layout.cells[0];
    drop((probe, first));
    let (pane, s) = Session::of(c.cols, c.rows - 4);
    let mut app = app_in(sb.dir(), vec![win(1, vec![pane])]);
    app.strips.on = true;
    app.strips.born = Some(Instant::now() - Duration::from_secs(4));
    let helper = reg::allocate_next_session_id().unwrap();
    app.shadows.push(Pane::new_vacant(helper, 80, 24));
    app.pane_control.shadows.family.adopted(helper, s.sid);
    app.rebuild_layout(0);
    app.strips_tick();
    assert_eq!(given(&app), 4, "found");
    let wait = app.strips_wait(Instant::now()).unwrap();
    assert!(wait <= Duration::from_secs(1), "a second is left of the five: {wait:?}");
    std::thread::sleep(Duration::from_millis(1_010));
    app.strips_tick();
    let wait = app.strips_wait(Instant::now()).unwrap();
    assert!(wait <= Duration::from_secs(3) && wait > Duration::from_secs(2), "then as if it had none: {wait:?}");
}

#[test]
fn a_press_on_how_many_more_opens_the_panel_and_one_on_a_helper_gone_does_not() {
    let sb = Sandbox::new();
    let (mut app, mut s) = app(&sb);
    say(&mut app, s.sid, (4..10).map(|i| gone(i, true)).collect());
    assert_eq!(given(&app), 8);
    frame(&mut app, &mut s, b"$ ");
    assert_eq!(lines(&app, s.sid)[7], "  ↓ 1 more");
    for (row, opens) in [(1, false), (2, false), (3, false), (8, true)] {
        let (x, y) = at_row(&app, row);
        app.mouse_down(0, x, y, Modifiers::default());
        app.mouse_up(0, 1, x, y);
        assert_eq!(app.windows[0].panels.open() == Some(SurfaceKind::Helpers), opens, "row {row}");
        let change = app.windows[0].panels.close(CloseBy::Replaced);
        app.apply_surface_change(0, change);
    }
}

#[test]
fn rows_go_on_from_claudes_own_list() {
    let sb = Sandbox::new();
    let (mut app, mut s) = app(&sb);
    let mut record = HelperStrip { batch: 1, rows: vec![gone(4, true)], ..HelperStrip::empty(s.sid) };
    record.parent_agent = StripAgent::Claude;
    app.pane_control.strips.take(record);
    app.strips_tick();
    assert_eq!(given(&app), 4, "claude on the main screen: a head, its row and a blank row");
    let c = app.windows[0].layout.cells[0];
    let rule = "─".repeat(c.cols as usize);
    let own = format!("{rule}\r\n❯\r\n{rule}\r\n  foot\r\n\r\n  ⏺ main\r\n  ◯ general-purpose  job{}1s", " ".repeat(c.cols as usize - 28));
    frame(&mut app, &mut s, own.as_bytes());
    let v = app.strips.view(Some(s.sid)).unwrap();
    assert_eq!(v.top_row, 7, "right under it");
    assert!(lines(&app, s.sid)[0].starts_with("  ◯ codex            look around"), "{:?}", lines(&app, s.sid));
    frame(&mut app, &mut s, format!("{rule}\r\n❯\r\n{rule}\r\n  foot").as_bytes());
    assert_eq!(lines(&app, s.sid)[1], "  ⏺ main", "its list went: ours has a main");
}

#[test]
fn a_cleared_row_is_hidden_for_its_batch_only() {
    let mut h = host::Host::default();
    let record_of = |batch| HelperStrip { batch, rows: vec![gone(1, true), gone(2, true)], ..HelperStrip::empty(9) };
    let (one, two) = (record_of(1), record_of(2));
    let mut out = [NO_ROW; HELPER_STRIP_ROWS];
    h.heard(&one);
    h.hide(1);
    assert_eq!(h.rows(&one, 0, &mut out), 1);
    assert_eq!(out[0].name, "codex 0002");
    h.hide(2);
    assert_eq!(h.said(Some(&one), true, false), height::Said::Cleared);
    for sid in 10..60 {
        h.hide(sid);
    }
    h.heard(&one);
    let mut out = [NO_ROW; HELPER_STRIP_ROWS];
    assert_eq!(h.rows(&one, 0, &mut out), 0);
    h.heard(&two);
    let mut out = [NO_ROW; HELPER_STRIP_ROWS];
    assert_eq!(h.rows(&two, 0, &mut out), 2, "a new batch");
}

#[test]
fn a_shadow_has_its_panes_size_and_one_given_a_size_keeps_it() {
    let sb = Sandbox::new();
    let (mut app, s) = app(&sb);
    let c = app.windows[0].layout.cells[0];
    assert_eq!(app.helper_size(s.sid), Some((c.cols, c.rows - 3)), "with the row it will be listed in");
    let (a, mut sa) = Session::of(80, 24);
    let (b, mut sb2) = Session::of(80, 24);
    app.shadows.extend([a, b]);
    app.strips.sized(sb2.sid);
    say(&mut app, s.sid, vec![row(sa.sid, true), row(sb2.sid, true)]);
    let c = app.windows[0].layout.cells[0];
    assert_eq!(c.strip_rows, 4, "both are ready");
    let asked = |app: &CoreApp, sa: &mut Session, sb2: &mut Session| (sa.resizes(&app.shadows[0]), sb2.resizes(&app.shadows[1]));
    assert_eq!(asked(&app, &mut sa, &mut sb2), (vec![], vec![]), "not before the pane's size has settled");
    assert!(app.strips_wait(Instant::now()).is_some_and(|w| w <= Duration::from_millis(500)));
    std::thread::sleep(Duration::from_millis(510));
    app.strips_tick();
    assert_eq!(asked(&app, &mut sa, &mut sb2), (vec![(c.cols, c.rows)], vec![]));
    assert_eq!(app.strips_wait(Instant::now()), None, "and then nothing is owed");
}

#[test]
fn composing_a_record_again_allocates_nothing() {
    let sb = Sandbox::new();
    let (mut app, mut s) = app(&sb);
    say(&mut app, s.sid, vec![gone(4, true), gone(5, false)]);
    frame(&mut app, &mut s, b"$ ");
    frame(&mut app, &mut s, b"$ ls");
    assert_eq!(super::alloc_tests::counted(|| app.place_strips(0)), 0);
}

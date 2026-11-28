use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use marspot::control_deny::DenyList;
use marspot_peer::control::{ErrorCode, OpenArgs};
use marspot_term::session_registry::SessionEntry;

use super::caller::{CallerKey, Peer, by_ancestry};
use super::config::Config;
use super::conn::Conn;
use super::dispatch::Ctx;
use super::limits::{Kind, Limits, SpawnCounts, check_spawn};
use super::*;

mod audit_tests;
mod conn_tests;
mod list_tests;
mod new_window_tests;
pub(crate) mod panel_act_tests;
mod parent_tests;
mod proc_stop_tests;
mod server_tests;
mod shadow_race_tests;
mod view_typing_tests;

fn temp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pc-{tag}-{}-{}", std::process::id(), marspot_peer::rand::hex(4)));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A hub of its own, not the process-wide one.
fn hub_at(dir: &std::path::Path) -> (Hub, std::sync::mpsc::Receiver<MainCmd>) {
    let (main_tx, rx) = std::sync::mpsc::channel();
    let hub = Hub {
        epoch: Instant::now(),
        caps: AtomicU32::new(0),
        hello_ms: AtomicU64::new(0),
        focus: AtomicU64::new(0),
        last_key_ms: AtomicU64::new(0),
        typed: typed_in::TypedIn::default(),
        conns: AtomicUsize::new(0),
        links: links::Links::new(),
        limits: Mutex::new(Limits::default()),
        shadow_gate: shadow_gate::Gate::default(),
        recent: Mutex::new(recent::Recent::default()),
        config: RwLock::new(Arc::new(Config::default())),
        deny: Mutex::new(DenyList::at(dir.join("control-deny.tsv"))),
        panes: RwLock::new(Arc::new(panes::PaneViews::default())),
        views_stale: std::sync::atomic::AtomicBool::new(false),
        views_owed: (Mutex::new(false), std::sync::Condvar::new()),
        open_windows: RwLock::new(Arc::new(Vec::new())),
        audit: audit_log::Audit::at(dir.join("audit.log"), audit_log::ROTATE_BYTES),
        main_tx,
        wake: Arc::new(|| {}),
    };
    (hub, rx)
}

fn entry(id: u64, cwd: &str) -> SessionEntry {
    SessionEntry {
        id,
        pid: 0,
        socket: "/nonexistent".into(),
        cols: 80,
        rows: 24,
        title: String::new(),
        cwd: cwd.into(),
        proto_version: 0,
        created_at_unix: 0,
        shm_name: String::new(),
        shell_child_pid: 0,
        build: String::new(),
        stamps_regions: false,
        shadow_of: 0,
    }
}

fn ctx(caller: CallerKey, sessions: Vec<SessionEntry>, cfg: Config) -> Ctx {
    let obs = crate::observe::Observation::from_parts(
        Instant::now(),
        marspot::pidtree::ProcTable::from_rows(Vec::new()),
        sessions.into_iter().map(|e| (e, true)).collect(),
        |_| None,
    );
    Ctx { caller, caller_name: "t".into(), obs: Arc::new(obs), cfg: Arc::new(cfg), view: |c| c.obs.clone() }
}

#[test]
fn rate_buckets_just_within_and_one_over() {
    let cfg = Config::default();
    let mut l = Limits::default();
    let me = CallerKey::Pane(7);
    for _ in 0..cfg.rate_input {
        l.admit(&cfg, me, Kind::Input, 1_000).unwrap();
    }
    let r = l.admit(&cfg, me, Kind::Input, 1_000).unwrap_err();
    assert_eq!(r.code, ErrorCode::LimitRate);
    assert!(r.retry_after_ms.is_some_and(|ms| ms > 0 && ms <= 50), "{r:?}");
    // a twentieth of a second later one more fits
    l.admit(&cfg, me, Kind::Input, 1_050).unwrap();
    // reads have their own bucket
    for _ in 0..cfg.rate_read {
        l.admit(&cfg, me, Kind::Read, 1_050).unwrap();
    }
    assert_eq!(l.admit(&cfg, me, Kind::Read, 1_050).unwrap_err().code, ErrorCode::LimitRate);
}

#[test]
fn the_global_rate_holds_whoever_claims_to_ask() {
    let cfg = Config { rate_global: 5, ..Config::default() };
    let mut l = Limits::default();
    for i in 0..5 {
        l.admit(&cfg, CallerKey::Pane(i), Kind::Other, 0).unwrap();
    }
    assert_eq!(l.admit(&cfg, CallerKey::Pane(99), Kind::Other, 0).unwrap_err().code, ErrorCode::LimitRate);
}

#[test]
fn pane_bytes_and_spawn_rates() {
    let cfg = Config::default();
    let mut l = Limits::default();
    l.admit_bytes(&cfg, 3, 128 * 1024, 0).unwrap();
    assert_eq!(l.admit_bytes(&cfg, 3, 1, 0).unwrap_err().code, ErrorCode::LimitBytes);
    for _ in 0..cfg.spawn_per_minute {
        l.admit_spawn(&cfg, 9, 0).unwrap();
    }
    assert_eq!(l.admit_spawn(&cfg, 9, 0).unwrap_err().code, ErrorCode::LimitRate);
    // ten seconds later one more of the six a minute
    l.admit_spawn(&cfg, 9, 10_000).unwrap();
}

#[test]
fn externals_share_one_entry_and_closed_panes_leave() {
    let cfg = Config { rate_global: 1_000_000, ..Config::default() };
    let mut l = Limits::default();
    for t in 0..10_000u64 {
        let _ = l.admit(&cfg, CallerKey::External, Kind::Input, t);
    }
    assert_eq!(l.table_sizes(), 1);
    l.admit(&cfg, CallerKey::Pane(5), Kind::Input, 0).unwrap();
    l.admit_bytes(&cfg, 5, 10, 0).unwrap();
    l.opened(CallerKey::Pane(5), 6, false);
    assert_eq!(l.window_panes_of(CallerKey::Pane(5)), 1);
    l.retain(&|sid| sid != 5 && sid != 6);
    assert_eq!(l.table_sizes(), 1, "only the externals' bucket is left");
}

#[test]
fn counts_headroom_totals_and_nesting() {
    let cfg = Config::default();
    let ok = SpawnCounts { live_sessions: 55, shadows_total: cfg.shadows_total - 1, shadows_under_root: 7, parent_depth: 1 };
    check_spawn(&cfg, &ok, true, 64).unwrap();
    let over = |f: fn(&mut SpawnCounts)| {
        let mut c = SpawnCounts { ..ok };
        f(&mut c);
        check_spawn(&cfg, &c, true, 64).unwrap_err().code
    };
    assert_eq!(over(|c| c.live_sessions = 56), ErrorCode::LimitCount);
    assert_eq!(check_spawn(&cfg, &SpawnCounts { shadows_total: cfg.shadows_total, ..ok }, true, 64).unwrap_err().code, ErrorCode::LimitCount);
    assert_eq!(over(|c| c.shadows_under_root = 8), ErrorCode::LimitCount);
    // a third level of shadow panes
    assert_eq!(over(|c| c.parent_depth = 2), ErrorCode::LimitCount);
    // a lower session cap moves the user's headroom with it: 12 - 8 = 4
    check_spawn(&cfg, &SpawnCounts { live_sessions: 3, ..ok }, false, 12).unwrap();
    assert_eq!(check_spawn(&cfg, &SpawnCounts { live_sessions: 4, ..ok }, false, 12).unwrap_err().code, ErrorCode::LimitCount);
}

#[test]
fn session_cap_follows_the_test_switch() {
    // SAFETY: this test is the only reader of the variable in this process
    unsafe { std::env::set_var("MARSPOT_SESSION_CAP", "12") };
    assert_eq!(super::limits::session_cap(), 12);
    unsafe { std::env::remove_var("MARSPOT_SESSION_CAP") };
    assert_eq!(super::limits::session_cap(), 64);
}

#[test]
fn caller_found_by_ancestry_or_external() {
    // 900 (agent tool) → 800 (claude) → 700 (pane 42's shell) → 1
    let table: HashMap<i32, (i32, u64)> = [(900, (800, 9)), (800, (700, 8)), (700, (1, 7)), (600, (1, 6))].into();
    let parent_of = |p: i32| table.get(&p).copied();
    let shells: HashMap<i32, (u64, u64)> = [(700, (7, 42))].into();
    assert_eq!(by_ancestry(900, parent_of, &shells), Some(42));
    assert_eq!(by_ancestry(700, parent_of, &shells), Some(42));
    assert_eq!(by_ancestry(600, parent_of, &shells), None);
    // a reused pid with another start time is not the shell
    let reused: HashMap<i32, (u64, u64)> = [(700, (99, 42))].into();
    assert_eq!(by_ancestry(900, parent_of, &reused), None);
    let peer = Peer { pid: Some(900), exe: String::new(), pane: by_ancestry(900, parent_of, &shells) };
    assert_eq!(caller::identify(&peer, None, Vec::new), CallerKey::Pane(42));
    assert_eq!(caller::identify(&peer, Some("th"), || vec![(5, "th".into())]), CallerKey::Pane(5));
    assert_eq!(caller::identify(&Peer::default(), Some("x"), Vec::new), CallerKey::External);
}

#[test]
fn an_external_shadow_pane_needs_a_parent() {
    let dir = temp("ext");
    let (hub, _rx) = hub_at(&dir);
    let mut conn = Conn::new(&hub, Peer::default(), None);
    let c = ctx(CallerKey::External, Vec::new(), Config::default());
    let open = OpenArgs { cwd: None, shadow: true, new_window: false, parent: None, cols: 120, rows: 40, command: None };
    assert_eq!(ops_pane::open(&mut conn, &c, &open).unwrap_err().code, ErrorCode::BadRequest);
    let visible = OpenArgs { shadow: false, ..open };
    // past the parent check, the core without bit 1 is not asked
    assert_eq!(ops_pane::open(&mut conn, &c, &visible).unwrap_err().code, ErrorCode::CoreRestarting);
    let late = Hub { epoch: Instant::now() - std::time::Duration::from_secs(10), ..hub_at(&dir).0 };
    late.hello_ms.store(1, std::sync::atomic::Ordering::Release);
    let mut conn = Conn::new(&late, Peer::default(), None);
    assert_eq!(ops_pane::open(&mut conn, &c, &visible).unwrap_err().code, ErrorCode::OldSession);
}

#[test]
fn switched_off_and_denied_and_typing() {
    let dir = temp("off");
    let (hub, _rx) = hub_at(&dir);
    let mut conn = Conn::new(&hub, Peer::default(), None);
    let off = ctx(CallerKey::External, Vec::new(), Config { enabled: false, ..Config::default() });
    for op in [marspot_peer::control::Op::List, marspot_peer::control::Op::Close { target: "1".into() }] {
        assert_eq!(dispatch::run(&mut conn, &off, &op).unwrap_err().code, ErrorCode::Disabled);
    }
    assert!(!Config::parse("control_enabled = false\n").enabled);
    assert!(Config::parse("control_enabled = true\ncontrol_rate_input = 5\n").rate_input == 5);
    std::fs::write(dir.join("control-deny.tsv"), "# no\n/w/denied/\n").unwrap();
    let denied = ctx(CallerKey::Pane(3), vec![entry(3, "/w/denied")], Config::default());
    assert_eq!(dispatch::run(&mut conn, &denied, &marspot_peer::control::Op::List).unwrap_err().code, ErrorCode::Disabled);
    let fine = ctx(CallerKey::Pane(4), vec![entry(4, "/w/other")], Config::default());
    assert!(dispatch::run(&mut conn, &fine, &marspot_peer::control::Op::Audit { limit: 1 }).is_ok());
    // the user typed into the focused pane just now
    hub.focused(4);
    hub.key_pressed();
    assert_eq!(ops_input::gate_typing(&conn, 4, false).unwrap_err().code, ErrorCode::UserTyping);
    assert!(ops_input::gate_typing(&conn, 4, true).is_ok());
    assert!(ops_input::gate_typing(&conn, 5, false).is_ok());
    hub.last_key_ms.store(1, std::sync::atomic::Ordering::Relaxed);
    let later = Hub { epoch: Instant::now() - std::time::Duration::from_secs(5), ..hub_at(&dir).0 };
    later.focused(4);
    later.last_key_ms.store(1, std::sync::atomic::Ordering::Relaxed);
    assert!(ops_input::gate_typing(&Conn::new(&later, Peer::default(), None), 4, false).is_ok());
}

#[test]
fn watch_table_shares_and_reply_routing() {
    let links = links::Links::new();
    let (tx, rx) = std::sync::mpsc::channel();
    let body = marspot::shell_proto::WatchBody {
        watch_id: 0,
        kind: marspot::shell_proto::WatchKind::Change,
        since_pos: 0,
        quiet_ms: 0,
        text: String::new(),
    };
    let me = CallerKey::Pane(1);
    let ids: Vec<u32> = (0..8).map(|_| links.add_watch(9, me, body.clone(), tx.clone(), (8, 64)).unwrap()).collect();
    assert!(ids.iter().all(|id| id & (1 << 31) == 0 && *id != 0));
    assert_eq!(links.add_watch(9, me, body.clone(), tx.clone(), (8, 64)).unwrap_err().code, ErrorCode::LimitCount);
    assert_eq!(links.add_watch(9, CallerKey::External, body.clone(), tx.clone(), (8, 8)).unwrap_err().code, ErrorCode::LimitCount);
    let fired = marspot::shell_proto::PaneWatchFired {
        sid: 9,
        watch_id: ids[3],
        reason: marspot::shell_proto::WatchReason::Met,
        pos: 77,
        status: Some(0),
        line: None,
    };
    assert!(links.route(MsgType::PaneWatchFired, &marspot::shell_proto::encode_pane_watch_fired(&fired)));
    assert!(matches!(rx.try_recv(), Ok(Reply::Fired(f)) if f.pos == 77));
    // a report naming another session is not this watch's
    let other = marspot::shell_proto::PaneWatchFired { sid: 10, ..fired };
    links.route(MsgType::PaneWatchFired, &marspot::shell_proto::encode_pane_watch_fired(&other));
    assert!(rx.try_recv().is_err());
    for id in ids {
        links.drop_watch(id);
    }
    assert!(links.begin_input(4, 1));
    assert!(!links.begin_input(4, 1));
    links.end_input(4);
    assert_eq!(links.sizes(), (0, 0, 0));
    assert!(!links.route(MsgType::KeyEvent, &[]));
}

#[test]
fn run_puts_the_status_line_on_its_own_line() {
    assert_eq!(ops_input::run_text("make # done\n", false), "make # done\nprintf '\\033]133;D;%s;marspot=run\\007' $?");
    assert!(ops_input::run_text("true", true).ends_with(" $status"));
}

#[test]
fn pane_control_run_waits_for_the_end_its_own_line_prints() {
    let w = ops_input::run_end();
    assert_eq!((w.kind, w.since_pos), (marspot::shell_proto::WatchKind::CommandEnd, 0));
    for fish in [false, true] {
        assert!(!w.text.is_empty() && ops_input::run_text("true", fish).contains(&format!(";{}", w.text)));
    }
    let any = ops_read::watch_body(&marspot_peer::control::Wait::CommandEnd);
    assert_eq!(any.text, "", "a read takes any end");
}

#[test]
fn reads_are_cut_from_the_oldest_end() {
    let long = "old line\n".repeat(200_000);
    let (t, cut) = ops_read::cap_text(format!("{long}newest"));
    assert!(cut && t.len() <= ops_read::MAX_TEXT && t.ends_with("newest") && t.starts_with("old line"));
    assert_eq!(ops_read::cap_text("short".into()), ("short".into(), false));
}

#[test]
fn pane_control_matched_line_by_absolute_row_across_history_and_wraps() {
    let mut t = marspot_term::terminal::Terminal::new(10, 3);
    t.feed(b"0123456789abcdef\r\nx\r\ny\r\nz");
    let g = t.grid();
    assert_eq!(g.scroll_push_count(), 2);
    // rows 0 and 1 scrolled off; the line wraps from row 0 into row 1
    assert_eq!(matched::logical_line(g, 0).as_deref(), Some("0123456789abcdef"));
    assert_eq!(matched::logical_line(g, 3).as_deref(), Some("y"));
    assert_eq!(matched::logical_line(g, 9), None);
}

//! Shadow-affecting requests from several connections at once, against
//! a core the test plays that takes its time to answer, so the
//! requests overlap.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Barrier, Mutex};
use std::time::Duration;

use marspot::shell_proto::{
    CAP_SHADOWS, DoneCode, MsgType, PaneDone, PaneSpawned, decode_pane_place, decode_pane_spawn, encode_pane_done,
    encode_pane_spawned,
};
use marspot_peer::control::{ErrorCode, OpenArgs, Reject, Where};

use super::super::caller::{CallerKey, Peer};
use super::super::config::Config;
use super::super::conn::Conn;
use super::super::{Hub, MainCmd, ops_pane};
use super::{ctx, entry, hub_at, temp};

/// How long the core takes over each answer.
const SLOW: Duration = Duration::from_millis(150);

/// Run `body` while a slow core answers every frame queued.
fn with_slow_core<R>(hub: &Hub, rx: Receiver<MainCmd>, answer: impl Fn(&Hub, MsgType, &[u8]) + Send + Sync, body: impl FnOnce() -> R) -> R {
    hub.caps.store(CAP_SHADOWS, Ordering::Release);
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        let (stop, answer) = (&stop, &answer);
        s.spawn(move || {
            while !stop.load(Ordering::Acquire) {
                if let Ok(MainCmd::Send { msg, payload, .. }) = rx.recv_timeout(Duration::from_millis(20)) {
                    std::thread::sleep(SLOW);
                    answer(hub, msg, &payload);
                }
            }
        });
        let out = body();
        stop.store(true, Ordering::Release);
        out
    })
}

#[test]
fn pane_control_shadow_crossed_places_cannot_make_a_loop() {
    let (hub, rx) = hub_at(&temp("cross"));
    let window = || vec![entry(1, "/w"), entry(2, "/w"), entry(3, "/w")];
    // where the core put each pane it moved: sid → parent
    let placed: Mutex<HashMap<u64, u64>> = Mutex::new(HashMap::new());
    let answer = |hub: &Hub, msg: MsgType, p: &[u8]| {
        let Some(Ok(m)) = (msg == MsgType::PanePlace).then(|| decode_pane_place(p)) else { return };
        placed.lock().unwrap().insert(m.sid, m.host_sid);
        let done = PaneDone { req_id: m.req_id, code: DoneCode::Ok, text: String::new() };
        hub.links.route(MsgType::PaneDone, &encode_pane_done(&done).unwrap());
    };
    // pane 2 takes pane 1 under it while pane 1 takes pane 2 under it
    let asks = [(2u64, 1u64), (1, 2)];
    let ctxs: Vec<_> = asks.iter().map(|(caller, _)| ctx(CallerKey::Pane(*caller), window(), Config::default())).collect();
    let start = Barrier::new(asks.len());
    let outs: Vec<Result<(), Reject>> = with_slow_core(&hub, rx, answer, || {
        std::thread::scope(|s| {
            let runs: Vec<_> = asks
                .iter()
                .zip(&ctxs)
                .map(|((_, moved), c)| {
                    let (hub, start) = (&hub, &start);
                    s.spawn(move || {
                        let mut conn = Conn::new(hub, Peer::default(), None);
                        start.wait();
                        ops_pane::place_sid(&mut conn, c, *moved, Where::Shadow, false).map(|_| ())
                    })
                })
                .collect();
            runs.into_iter().map(|r| r.join().unwrap()).collect()
        })
    });
    assert_eq!(outs.iter().filter(|o| o.is_ok()).count(), 1, "{outs:?}");
    let refused = outs.iter().find_map(|o| o.as_ref().err()).unwrap();
    assert_eq!(refused.code, ErrorCode::BadRequest, "{refused:?}");
    let placed = placed.lock().unwrap();
    assert_eq!(placed.len(), 1, "the core was asked once: {placed:?}");
    for (sid, parent) in placed.iter() {
        assert_ne!(placed.get(parent), Some(sid), "{sid} and {parent} are each other's parent");
    }
}

#[test]
fn pane_control_shadow_concurrent_opens_hold_the_count_limit() {
    let (hub, rx) = hub_at(&temp("opens"));
    let cfg = Config { shadows_per_parent: 2, ..Config::default() };
    let next = AtomicU64::new(100);
    let spawned = AtomicU64::new(0);
    let answer = |hub: &Hub, msg: MsgType, p: &[u8]| {
        let Some(Ok(m)) = (msg == MsgType::PaneSpawn).then(|| decode_pane_spawn(p)) else { return };
        spawned.fetch_add(1, Ordering::AcqRel);
        let sid = next.fetch_add(1, Ordering::AcqRel);
        let s = PaneSpawned { req_id: m.req_id, ok: true, sid, err: String::new() };
        hub.links.route(MsgType::PaneSpawned, &encode_pane_spawned(&s).unwrap());
    };
    let n = 5;
    let ctxs: Vec<_> = (0..n).map(|_| ctx(CallerKey::Pane(10), vec![entry(10, "/w")], cfg.clone())).collect();
    let open = OpenArgs { cwd: None, shadow: true, parent: None, cols: 120, rows: 40, command: None };
    let start = Barrier::new(n);
    let outs: Vec<Result<(), Reject>> = with_slow_core(&hub, rx, answer, || {
        std::thread::scope(|s| {
            let runs: Vec<_> = ctxs
                .iter()
                .map(|c| {
                    let (hub, start, open) = (&hub, &start, &open);
                    s.spawn(move || {
                        let mut conn = Conn::new(hub, Peer::default(), None);
                        start.wait();
                        ops_pane::open(&mut conn, c, open).map(|_| ())
                    })
                })
                .collect();
            runs.into_iter().map(|r| r.join().unwrap()).collect()
        })
    });
    assert_eq!(outs.iter().filter(|o| o.is_ok()).count(), 2, "{outs:?}");
    assert!(outs.iter().filter_map(|o| o.as_ref().err()).all(|r| r.code == ErrorCode::LimitCount), "{outs:?}");
    assert_eq!(spawned.load(Ordering::Acquire), 2, "the core was asked for two panes");
}

#[test]
fn pane_control_shadow_open_with_no_answer_still_counts() {
    let (hub, rx) = hub_at(&temp("noanswer"));
    hub.caps.store(CAP_SHADOWS, Ordering::Release);
    let cfg = Config { shadows_per_parent: 1, ..Config::default() };
    let c = ctx(CallerKey::Pane(10), vec![entry(10, "/w")], cfg);
    let open = OpenArgs { cwd: None, shadow: true, parent: None, cols: 120, rows: 40, command: None };
    let mut conn = Conn::new(&hub, Peer::default(), None);
    // the core is sent the spawn but never answers
    let first = ops_pane::open_within(&mut conn, &c, &open, Duration::from_millis(200)).unwrap_err();
    assert_eq!(first.code, ErrorCode::Unknown, "{first:?}");
    assert!(matches!(rx.try_recv(), Ok(MainCmd::Send { msg: MsgType::PaneSpawn, .. })));
    // the view still shows no shadow, but the pane may exist
    let second = ops_pane::open_within(&mut conn, &c, &open, Duration::from_millis(200)).unwrap_err();
    assert_eq!(second.code, ErrorCode::LimitCount, "{second:?}");
    assert!(rx.try_recv().is_err(), "no second spawn was sent");
}

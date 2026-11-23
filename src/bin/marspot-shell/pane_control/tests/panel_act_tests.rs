//! The helper panel's buttons, against a core the test plays.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use marspot::shell_proto::{
    CAP_SHADOWS, CwdControl, DoneCode, HelperPanelAct, MsgType, PaneDone, PanelAct, RetireMode, decode_pane_retire, encode_pane_done,
};

use super::super::MainCmd;
use super::super::caller::{CallerKey, Peer};
use super::super::config::Config;
use super::super::conn::Conn;
use super::super::dispatch::Ctx;
use super::super::panel_act;
use super::{ctx, entry, hub_at, temp};
use crate::observe::Observation;

fn act(sid: u64, act: PanelAct) -> HelperPanelAct {
    HelperPanelAct { sid, act, want: CwdControl::Flip }
}

fn control(sid: u64, want: CwdControl) -> HelperPanelAct {
    HelperPanelAct { sid, act: PanelAct::ToggleCwdControl, want }
}

/// The panes each closed, in order, with how, and what `run` answered.
fn closes(what: HelperPanelAct) -> (Vec<(u64, RetireMode)>, (u32, u32)) {
    let (closed, counts, _) = closes_seeing(what, |c| c.obs.clone(), "");
    (closed, counts)
}

/// [`closes`] with the view taken before each close, and the audit log
/// once `says` is in it.
fn closes_seeing(what: HelperPanelAct, view: fn(&Ctx) -> Arc<Observation>, says: &str) -> (Vec<(u64, RetireMode)>, (u32, u32), String) {
    let dir = temp("panel");
    let (hub, rx) = hub_at(&dir);
    hub.caps.store(CAP_SHADOWS, Ordering::Release);
    // 2 and 3 are shadows of 1, 4 of 3; 9 of another pane.  Alive: this process.
    let mut entries = vec![entry(1, "/w"), entry(2, "/w"), entry(3, "/w"), entry(4, "/w"), entry(8, "/w"), entry(9, "/w")];
    for (id, parent) in [(2, 1), (3, 1), (4, 3), (9, 8)] {
        entries.iter_mut().find(|e| e.id == id).unwrap().shadow_of = parent;
    }
    let mut c = ctx(CallerKey::External, entries, Config::default());
    c.view = view;
    let (tx, stop) = std::sync::mpsc::channel::<()>();
    let out = std::thread::scope(|s| {
        let hub = &hub;
        let core = s.spawn(move || {
            let mut seen = Vec::new();
            loop {
                if let Ok(MainCmd::Send { msg, payload, .. }) = rx.recv_timeout(Duration::from_millis(20)) {
                    assert_eq!(msg, MsgType::PaneRetire);
                    let m = decode_pane_retire(&payload).unwrap();
                    seen.push((m.sid, m.mode));
                    let done = PaneDone { req_id: m.req_id, code: DoneCode::Ok, text: String::new() };
                    hub.links.route(MsgType::PaneDone, &encode_pane_done(&done).unwrap());
                } else if stop.try_recv().is_ok() {
                    return seen;
                }
            }
        });
        let mut conn = Conn::new(hub, Peer::default(), None);
        let counts = panel_act::run(&mut conn, &c, what, &dir.join("control-deny.tsv"));
        tx.send(()).unwrap();
        (core.join().unwrap(), counts)
    });
    let log = logged(&dir, says);
    std::fs::remove_dir_all(&dir).unwrap();
    (out.0, out.1, log)
}

/// The audit log once `says` is in it.
fn logged(dir: &std::path::Path, says: &str) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let log = std::fs::read_to_string(dir.join("audit.log")).unwrap_or_default();
        if log.contains(says) {
            return log;
        }
        assert!(std::time::Instant::now() < deadline, "no {says:?} in the audit log: {log}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// What `c` first saw, with the shadows in `to` moved.
fn moved(c: &Ctx, to: &[(u64, u64)]) -> Arc<Observation> {
    let mut entries: Vec<_> = c.obs.sessions.iter().map(|s| (s.entry.clone(), s.live)).collect();
    for (e, _) in &mut entries {
        if let Some((_, parent)) = to.iter().find(|t| t.0 == e.id) {
            e.shadow_of = *parent;
        }
    }
    let table = marspot::pidtree::ProcTable::from_rows(Vec::new());
    Arc::new(Observation::from_parts(std::time::Instant::now(), table, entries, |_| None))
}

/// Pane 2, a shadow pane, closed on the panel.
pub(crate) fn close_two() -> (Vec<(u64, RetireMode)>, (u32, u32)) {
    closes(act(2, PanelAct::CloseHelper))
}

#[test]
fn helper_panel_close_of_no_helper_is_a_pane_close() {
    let sandbox = marspot_term::test_env::sandbox_state_dir();
    live_entries(&sandbox, &[2]);
    let (closed, counts) = closes(act(2, PanelAct::CloseHelper));
    assert_eq!(closed, [(2, RetireMode::Recycle)], "kept in the recycle dir: the user did not open it");
    assert_eq!(counts, (1, 0));
}

#[test]
fn helper_panel_close_all_closes_the_tree_from_the_bottom() {
    let sandbox = marspot_term::test_env::sandbox_state_dir();
    live_entries(&sandbox, &[2, 3, 4]);
    let (closed, counts) = closes(act(1, PanelAct::CloseAllOfParent));
    assert_eq!(closed.iter().map(|c| c.0).collect::<Vec<_>>(), [4, 2, 3], "nothing of another pane's");
    assert_eq!(counts, (3, 0));
}

#[test]
fn helper_panel_a_pane_in_a_window_is_not_closed() {
    let sandbox = marspot_term::test_env::sandbox_state_dir();
    live_entries(&sandbox, &[1, 2]);
    let (closed, counts) = closes(act(1, PanelAct::CloseHelper));
    assert_eq!((closed.len(), counts), (0, (0, 1)));
    // a shadow when the panel asked, in a window by now
    let (closed, counts, log) = closes_seeing(act(2, PanelAct::CloseHelper), |c| moved(c, &[(2, 0)]), "refused: in a window");
    assert_eq!((closed.len(), counts), (0, (0, 1)));
    assert!(log.contains("pane.close"), "{log}");
}

#[test]
fn helper_panel_close_all_leaves_what_went_elsewhere_meanwhile() {
    let sandbox = marspot_term::test_env::sandbox_state_dir();
    live_entries(&sandbox, &[2, 3, 4]);
    // 2 to a window, 4 below another pane
    let see = |c: &Ctx| moved(c, &[(2, 0), (4, 8)]);
    let (closed, counts, _) = closes_seeing(act(1, PanelAct::CloseAllOfParent), see, "refused: no longer below");
    assert_eq!(closed.iter().map(|c| c.0).collect::<Vec<_>>(), [3]);
    assert_eq!(counts, (1, 2));
}

#[test]
fn helper_panel_a_gone_pane_is_counted_as_failed() {
    let _sandbox = marspot_term::test_env::sandbox_state_dir();
    let (closed, counts) = closes(act(2, PanelAct::CloseHelper));
    assert_eq!((closed.len(), counts), (0, (0, 1)));
}

#[test]
fn helper_panel_denies_and_allows_the_directory_and_writes_it_down() {
    let dir = temp("panel-deny");
    let (hub, _rx) = hub_at(&dir);
    let file = dir.join("control-deny.tsv");
    let c = ctx(CallerKey::External, vec![entry(1, "/w/spg")], Config::default());
    let mut conn = Conn::new(&hub, Peer::default(), None);
    let denied = || hub.deny.lock().unwrap().denies("/w/spg");
    let mut run = |want| panel_act::run(&mut conn, &c, control(1, want), &file);
    assert_eq!(run(CwdControl::Allow), (1, 0));
    assert!(!denied(), "it was allowed");
    for _ in 0..2 {
        assert_eq!(run(CwdControl::Deny), (1, 0));
        assert!(denied(), "asked twice, not turned back");
    }
    assert_eq!(run(CwdControl::Allow), (1, 0));
    assert!(!denied());
    // a core that does not say
    assert_eq!(run(CwdControl::Flip), (1, 0));
    assert!(denied());
    assert_eq!(run(CwdControl::Unknown(9)), (0, 1));
    // a line of a directory above it
    std::fs::write(&file, "/w\n").unwrap();
    assert_eq!(run(CwdControl::Allow), (0, 1));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "/w\n");
    // a pane nobody knows the directory of
    assert_eq!(panel_act::run(&mut conn, &c, control(77, CwdControl::Deny), &file), (0, 1));
    let log = logged(&dir, "refused: denied by the line /w");
    assert!(log.contains("control.deny") && log.contains("/w/spg"), "{log}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Registry entries of sessions whose process runs: this one.
fn live_entries(_sandbox: &marspot_term::test_env::StateSandbox, sids: &[u64]) {
    for sid in sids {
        let dir = marspot_term::session_registry::session_dir(*sid);
        std::fs::create_dir_all(&dir).unwrap();
        let mut e = entry(*sid, "/w");
        e.pid = std::process::id() as i32;
        marspot_term::session_registry::write_session_entry(&e).unwrap();
    }
}

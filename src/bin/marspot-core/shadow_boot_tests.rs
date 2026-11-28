//! Which panes a core starting up keeps in no window, against real
//! `marspot-session` processes.

use super::super::shadows::tests::{app_with_parent, drive, open_shadow, sandbox, wait_for};
use crate::pane_control::Route;
use marspot::shell_proto::decode_pane_spawned;
use super::super::spawn::{ShadowEnv, spawn_l3_shadow_with_cwd};
use super::*;
use marspot::shell_proto::SHADOW_IDLE_DEFAULT_S;
use marspot::state::{SavedPane, SavedWindowLayout};
use marspot_term::session_registry as reg;

fn sids(panes: &[Pane]) -> HashSet<u64> {
    panes.iter().filter_map(Pane::shelld_session_id).collect()
}

/// A parent, a top pane in no window, a child shown in a window whose
/// slot was lost and a child in no window; the core that ran them gone.
fn four(with_record: bool) -> (u64, u64, u64, u64, (Sender<CoreEvent>, marspot_term::mailbox::Receiver<CoreEvent>)) {
    let (tx, rx) = marspot_term::mailbox::channel();
    let id = || reg::allocate_next_session_id().unwrap();
    let (psid, top, shown, hidden) = (id(), id(), id(), id());
    let env = ShadowEnv { parent: psid, idle_s: SHADOW_IDLE_DEFAULT_S };
    let p = spawn_l3_pane_with_cwd(60, 16, psid, "", &tx).expect("real L3 spawn (is marspot-session built?)");
    let t = spawn_l3_pane_with_cwd(60, 16, top, "", &tx).expect("real L3 spawn");
    let s = spawn_l3_shadow_with_cwd(60, 16, shown, env, &tx).expect("shadow L3 spawn");
    let h = spawn_l3_shadow_with_cwd(60, 16, hidden, env, &tx).expect("shadow L3 spawn");
    std::mem::forget((p, t, s, h));
    if with_record {
        write_hidden_record(&[top, hidden]);
    }
    (psid, top, shown, hidden, (tx, rx))
}

fn boot(psid: u64, tx: &Sender<CoreEvent>) -> (HashSet<u64>, HashSet<u64>) {
    let saved = SavedWindowLayout {
        grid_cols: 1,
        grid_rows: 1,
        focused_idx: 0,
        panes: vec![SavedPane { sid: psid, flags: 0, custom_title: String::new(), last_cwd: String::new() }],
    };
    let (panes, _) = assemble_panes_at_boot(Some(&saved), 1, 60, 16, tx, Sweep::Full, &Default::default());
    let shown = sids(&panes);
    let shadows = adopt_shadows_at_boot(&shown, tx);
    (shown, sids(&shadows))
}

/// A top pane in no window stays in none; a child that was shown and
/// lost its slot is shown again, as any pane whose slot was lost.
#[test]
fn shadow_boot_a_top_pane_in_no_window_stays_hidden_and_a_shown_child_is_shown() {
    let _sb = sandbox();
    let (psid, top, shown, hidden, (tx, _rx)) = four(true);
    let (window, shadows) = boot(psid, &tx);
    assert_eq!(window, [psid, shown].into(), "the child that was shown is back in a window");
    assert_eq!(shadows, [top, hidden].into(), "the top pane in no window stays in none");
    assert_eq!(reg::read_session_entry(shown).unwrap().shadow_of, psid, "still its parent's child");
}

/// With no record, from a core before it, every child no saved window
/// holds goes to the shadows, and a top pane to the window.
#[test]
fn shadow_boot_without_a_record_children_go_to_the_shadows() {
    let _sb = sandbox();
    let (psid, top, shown, hidden, (tx, _rx)) = four(false);
    let (window, shadows) = boot(psid, &tx);
    assert_eq!(window, [psid, top].into());
    assert_eq!(shadows, [shown, hidden].into());
}

fn recorded() -> HashSet<u64> {
    hidden_record().expect("the core keeps a record")
}

/// A pane in no window is in the record while it lives and leaves it
/// when its shell ends.
#[test]
fn shadow_boot_record_loses_a_pane_when_it_ends() {
    let sb = sandbox();
    let (mut app, rx, psid) = app_with_parent(&sb);
    let sid = open_shadow(&mut app, &rx, 7, psid);
    let other = open_shadow(&mut app, &rx, 8, psid);
    assert_eq!(recorded(), [sid, other].into());
    app.inject_input(sid, b"q\r");
    wait_for("the ended pane left the record", || {
        drive(&mut app, &rx);
        app.pump_all();
        !recorded().contains(&sid)
    });
    assert_eq!(recorded(), [other].into(), "the live one stays");
}

/// The record lists only panes the core holds, and a spawn past the
/// session cap leaves none behind: at the cap it holds every pane in no
/// window and no more.
#[test]
fn shadow_boot_record_stays_within_the_session_cap() {
    const CAP: usize = 4;
    let mut sb = sandbox();
    sb.set("MARSPOT_SESSION_CAP", &CAP.to_string());
    let (mut app, rx, psid) = app_with_parent(&sb);
    let lines = || std::fs::read_to_string(record_path()).unwrap().lines().count();
    let hidden: HashSet<u64> = (0..CAP as u32 - 1).map(|r| open_shadow(&mut app, &rx, 10 + r, psid)).collect();
    assert_eq!(recorded(), hidden, "the parent and three panes in no window fill the cap");
    assert_eq!(lines(), CAP - 1);
    assert_eq!(app.dev_open_shadow(20, psid), Route::Forwarded, "the spawn is tried");
    let mut answer = None;
    wait_for("the spawn past the cap was answered", || {
        drive(&mut app, &rx);
        assert!(lines() <= CAP, "the record never lists more than the cap");
        answer = app
            .pane_control
            .reports
            .frames()
            .filter(|f| f.msg_type == MsgType::PaneSpawned)
            .map(|f| decode_pane_spawned(&f.payload).unwrap())
            .find(|s| s.req_id == 20);
        answer.is_some()
    });
    assert!(!answer.unwrap().ok, "refused at the cap");
    assert_eq!(recorded(), hidden, "the refused pane left no line");
    assert_eq!(lines(), CAP - 1);
}

//! A closed pane leaves no session directory behind, however much its
//! session was writing.  Against real `marspot-session` processes.

use super::super::spawn::spawn_l3_shadow_with_cwd;
use super::tests::{app_with_parent, drive, frame, reaped, sandbox, wait_for};
use super::*;
use crate::boot_assembly_tests::Sandbox;
use marspot::shell_proto::{PaneRetire, encode_pane_retire};
use marspot_term::session_registry as reg;

/// A shell that prints without end: as fast as it can, or a line at a
/// time, when its session has little to save and acts on a close within
/// the time its directory takes to empty.
fn printing(sb: &mut Sandbox, flood: bool) {
    let shell = sb.dir().join("printing");
    let line = "a line of output that goes to the byte log and to the history";
    let body = if flood {
        format!("exec /usr/bin/yes '{line}'")
    } else {
        format!("while :; do echo '{line}'; sleep 0.02; done")
    };
    std::fs::write(&shell, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&shell, <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755))
        .unwrap();
    sb.set("SHELL", &shell.to_string_lossy());
}

fn writes(sid: u64, least: u64) -> bool {
    let files = std::fs::read_dir(reg::session_dir(sid)).into_iter().flatten().flatten();
    files.filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum::<u64>() > least
}

fn retire(app: &mut CoreApp, req_id: u32, sid: u64, mode: RetireMode) {
    let f = frame(MsgType::PaneRetire, encode_pane_retire(&PaneRetire { req_id, sid, mode }));
    assert_eq!(app.route_pane_control(f), Route::Answered(DoneCode::Ok));
}

/// What is left of the session directories; `retired` too, which a
/// removal goes through.
fn left(sb: &Sandbox, dir: &str) -> Vec<String> {
    let names = std::fs::read_dir(sb.dir().join(dir)).into_iter().flatten().flatten();
    names.map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| !n.starts_with('.')).collect()
}

#[test]
fn close_of_fifty_panes_in_full_output_leaves_no_session_dir() {
    let mut sb = sandbox();
    let (mut app, rx, psid) = app_with_parent(&sb);
    let tx = app.event_tx.clone();
    for round in 0..5u32 {
        let flood = round % 2 == 0;
        printing(&mut sb, flood);
        let mut sids = Vec::new();
        let mut pids = Vec::new();
        for n in 0..10 {
            let sid = reg::allocate_next_session_id().unwrap();
            if n % 2 == 0 {
                app.windows[0].panes.push(spawn_l3_pane_with_cwd(60, 16, sid, "", &tx).expect("real L3 spawn"));
            } else {
                let env = ShadowEnv { parent: psid, idle_s: 0 };
                app.shadows.push(spawn_l3_shadow_with_cwd(60, 16, sid, env, &tx).expect("shadow L3 spawn"));
            }
            let pid = reg::read_session_entry(sid).unwrap().pid;
            sb.own(pid);
            sids.push(sid);
            pids.push(pid);
        }
        wait_for("every pane wrote to its dir", || {
            drive(&mut app, &rx);
            sids.iter().all(|&sid| writes(sid, if flood { 512 * 1024 } else { 1024 }))
        });
        for (n, &sid) in sids.iter().enumerate() {
            match app.find_window_pane_by_sid(sid) {
                Some((wi, idx)) => app.close_session(wi, idx),
                None => retire(&mut app, round * 10 + n as u32, sid, RetireMode::Delete),
            }
        }
        for pid in pids {
            wait_for("the session's process ended", || reaped(pid));
        }
        assert_eq!(left(&sb, "sessions"), [psid.to_string()], "only the parent's dir is there, round {round}");
        assert_eq!(left(&sb, "retired"), [""; 0], "and none was kept, round {round}");
    }
}

/// The session was not there to be told when its pane was closed.
#[test]
fn close_of_a_shadow_still_starting_leaves_no_session() {
    let sb = sandbox();
    let (mut app, rx, psid) = app_with_parent(&sb);
    assert_eq!(app.dev_open_shadow(7, psid), Route::Forwarded);
    let sid = app.shadows[0].shelld_session_id().unwrap();
    retire(&mut app, 8, sid, RetireMode::Delete);
    wait_for("its spawn landed", || match rx.try_recv() {
        Ok(CoreEvent::L3SpawnFinished(s, o)) => {
            sb.own(reg::read_session_entry(s).expect("the session started").pid);
            app.adopt_spawn_outcome(s, o);
            true
        }
        _ => false,
    });
    wait_for("the session's dir went", || left(&sb, "sessions") == [psid.to_string()]);
}

/// A window that is put away while a pane in it starts: nobody closed
/// the pane, and its session is there when the window comes back.
#[test]
fn close_of_a_window_while_a_pane_starts_keeps_its_session() {
    let sb = sandbox();
    let (mut app, rx, _psid) = app_with_parent(&sb);
    let tx = app.event_tx.clone();
    let history = |sid| reg::session_dir(sid).join("scrollback.bin");
    for (window, fails) in [(2, false), (3, true)] {
        let sid = reg::allocate_next_session_id().unwrap();
        std::fs::create_dir_all(reg::session_dir(sid)).unwrap();
        std::fs::write(history(sid), b"").unwrap();
        let pane = match fails {
            true => Pane::new_pending(sid, 60, 16),
            false => spawn_l3_pane_async(60, 16, sid, String::new(), &tx),
        };
        app.windows.push(crate::window_state_tests::win(window, vec![pane]));
        app.close_window(window);
        if fails {
            app.adopt_spawn_outcome(sid, SpawnOutcome(Err("no".into())));
        } else {
            wait_for("its spawn landed", || match rx.try_recv() {
                Ok(CoreEvent::L3SpawnFinished(s, o)) => {
                    sb.own(reg::read_session_entry(s).expect("the session started").pid);
                    app.adopt_spawn_outcome(s, o);
                    true
                }
                _ => false,
            });
            std::thread::sleep(Duration::from_millis(500));
            assert!(reg::entry_is_live(&reg::read_session_entry(sid).unwrap()), "it still runs");
        }
        assert!(history(sid).exists(), "fails: {fails}");
        assert!(!reg::is_closed(sid));
    }
}

/// The same for a window pane the user closed, and only once.
#[test]
fn close_of_a_window_pane_still_starting_leaves_no_session() {
    let sb = sandbox();
    let (mut app, rx, psid) = app_with_parent(&sb);
    let tx = app.event_tx.clone();
    let sid = reg::allocate_next_session_id().unwrap();
    app.windows[0].panes.push(spawn_l3_pane_async(60, 16, sid, String::new(), &tx));
    app.close_session(0, 1);
    wait_for("its spawn landed", || match rx.try_recv() {
        Ok(CoreEvent::L3SpawnFinished(s, o)) => {
            sb.own(reg::read_session_entry(s).expect("the session started").pid);
            app.adopt_spawn_outcome(s, o);
            true
        }
        _ => false,
    });
    wait_for("the session's dir went", || left(&sb, "sessions") == [psid.to_string()]);
    let closed = &mut app.pane_control.shadows.closed;
    assert!(!closed.take(sid));
    for s in 1..=100 {
        closed.note(s);
    }
    assert!(!closed.take(36) && closed.take(37), "the oldest are forgotten");
}

/// A pane recycled while it prints keeps its history, in the recycle
/// dir alone.
#[test]
fn close_into_the_recycle_dir_keeps_the_history() {
    let mut sb = sandbox();
    let (mut app, _rx, psid) = app_with_parent(&sb);
    printing(&mut sb, true);
    let sid = reg::allocate_next_session_id().unwrap();
    let env = ShadowEnv { parent: psid, idle_s: 0 };
    app.shadows.push(spawn_l3_shadow_with_cwd(60, 16, sid, env, &app.event_tx.clone()).expect("shadow L3 spawn"));
    let pid = reg::read_session_entry(sid).unwrap().pid;
    sb.own(pid);
    wait_for("the pane wrote to its dir", || writes(sid, 512 * 1024));
    retire(&mut app, 9, sid, RetireMode::Recycle);
    wait_for("the session's process ended", || reaped(pid));
    assert_eq!(left(&sb, "sessions"), [psid.to_string()]);
    let kept = left(&sb, "retired");
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert!(kept[0].starts_with(&format!("{sid}-")), "{kept:?}");
    assert!(sb.dir().join("retired").join(&kept[0]).join("entry.toml").exists());
}

/// A slot with no L3 as far as the core knows, whose session runs after
/// all: closing the slot closes the session.
#[test]
fn close_of_an_empty_slot_closes_the_l3_that_holds_its_dir() {
    let sb = sandbox();
    let (mut app, _rx, psid) = app_with_parent(&sb);
    let sid = reg::allocate_next_session_id().unwrap();
    let _running = spawn_l3_pane_with_cwd(60, 16, sid, "", &app.event_tx.clone()).expect("real L3 spawn");
    sb.own(reg::read_session_entry(sid).unwrap().pid);
    app.windows[0].panes.push(Pane::new_vacant(sid, 60, 16));
    app.close_session(0, 1);
    // closed, not recycled: it goes through the recycle dir and leaves nothing there
    wait_for("the session's dir went", || {
        left(&sb, "sessions") == [psid.to_string()] && left(&sb, "retired").is_empty()
    });
}

/// The core reaps the L3s it started once their panes are closed.
#[test]
fn close_leaves_no_zombie() {
    let sb = sandbox();
    let (mut app, _rx, psid) = app_with_parent(&sb);
    let tx = app.event_tx.clone();
    let (pane, shadow) = (reg::allocate_next_session_id().unwrap(), reg::allocate_next_session_id().unwrap());
    app.windows[0].panes.push(spawn_l3_pane_with_cwd(60, 16, pane, "", &tx).expect("real L3 spawn"));
    let env = ShadowEnv { parent: psid, idle_s: 0 };
    app.shadows.push(spawn_l3_shadow_with_cwd(60, 16, shadow, env, &tx).expect("shadow L3 spawn"));
    let pids = [pane, shadow].map(|sid| reg::read_session_entry(sid).unwrap().pid);
    for pid in pids {
        sb.own(pid);
    }
    app.close_session(0, 1);
    retire(&mut app, 1, shadow, RetireMode::Delete);
    for pid in pids {
        wait_for("the session's process was reaped", || reaped(pid));
    }
}

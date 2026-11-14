//! A pane whose shell ends closes and leaves nothing to resurrect; a
//! pane whose L3 is killed stays for a new core to resurrect.  Against
//! real `marspot-session` processes.

use super::boot_assembly_tests::Sandbox;
use super::window_state_tests::{app_in, win};
use super::*;
use marspot_term::session_registry as reg;
use std::time::Duration;

/// A shell for the panes: a script that runs `body`.
fn shell(sb: &Sandbox, name: &str, body: &str) -> String {
    let path = sb.dir().join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755))
        .unwrap();
    path.to_string_lossy().into_owned()
}

/// A pane whose L3 runs `shell`.
fn pane(sb: &mut Sandbox, shell: &str) -> (u64, Pane) {
    sb.set("SHELL", shell);
    let (tx, _rx) = mpsc::channel();
    let sid = reg::allocate_next_session_id().unwrap();
    let p = spawn_l3_pane_with_cwd(60, 16, sid, "", &tx).expect("real L3 spawn (is marspot-session built?)");
    (sid, p)
}

/// Pump until `done`.  The limit is for a loaded machine; the shells
/// end within half a second.
fn pump_until(app: &mut CoreApp, what: &str, mut done: impl FnMut(&CoreApp) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done(app) {
        assert!(Instant::now() < deadline, "never happened: {what}");
        app.pump_all();
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn close_requests(app: &CoreApp) -> usize {
    app.pending_to_shell.iter().filter(|(t, _)| *t == MsgType::WindowCloseRequest).count()
}

fn sids(app: &CoreApp, wi: usize) -> Vec<Option<u64>> {
    app.windows[wi].panes.iter().map(|p| p.shelld_session_id()).collect()
}

#[test]
fn a_pane_whose_shell_ends_closes_and_one_whose_session_is_killed_stays() {
    let mut sb = Sandbox::new();
    let ends = shell(&sb, "ends", "sleep 0.5; exit 0");
    let lives = shell(&sb, "lives", "exec sleep 60");
    let (gone, a) = pane(&mut sb, &ends);
    let (kept, b) = pane(&mut sb, &lives);
    let b_pid = b.session().l3_pid().unwrap();
    let mut app = app_in(sb.dir(), vec![win(1, vec![a, b])]);

    pump_until(&mut app, "the pane whose shell ended closed", |app| app.windows[0].panes.len() == 1);
    assert_eq!(sids(&app, 0), vec![Some(kept)], "the other pane is the one left");
    assert!(!reg::session_dir(gone).exists(), "the ended session's dir must go: a later core would resurrect it");
    assert!(reg::session_dir(kept).exists());
    assert_eq!(close_requests(&app), 0, "a window with a live pane stays open");
    assert!(!app.all_exited);
    let saved = marspot::state::read().expect("the close saved the layout");
    let saved: Vec<u64> = saved.windows[0].panes.iter().map(|p| p.sid).collect();
    assert_eq!(saved, vec![kept], "the closed pane is not in the layout a restarted core restores");

    // A killed L3 is a crash: its pane and its dir stay, and with no
    // other session left the core exits for a new one to resurrect it.
    // SAFETY: the L3 this test spawned, not yet reaped.
    unsafe { libc::kill(b_pid, libc::SIGKILL) };
    pump_until(&mut app, "the killed session was seen to exit", |app| app.windows[0].panes[0].is_exited());
    app.pump_all();
    assert_eq!(sids(&app, 0), vec![Some(kept)], "a crashed session's pane stays");
    assert!(reg::session_dir(kept).exists(), "a crashed session's dir is kept for resurrection");
    assert!(app.all_exited, "every session lost: the core exits so a new one resurrects them");
    assert_eq!(close_requests(&app), 0);
}

#[test]
fn when_every_shell_has_ended_the_app_is_asked_to_quit_not_restarted() {
    let mut sb = Sandbox::new();
    let ends = shell(&sb, "ends", "sleep 0.5; exit 0");
    let (s1, a) = pane(&mut sb, &ends);
    let (s2, b) = pane(&mut sb, &ends);
    let (s3, c) = pane(&mut sb, &ends);
    let mut app = app_in(sb.dir(), vec![win(1, vec![a, b]), win(2, vec![c])]);

    pump_until(&mut app, "both windows emptied", |app| {
        app.windows.iter().all(|w| w.panes.iter().all(|p| p.is_dormant()))
    });
    assert_eq!(close_requests(&app), 2, "each emptied window is closed through L1");
    assert!(app.layout_discarded, "the last window emptied: the layout goes, as when the user closes it");
    assert!(marspot::state::read().is_none(), "nothing saved for the next launch to restore");
    for sid in [s1, s2, s3] {
        assert!(!reg::session_dir(sid).exists(), "session {sid}'s dir must go");
    }
    app.pump_all();
    assert!(!app.all_exited, "the core must not exit on its own: L1 would take that for a crash and restart it");
}

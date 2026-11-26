//! A pane in `shadows` is reached by everything keyed by session id,
//! against a real `marspot-session` process.

use crate::boot_assembly_tests::Sandbox;
use crate::window_state_tests::{app_in, win};
use crate::*;
use marspot_term::session_registry as reg;
use std::time::Duration;

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "never happened: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn panes_mut_reaches_a_shadow_pane() {
    let mut sb = Sandbox::new();
    let out = sb.dir().join("out");
    let shell = sb.dir().join("shell");
    // one line per forwarding path, then a third line ends the shell
    std::fs::write(
        &shell,
        format!(
            "#!/bin/sh\nIFS= read -r a; printf '%s' \"$a\" > {o}.inject\nIFS= read -r b; printf '%s' \"$b\" > {o}.paste\nIFS= read -r c\nexit 0\n",
            o = out.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        &shell,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .unwrap();
    sb.set("SHELL", &shell.to_string_lossy());
    let (tx, _rx) = marspot_term::mailbox::channel();
    let sid = reg::allocate_next_session_id().unwrap();
    let shadow = spawn_l3_pane_with_cwd(60, 16, sid, "", &tx)
        .expect("real L3 spawn (is marspot-session built?)");
    let drawn = Pane::new_vacant(reg::allocate_next_session_id().unwrap(), 60, 16);
    let mut app = app_in(sb.dir(), vec![win(1, vec![drawn])]);
    app.shadows.push(shadow);

    assert!(app.pane_by_sid_mut(sid).is_some(), "found by session id");
    assert!(
        app.find_window_pane_by_sid(sid).is_none(),
        "a shadow is in no window"
    );

    // stage the build the L3 already runs and make its entry claim an
    // older one: L2 asks it to move, and the L3 itself sees nothing to do
    let mut entry = reg::read_session_entry(sid).unwrap();
    assert!(
        !entry.build.is_empty(),
        "the L3 under test records its build"
    );
    let staged = marspot_term::binary_tree::BinaryTree::default_for("marspot-session")
        .unwrap()
        .current();
    std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
    std::fs::write(&staged, format!("{}|END", entry.build)).unwrap();
    entry.build = "MARSPOT_FP=old|0".into();
    reg::write_session_entry(&entry).unwrap();
    assert_eq!(
        app.ask_every_l3_to_update(),
        1,
        "the update fan-out asks the shadow"
    );

    let read = |ext: &str| std::fs::read_to_string(out.with_extension(ext)).ok();
    app.inject_input(sid, b"injected\r");
    wait_for("injected bytes reached the shadow's shell", || {
        read("inject").as_deref() == Some("injected")
    });
    app.forward_pane_paste(sid, "pasted\r");
    wait_for("the paste reached the shadow's shell", || {
        read("paste").as_deref() == Some("pasted")
    });

    app.inject_input(sid, b"\r");
    wait_for("the shadow's shell ended and its pane closed", || {
        if let Some(p) = app.shadows.first_mut() {
            p.pump();
        }
        app.close_ended_panes();
        app.shadows.is_empty()
    });
    assert!(
        !reg::session_dir(sid).exists(),
        "the ended session's dir is gone"
    );
    assert_eq!(app.windows[0].panes.len(), 1, "the drawn pane is untouched");
}

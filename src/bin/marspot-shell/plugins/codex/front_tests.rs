//! Which codex is a pane's, on real shells and real processes.
//!
//! The 2026-11-17 incident: claude, in front of its pane, ran `codex`
//! as one of its tools.  The plugin took the pane for a codex pane,
//! badged it, and a pick on claude's badge menu typed a codex resume
//! line into claude.

use std::sync::Arc;
use std::time::Instant;

use crate::observe::{Observation, Session};
use crate::plugins::pty_op::front::tests::{shell_on_a_pty, wait_for};
use marspot::pidtree::{self, ProcTable};

/// Pane 1 as the observer would see it now, its shell `shell`.
fn look(shell: i32) -> Observation {
    Observation::from_parts(
        Instant::now(),
        ProcTable::take(),
        vec![(crate::observe::tests::entry(1, 1, shell), true)],
        |r| pidtree::proc_argv(r.pid).map(Arc::<[String]>::from),
    )
}

fn pane(obs: &Observation) -> &Session {
    obs.session(1).expect("the pane is in the observation")
}

#[test]
fn a_codex_started_by_the_program_in_front_is_not_the_pane_s() {
    let (mut sh, mut m) = shell_on_a_pty();
    let shell = sh.id() as i32;
    // some program in front, with a codex of its own under it
    std::io::Write::write_all(&mut m, b"/bin/bash -c '(exec -a codex /bin/sleep 30) & wait'\n").unwrap();
    let ready = wait_for(|| {
        let obs = look(shell);
        let s = pane(&obs);
        s.front().is_some_and(|p| p.row.comm == "bash") && s.find("codex").is_some()
    });
    let obs = look(shell);
    let bound = super::pane_codex(pane(&obs)).map(|p| p.row.pid);
    let _ = sh.kill();
    let _ = sh.wait();
    assert!(ready, "bash never took the terminal with a codex under it");
    assert_eq!(bound, None, "the codex belongs to the program in front, not to the pane");
}

#[test]
fn the_codex_started_at_the_prompt_is_the_pane_s() {
    let (mut sh, mut m) = shell_on_a_pty();
    let shell = sh.id() as i32;
    std::io::Write::write_all(&mut m, b"(exec -a codex /bin/sleep 30)\n").unwrap();
    let ready = wait_for(|| super::pane_codex(pane(&look(shell))).is_some());
    let obs = look(shell);
    let front = pane(&obs).front().map(|p| p.row.pid);
    let bound = super::pane_codex(pane(&obs)).map(|p| p.row.pid);
    let _ = sh.kill();
    let _ = sh.wait();
    assert!(ready, "the codex never took the terminal");
    assert_eq!(bound, front);
}

#[test]
fn a_codex_put_aside_is_not_the_pane_s() {
    let (mut sh, mut m) = shell_on_a_pty();
    let shell = sh.id() as i32;
    // in the background: running in the pane, not what the pane is talking to
    std::io::Write::write_all(&mut m, b"(exec -a codex /bin/sleep 30) &\n").unwrap();
    let ready = wait_for(|| {
        let obs = look(shell);
        let s = pane(&obs);
        s.find("codex").is_some() && s.front().is_none()
    });
    let obs = look(shell);
    let bound = super::pane_codex(pane(&obs)).map(|p| p.row.pid);
    let leftover = pane(&obs).find("codex").map(|p| p.row.pid);
    let _ = sh.kill();
    let _ = sh.wait();
    if let Some(pid) = leftover {
        let _ = pidtree::kill_pid(pid, libc::SIGKILL);
    }
    assert!(ready, "the background codex never showed up with the shell in front");
    assert_eq!(bound, None);
}

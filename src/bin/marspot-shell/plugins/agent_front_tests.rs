//! Each agent plugin binds a pane only to the agent in front of it.
//!
//! One vendor's CLI in front of the pane starts a child named like
//! another vendor's CLI, as a tool.  The child's plugin must not take
//! the pane; the plugin of the program in front does.

use std::sync::Arc;
use std::time::Instant;

use crate::observe::{Observation, Proc, Session};
use crate::plugins::pty_op::front::tests::{shell_on_a_pty, wait_for};
use marspot::pidtree::{self, ProcTable};

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

/// Runs `line` at a real shell's prompt and waits until `front_is`
/// binds the program in front and `child` runs under it.  Returns
/// what `claims` binds then, as a pid.
fn claim_with_a_child_in_front(
    line: &str,
    front_is: fn(&Session) -> Option<i32>,
    child: fn(&Proc) -> bool,
    claims: fn(&Session) -> Option<i32>,
) -> Option<i32> {
    let (mut sh, mut m) = shell_on_a_pty();
    let shell = sh.id() as i32;
    std::io::Write::write_all(&mut m, format!("{line}\n").as_bytes()).unwrap();
    let ready = wait_for(|| {
        let obs = look(shell);
        let s = pane(&obs);
        front_is(s).is_some_and(|front| s.descendants.iter().any(|p| p.row.ppid == front && child(p)))
    });
    let obs = look(shell);
    let s = pane(&obs);
    let claimed = claims(s);
    let started: Vec<i32> = s.descendants.iter().map(|p| p.row.pid).collect();
    let _ = sh.kill();
    let _ = sh.wait();
    for pid in started {
        let _ = pidtree::kill_pid(pid, libc::SIGKILL);
    }
    assert!(ready, "the program in front never showed up with its child: {line}");
    claimed
}

fn claude(s: &Session) -> Option<i32> {
    super::claudecode::pane_claude(s).map(|p| p.row.pid)
}

fn codex(s: &Session) -> Option<i32> {
    super::codex::pane_codex(s).map(|p| p.row.pid)
}

fn gemini(s: &Session) -> Option<i32> {
    super::gemini::agent_in(s).map(|(p, _)| p.row.pid)
}

fn kimi(s: &Session) -> Option<i32> {
    super::kimi::agent_in(s).map(|p| p.row.pid)
}

#[test]
fn codex_started_by_claude_in_front_is_not_claimed_by_codex() {
    let claimed = claim_with_a_child_in_front(
        "(exec -a claude /bin/bash -c '(exec -a codex /bin/sleep 30) & wait')",
        claude,
        |p| p.name() == "codex",
        codex,
    );
    assert_eq!(claimed, None);
}

#[test]
fn claude_started_by_codex_in_front_is_not_claimed_by_claudecode() {
    let claimed = claim_with_a_child_in_front(
        "(exec -a codex /bin/bash -c '(exec -a claude /bin/sleep 30) & wait')",
        codex,
        |p| p.name() == "claude",
        claude,
    );
    assert_eq!(claimed, None);
}

#[test]
fn gemini_started_by_claude_in_front_is_not_claimed_by_gemini() {
    let claimed = claim_with_a_child_in_front(
        "(exec -a claude /bin/bash -c '(exec -a gemini /bin/sleep 30) & wait')",
        claude,
        super::gemini::is_gemini,
        gemini,
    );
    assert_eq!(claimed, None);
}

#[test]
fn kimi_started_by_gemini_in_front_is_not_claimed_by_kimi() {
    // kimi is told by the kernel's name for the process, the file name
    // of its executable, so the child runs a copy of sleep named kimi;
    // a copy keeps the system's signature and is killed on exec unless
    // it is signed again
    let dir = std::env::temp_dir().join(format!("marspot-agent-front-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("kimi");
    std::fs::copy("/bin/sleep", &bin).unwrap();
    let signed = std::process::Command::new("/usr/bin/codesign").arg("--force").args(["-s", "-"]).arg(&bin).output().unwrap();
    assert!(signed.status.success(), "{}", String::from_utf8_lossy(&signed.stderr));
    let claimed = claim_with_a_child_in_front(
        &format!("(exec -a gemini /bin/bash -c '{} 30 & wait')", bin.display()),
        gemini,
        |p| p.row.comm == "kimi",
        kimi,
    );
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(claimed, None);
}

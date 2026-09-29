//! A pane's shell gets the user's environment, never the launcher's
//! session identity.
//!
//! `open(1)` forwards the caller's environment (verified 2026-09-07
//! with a canary), so a terminal launched from inside an agent pane
//! inherits that agent's session vars — and would hand them to every
//! pane it opens.  Agents started in those panes then believe they
//! are children of a session that was never theirs: four of them
//! turned transcript saving off and exited without a word.
use marspot_term::pty::{Pty, PtyConfig, SESSION_ENV_PREFIXES, TerminalSize};
use std::time::{Duration, Instant};

/// Run `env` in a pty configured the way a pane's shell is, and
/// return everything it printed.
fn env_seen_by_a_pane(strip: Vec<String>) -> String {
    let mut pty = Pty::spawn(PtyConfig {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "env; exit".into()],
        size: TerminalSize { cols: 200, rows: 40, ..Default::default() },
        env_remove_prefixes: strip,
        ..Default::default()
    })
    .expect("spawn");
    unsafe {
        let fd = pty.raw_master();
        let fl = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
    let mut out = String::new();
    let mut buf = [0u8; 8192];
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(5) {
        match pty.read(&mut buf) {
            Ok(n) if n > 0 => out.push_str(&String::from_utf8_lossy(&buf[..n])),
            _ => {
                if out.contains("CANARY_TAIL") {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    out
}

#[test]
fn a_pane_does_not_inherit_the_launchers_session_identity() {
    // Exactly what was found on the real L1: this session's id, its
    // child-session flag, and the socket its agents talk over.
    unsafe {
        std::env::set_var("CLAUDE_CODE_CHILD_SESSION", "1");
        std::env::set_var("CLAUDE_CODE_SESSION_ID", "not-yours");
        std::env::set_var("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/cc-socks/000.sock");
        std::env::set_var("CLAUDECODE", "1");
        std::env::set_var("MARSPOT_SESSION_ID", "999");
        std::env::set_var("CODEX_HOME", "/nope");
        // An ordinary variable, to prove the shell still gets one.
        std::env::set_var("CANARY_TAIL", "kept");
    }

    let stripped = env_seen_by_a_pane(
        SESSION_ENV_PREFIXES.iter().map(|s| (*s).to_string()).collect(),
    );
    for name in [
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDECODE",
        "MARSPOT_SESSION_ID",
        "CODEX_HOME",
    ] {
        assert!(
            !stripped.contains(name),
            "{name} reached the pane's shell:\n{stripped}"
        );
    }
    assert!(
        stripped.contains("CANARY_TAIL=kept"),
        "the user's own environment must still arrive:\n{stripped}"
    );
}

#[test]
fn without_the_strip_list_they_all_come_through() {
    // The other half of the claim: these ARE inherited by default, so
    // the list above is doing the work rather than describing a thing
    // that never happens.
    unsafe {
        std::env::set_var("CLAUDE_CODE_CHILD_SESSION", "1");
        std::env::set_var("CANARY_TAIL", "kept");
    }
    let plain = env_seen_by_a_pane(Vec::new());
    assert!(
        plain.contains("CLAUDE_CODE_CHILD_SESSION"),
        "inheritance is the default:\n{plain}"
    );
}

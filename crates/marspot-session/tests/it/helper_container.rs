//! A container helper's script in a real session: for
//! bin/test-helper-container.sh, which has the container runtime and
//! writes the script.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::pane_link::{bin, wait};
use crate::self_execv::Sandbox;
use crate::support::{COLS, ROWS, Session};

fn var(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"))
}

/// Whether container `name` runs.
fn listed(docker: &str, name: &str) -> bool {
    // it may never answer, and takes no notice of an alarm
    let limit = r#"my $t = shift; my $p = fork; die unless defined $p;
        if (!$p) { setpgrp; exec @ARGV; exit 127 }
        $SIG{ALRM} = sub { kill "KILL", -$p; exit 124 };
        alarm $t; waitpid $p, 0; exit($? & 127 ? 128 + ($? & 127) : $? >> 8)"#;
    let out = Command::new("/usr/bin/perl").args(["-e", limit, "20", docker]).args(["ps", "-q", "--filter", &format!("name=^/{name}$")]).output().unwrap();
    assert!(out.status.success(), "docker ps: {}", String::from_utf8_lossy(&out.stderr));
    !out.stdout.is_empty()
}

/// Run the script as the session's shell, wait for the result file, then
/// end the session while the agent in the container prints nothing.
#[test]
#[ignore = "run by bin/test-helper-container.sh"]
fn helper_container_goes_with_its_session() {
    use std::os::unix::fs::PermissionsExt;
    let (docker, dir) = (var("CONTAINER_DOCKER"), PathBuf::from(var("CONTAINER_DIR")));
    let name = std::fs::read_to_string(dir.join("name")).unwrap();
    let mut sandbox = Sandbox::new();
    let shell = sandbox.path().join("shell.sh");
    // the script is a child of the pane's shell, with a process group
    // of its own, as in a helper's pane
    let body = format!("#!/bin/sh\nset -m\nsh '{}'\nexec sleep 600\n", dir.join("run.sh").display());
    std::fs::write(&shell, body).unwrap();
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    sandbox.env.set("SHELL", &shell);
    let sid = crate::support::id_base() + 4900;
    let (mut s, regions) = Session::spawn_named(bin(), COLS, ROWS, sid);
    std::fs::write(dir.join("session.pid"), s.pid().to_string()).unwrap();
    let result = dir.join("share/result.md");
    let holds = |what: &str| std::fs::read_to_string(&result).is_ok_and(|t| t.contains(what));
    wait(60.0, "the agent", || holds("terminal\n"));
    s.send_line("hello");
    wait(30.0, "the line it was typed", || holds("END\n"));
    assert!(listed(&docker, &name), "the container runs while its session does");
    std::thread::sleep(Duration::from_secs(1));

    if var("CONTAINER_END") == "kill" {
        s.child.kill().unwrap();
    } else {
        // SAFETY: a process this test started
        unsafe { libc::kill(s.pid() as i32, libc::SIGHUP) };
    }
    let _ = s.child.wait();
    std::fs::remove_file(dir.join("session.pid")).unwrap();
    let ended = Instant::now();
    while listed(&docker, &name) && ended.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let took = ended.elapsed();
    println!("gone {:.2} s after the session ended", took.as_secs_f64());
    assert!(took < Duration::from_secs(2), "the container was there {took:?} after its session");
    regions.delete();
}

use std::path::PathBuf;

use super::*;
use crate::plugins::handoff::history::tmp;
use crate::plugins::peerbus::fake_daemon::Daemon;

const SOCKET: &str = "app-server-control/app-server-control.sock";
const ROLLOUT: &str = "rollout-2026-11-28T10-00-00-0a1b2c3d-1111-2222-3333-444455556666.jsonl";

/// A codex home inside `name`'s temp dir whose daemon socket path is
/// `len` bytes: past the 104 a `sockaddr_un` declares, as a sandbox
/// state dir or a long `$HOME` gives.
fn home_with_socket_of(name: &str, len: usize) -> PathBuf {
    let base = tmp(name);
    let pad = len - base.as_os_str().len() - 1 - "/".len() - SOCKET.len();
    let home = base.join(format!("h{}", "o".repeat(pad - 1)));
    std::fs::create_dir_all(&home).unwrap();
    assert_eq!(home.join(SOCKET).as_os_str().len(), len);
    home
}

fn leaving_codex(home: PathBuf) -> Leaving {
    Leaving {
        agent: Agent::Codex,
        proc: marspot::pidtree::ProcId { pid: 1, start_us: 1 },
        shell_pid: 1,
        home,
        session_id: None,
        cwd: Some("/w/a".into()),
        go_on: false,
    }
}

fn told(switch: &transcript::Switch) -> String {
    transcript::message(Agent::Codex, Path::new("/tmp/handoff.md"), false, switch, false)
}

#[test]
fn a_codex_daemon_at_a_socket_path_past_104_bytes_is_reached_and_the_pane_told_it_has_been_stopped() {
    let home = home_with_socket_of("note-long-daemon", 170);
    let daemon = Daemon::start(&home);
    let switch = switch_note(&leaving_codex(home), Some(Path::new(ROLLOUT)));
    assert!(switch.stopped, "{}", switch.left);
    assert_eq!(switch.left, "has exited; it had no active goal and no turn running");
    assert!(told(&switch).contains("has been stopped"), "{}", told(&switch));
    assert_eq!(daemon.asked("thread/goal/get"), 1);
    assert_eq!(daemon.asked("thread/turns/list"), 1);
}

#[test]
fn no_daemon_at_the_codex_home_leaves_nothing_carrying_the_thread_on() {
    let home = home_with_socket_of("note-no-daemon", 170);
    let switch = switch_note(&leaving_codex(home), Some(Path::new(ROLLOUT)));
    assert!(switch.stopped, "{}", switch.left);
    assert!(switch.left.contains("no codex daemon was running"), "{}", switch.left);
    assert!(told(&switch).contains("has been stopped"), "{}", told(&switch));
}

#[test]
fn a_socket_path_no_address_can_hold_is_named_and_the_pane_told_it_may_still_be_running() {
    let home = home_with_socket_of("note-over", 300);
    let sock = home.join(SOCKET);
    let switch = switch_note(&leaving_codex(home), Some(Path::new(ROLLOUT)));
    assert!(!switch.stopped, "{}", switch.left);
    assert!(switch.left.contains(&sock.display().to_string()), "{}", switch.left);
    assert!(told(&switch).contains("may still be running"), "{}", told(&switch));
}

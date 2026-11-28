use std::process::{Child, Command};
use std::time::{Duration, Instant};

use marspot::pidtree::{self, ProcId, ProcRow, SignalError};
use marspot::zombie::orphans::{self, Orphan};

use super::super::process_orphans::left_in;
use super::escalate;

fn row(pid: i32, ppid: i32, comm: &str) -> ProcRow {
    ProcRow {
        pid,
        ppid,
        comm: comm.into(),
        start_unix: 0,
        start_us: 1_000 + pid as u64,
        pgid: pid,
        tty_dev: 0,
        tty_fg_pgid: 0,
        status: libc::SSLEEP,
        execed: true,
    }
}

fn orphan(pid: i32, start_us: u64, name: &str) -> Orphan {
    Orphan {
        first_seen: 1_800_000_000 + pid as u64,
        id: ProcId { pid, start_us },
        sid: 493,
        name: name.into(),
        rules: String::new(),
        argv: String::new(),
    }
}

#[test]
fn process_panel_lists_what_closed_panes_left_as_recorded_and_still_running() {
    let all = vec![row(1, 0, "launchd"), row(40, 1, "codex"), row(41, 40, "node"), row(50, 1, "tail"), row(60, 1, "srv")];
    let body = orphans::format(&[
        orphan(40, 1_040, "spg"),
        // the pid is another process's now
        orphan(50, 7, "spg"),
        orphan(60, 1_060, ""),
        orphan(70, 1_070, "gone"),
    ]);
    let left = left_in(&body, &all).expect("two still run");
    let got: Vec<(&str, i32, usize)> = left.iter().map(|(l, t)| (l.as_str(), t.pid, t.children.len())).collect();
    assert_eq!(got, vec![("spg · codex", 40, 1), ("sid 493 · srv", 60, 0)], "with their descendants, a pane never seen by its sid");
    assert!(left_in(&orphans::format(&[orphan(70, 1_070, "x")]), &all).is_none(), "no entry when nothing they left runs");
    assert!(left_in("", &all).is_none());
}

/// A child that sleeps until it is killed, and is reaped when the test ends.
struct Sleeper(Child);

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Sleeper {
    fn exited_within(&mut self, limit: Duration) -> bool {
        let end = Instant::now() + limit;
        while Instant::now() < end {
            if self.0.try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }
}

#[test]
fn process_panel_ends_by_proc_id_and_refuses_a_pid_whose_start_time_changed() {
    let mut s = Sleeper(Command::new("/bin/sleep").arg("30").spawn().unwrap());
    let id = pidtree::proc_id(s.0.id() as i32).expect("running");
    let other = ProcId { start_us: id.start_us + 1, ..id };
    // what the kill button sends, against a row whose pid changed hands
    assert_eq!(pidtree::signal(other, libc::SIGTERM), Err(SignalError::Gone));
    let long_ago = Instant::now() - Duration::from_secs(60);
    let mut pending = vec![(other, long_ago)];
    escalate(&mut pending, Instant::now());
    assert!(pending.is_empty(), "dropped without a signal");
    assert!(!s.exited_within(Duration::from_millis(200)), "the process holding the pid was left alone");

    let mut pending = vec![(id, Instant::now())];
    escalate(&mut pending, Instant::now());
    assert_eq!(pending.len(), 1, "within the grace period it waits");
    escalate(&mut pending, Instant::now() + super::KILL_ESCALATION_GRACE);
    assert!(pending.is_empty());
    assert!(s.exited_within(Duration::from_secs(5)), "past it the same process gets SIGKILL");
}

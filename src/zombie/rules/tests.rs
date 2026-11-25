use std::path::{Path, PathBuf};

use super::*;

const NOW: u64 = 1_800_000_000;

fn l() -> Limits {
    Limits::default()
}

fn task<'a>(id: &'a str, command: &'a str, started: u64, silent: u64, cpu_ms: u64) -> BgTask<'a> {
    BgTask { id, command, started, written: Some(NOW - silent), cpu: Some(cpu_ms * 1_000_000) }
}

#[test]
fn zombie_b1_names_the_older_copies_of_a_command_and_never_the_newest() {
    let w = "bash watchdog.sh --stopped 3600 --wake 2700";
    let tasks = [task("old", w, 100, 9 * HOUR, 200), task("mid", w, 200, 3 * HOUR, 100), task("new", w, 300, 7 * HOUR, 0)];
    assert_eq!(b1(&tasks, NOW, &l()), vec![("old", "new"), ("mid", "new")]);
}

#[test]
fn zombie_b1_misses_at_its_thresholds_and_without_a_reading() {
    let w = "same";
    let quiet_enough = task("a", w, 100, 2 * HOUR - 1, 0);
    assert!(b1(&[quiet_enough, task("b", w, 200, 0, 0)], NOW, &l()).is_empty(), "one second short of two hours");
    let busy = task("a", w, 100, 9 * HOUR, 501);
    assert!(b1(&[busy, task("b", w, 200, 0, 0)], NOW, &l()).is_empty(), "past half a second of CPU");
    let edge = task("a", w, 100, 2 * HOUR, 500);
    assert_eq!(b1(&[edge, task("b", w, 200, 0, 0)], NOW, &l()), vec![("a", "b")], "at both limits it counts");
    let unknown = BgTask { cpu: None, ..task("a", w, 100, 9 * HOUR, 0) };
    assert!(b1(&[unknown, task("b", w, 200, 0, 0)], NOW, &l()).is_empty(), "no CPU window yet: not judged");
    let unwritten = BgTask { written: None, ..task("a", w, 100, 9 * HOUR, 0) };
    assert!(b1(&[unwritten, task("b", w, 200, 0, 0)], NOW, &l()).is_empty(), "no output time: not judged");
}

#[test]
fn zombie_b1_does_not_pair_commands_that_differ_by_an_argument() {
    // two watchdogs of one session: both scripts, both silent, different arguments
    let a = "bash watchdog.sh --stale 14400 --wake 2700 --stopped 1200";
    let b = "bash watchdog.sh --stale 14400 --wake 7200 --stopped 7200";
    assert!(b1(&[task("x", a, 100, 9 * HOUR, 0), task("y", b, 200, 9 * HOUR, 0)], NOW, &l()).is_empty());
}

#[test]
fn zombie_b3_waits_out_its_grace() {
    assert!(b3(Some(NOW - 30 * MINUTE), true, NOW, &l()));
    assert!(!b3(Some(NOW - 30 * MINUTE + 1), true, NOW, &l()), "within the grace");
    assert!(!b3(Some(NOW - HOUR), false, NOW, &l()), "the process is gone");
    assert!(!b3(None, true, NOW, &l()), "never told");
}

fn sub(last: Last, last_at: u64) -> Subagent<'static> {
    Subagent { id: "a1", first: 1000, last_at, last, finished: false, tasks_alive: 0 }
}

#[test]
fn zombie_s1_hits_a_subagent_nothing_will_wake() {
    let s = sub(Last::Idle, NOW - 30 * MINUTE);
    assert!(s1(&s, 1000, Some(NOW - 10 * MINUTE), NOW, &l()));
}

#[test]
fn zombie_s1_misses_at_each_of_its_conditions() {
    let s = sub(Last::Idle, NOW - 30 * MINUTE);
    let main = Some(NOW - 10 * MINUTE);
    assert!(!s1(&sub(Last::Idle, NOW - 30 * MINUTE + 1), 1000, main, NOW, &l()), "quiet one second short");
    assert!(!s1(&s, 1000, Some(NOW - 10 * MINUTE + 1), NOW, &l()), "main idle one second short");
    assert!(!s1(&s, 1000, None, NOW, &l()), "main session in a turn");
    assert!(!s1(&Subagent { tasks_alive: 1, ..s.clone() }, 1000, main, NOW, &l()), "a task of its own still runs");
    assert!(!s1(&Subagent { finished: true, ..s.clone() }, 1000, main, NOW, &l()), "finished");
    assert!(!s1(&s, 1001, main, NOW, &l()), "older than the agent process: the previous process's");
    assert!(!s1(&sub(Last::Busy, NOW - HOUR), 1000, main, NOW, &l()), "a tool call open");
}

#[test]
fn zombie_s2_hits_a_foreground_call_past_its_limit_plus_ten_minutes() {
    let at = NOW - HOUR;
    assert_eq!(s2(&sub(Last::Bash { at, timeout_ms: None }, at), NOW, &l()), Some(HOUR));
    let edge = NOW - 120 - 10 * MINUTE;
    assert_eq!(s2(&sub(Last::Bash { at: edge, timeout_ms: None }, edge), NOW, &l()), None, "exactly at the limit");
    let long = Last::Bash { at, timeout_ms: Some(3_600_000) };
    assert_eq!(s2(&sub(long, at), NOW, &l()), None, "its own timeout is an hour");
    assert_eq!(s2(&sub(Last::Idle, at), NOW, &l()), None);
}

fn argv(s: &str) -> Vec<String> {
    s.split(' ').map(str::to_string).collect()
}

#[test]
fn zombie_tail_arguments_are_read_as_tail_reads_them() {
    assert_eq!(tail_follows(&argv("tail -n +1 -f /x/e2e1.log")), Some(vec!["/x/e2e1.log"]));
    assert_eq!(tail_follows(&argv("tail -n0 -F a.log b.log")), Some(vec!["a.log", "b.log"]));
    assert_eq!(tail_follows(&argv("tail -fn 5 a.log")), Some(vec!["a.log"]));
    assert_eq!(tail_follows(&argv("tail -n 5 a.log")), None, "not following");
    assert_eq!(tail_follows(&argv("tail -f")), None, "standard input");
    assert_eq!(tail_follows(&argv("tail -f -- -odd")), Some(vec!["-odd"]));
}

#[test]
fn zombie_o1_hits_a_tail_whose_every_file_is_stale_or_gone() {
    let cwd = Path::new("/w");
    let written = |p: &Path| match p.to_str() {
        Some("/w/a.log") => Some(NOW - 24 * HOUR),
        Some("/w/b.log") => Some(NOW - 24 * HOUR + 1),
        _ => None,
    };
    assert!(o1(&argv("tail -F a.log gone.log"), cwd, written, NOW, &l()));
    assert!(!o1(&argv("tail -F a.log b.log"), cwd, written, NOW, &l()), "one file written within the day");
    assert!(!o1(&argv("tail a.log"), cwd, written, NOW, &l()), "a tail that does not follow");
}

#[test]
fn zombie_temporary_places_are_the_session_dir_a_projects_tmp_and_tmpdir() {
    let uid = 501;
    let td = Some(Path::new("/var/folders/x/T/"));
    let session = "/private/tmp/claude-501/-w-p/32f6eecf-5377-400d-958d-1ca24f1821d7/scratchpad/srv";
    assert!(temporary(Path::new(session), uid, td));
    assert!(!temporary(Path::new("/private/tmp/claude-501/-w-p/not-a-session/x"), uid, td));
    assert!(!temporary(Path::new("/private/tmp/claude-502/-w-p/32f6eecf-5377-400d-958d-1ca24f1821d7"), uid, td));
    let proj = PathBuf::from("/w/proj").join(".tmp");
    assert!(temporary(&proj.join("vite"), uid, td));
    assert!(temporary(&proj, uid, td));
    assert!(!temporary(&proj.join("worktrees").join("a"), uid, td), "a worktree is somebody's checkout");
    assert!(temporary(Path::new("/private/var/folders/x/T/srv"), uid, td));
    assert!(!temporary(Path::new("/tmp/app.sock"), uid, td), "a socket in /tmp is not a session's");
}

#[test]
fn zombie_o2_hits_a_quiet_old_server_from_a_gone_session_in_a_temporary_place() {
    let cwd = PathBuf::from("/w/proj").join(".tmp").join("srv");
    let o = Outside { started: NOW - 24 * HOUR, cwd: Some(&cwd), exe: None, session_gone: true, cpu: Some(5_000_000_000) };
    assert!(o2(&o, 501, None, NOW, &l()));
    assert!(!o2(&Outside { started: NOW - 24 * HOUR + 1, ..o.clone() }, 501, None, NOW, &l()), "a second younger");
    assert!(!o2(&Outside { session_gone: false, ..o.clone() }, 501, None, NOW, &l()), "its session still runs");
    assert!(!o2(&Outside { cpu: Some(5_000_000_001), ..o.clone() }, 501, None, NOW, &l()), "busy");
    let wt = PathBuf::from("/w/proj").join(".tmp").join("worktrees").join("x");
    assert!(!o2(&Outside { cwd: Some(&wt), ..o.clone() }, 501, None, NOW, &l()), "in a worktree");
    let exe = cwd.join("server");
    let home = Path::new("/w/proj");
    assert!(o2(&Outside { cwd: Some(home), exe: Some(&exe), ..o.clone() }, 501, None, NOW, &l()), "by its executable");
}

#[test]
fn zombie_z0_counts_unreaped_children_of_the_shell_and_the_panes_only() {
    let z = |pid, ppid| Zombie { pid, ppid, start_us: 1 };
    let all = [z(10, 1708), z(11, 1708), z(12, 500), z(13, 77)];
    assert_eq!(z0(&all, 1708, |p| p == 500), vec![(500, 1), (1708, 2)]);
    assert!(z0(&[z(13, 77)], 1708, |_| false).is_empty());
}

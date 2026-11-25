use std::process::{Child, Command, Stdio};
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use marspot::pidtree::{self, ProcId, ProcRow, ProcTable};

use super::*;
use crate::observe::tests::entry;

/// A real `/bin/sh -c`, standing in for a pane's shell.  It and
/// everything under it are killed when the test ends, so nothing is
/// left for launchd.
pub(crate) struct Shell(Child);

impl Shell {
    pub(crate) fn run(script: &str) -> Shell {
        let c = Command::new("/bin/sh").args(["-c", script]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
        Shell(c.unwrap())
    }

    pub(crate) fn pid(&self) -> i32 {
        self.0.id() as i32
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        let mut all = vec![self.pid()];
        let mut i = 0;
        while i < all.len() {
            all.extend(pidtree::child_pids(all[i]));
            i += 1;
        }
        let ids: Vec<ProcId> = all.iter().filter_map(|p| pidtree::proc_id(*p)).collect();
        for id in ids.iter().skip(1) {
            let _ = pidtree::signal(*id, libc::SIGKILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The processes under `pid` whose `comm` is `comm`, once there are `n`.
pub(crate) fn under(pid: i32, comm: &str, n: usize) -> Vec<ProcRow> {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        let mut found = Vec::new();
        let mut at = vec![pid];
        while let Some(p) = at.pop() {
            for c in pidtree::child_pids(p) {
                at.push(c);
                if let Some(r) = pidtree::proc_row(c).filter(|r| r.comm == comm) {
                    found.push(r);
                }
            }
        }
        if found.len() >= n {
            return found;
        }
        assert!(Instant::now() < end, "{n} {comm} under {pid} within 5 s: {found:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The one child of `pid`, once it has a `sleep` under it: macOS's
/// `/bin/sh` runs as another shell, so a child shell is found by place,
/// not by name.
fn looping_under(pid: i32) -> ProcRow {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if let [c] = pidtree::child_pids(pid)[..]
            && let Some(r) = pidtree::proc_row(c)
            && !under(c, "sleep", 0).is_empty()
        {
            return r;
        }
        assert!(Instant::now() < end, "no child with a sleep under {pid} within 5 s");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// An observation of the whole host now, with live panes 1, 2, …
/// whose shells are `shells`.
pub(crate) fn observe(shells: &[i32]) -> Observation {
    let entries = shells.iter().enumerate().map(|(i, s)| (entry(i as u64 + 1, 0, *s), true)).collect();
    Observation::from_parts(Instant::now(), ProcTable::take(), entries, |r| pidtree::proc_argv(r.pid).map(Into::into))
}

fn id_of(r: &ProcRow) -> String {
    format!("{}@{}", r.pid, r.start_us)
}

fn ask(obs: &Observation, guard: &Guard, asked: &str, tree: bool) -> Verdict {
    let mut a = Attributor::default();
    a.begin(obs, &Known::default());
    one(obs, &mut a, guard, asked, tree)
}

fn ends_within(id: ProcId, limit: Duration) -> bool {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if pidtree::proc_id(id.pid) != Some(id) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn a_process_of_the_callers_own_pane_is_ended() {
    let a = Shell::run("/bin/sleep 61 & /bin/sleep 62 & wait");
    let kids = under(a.pid(), "sleep", 2);
    let obs = observe(&[a.pid()]);
    let guard = Guard::of(&obs, 1, None);
    let v = ask(&obs, &guard, &id_of(&kids[0]), true);
    assert_eq!(v.outcome, Outcome::Stopped, "{v:?}");
    assert_eq!(v.signalled, vec![kids[0].id()]);
    assert!(v.argv.starts_with("/bin/sleep 6"), "{v:?}");
    assert!(ends_within(kids[0].id(), Duration::from_secs(5)), "SIGTERM did not end it");
    assert_eq!(pidtree::proc_id(kids[1].pid), Some(kids[1].id()), "only the one named ends");
}

#[test]
fn a_start_time_that_does_not_match_is_gone_and_nothing_is_sent() {
    let a = Shell::run("/bin/sleep 61 & wait");
    let kid = under(a.pid(), "sleep", 1).remove(0);
    let obs = observe(&[a.pid()]);
    let guard = Guard::of(&obs, 1, None);
    let forged = format!("{}@{}", kid.pid, kid.start_us + 1);
    let v = ask(&obs, &guard, &forged, true);
    assert_eq!(v.outcome, Outcome::Gone, "{v:?}");
    assert!(v.signalled.is_empty());
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(pidtree::proc_id(kid.pid), Some(kid.id()), "the refused id was signalled");
    let v = ask(&obs, &guard, "not-an-id", true);
    assert_eq!(v.outcome, Outcome::Refused);
}

#[test]
fn another_panes_process_is_not_yours() {
    let a = Shell::run("/bin/sleep 61 & wait");
    let b = Shell::run("/bin/sleep 62 & wait");
    let theirs = under(b.pid(), "sleep", 1).remove(0);
    let obs = observe(&[a.pid(), b.pid()]);
    let guard = Guard::of(&obs, 1, None);
    let v = ask(&obs, &guard, &id_of(&theirs), true);
    assert_eq!(v.outcome, Outcome::NotYours, "{v:?}");
    assert!(v.message.contains("pane 2"), "{v:?}");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(pidtree::proc_id(theirs.pid), Some(theirs.id()));
}

#[test]
fn the_callers_shell_and_ancestors_are_refused() {
    // the inner shell runs its sleep as a child, not by exec
    let a = Shell::run("/bin/sh -c '/bin/sleep 61; true' & wait");
    let caller = under(a.pid(), "sleep", 1).remove(0);
    let obs = observe(&[a.pid()]);
    let guard = Guard::of(&obs, 1, Some(caller.pid));
    let parent = pidtree::proc_row(caller.ppid).unwrap();
    for (row, why) in [(&parent, "ancestors"), (&caller, "ancestors"), (obs.table.get(a.pid()).unwrap(), "shell")] {
        let v = ask(&obs, &guard, &id_of(row), true);
        assert_eq!(v.outcome, Outcome::Refused, "{v:?}");
        assert!(v.message.contains(why), "{v:?}");
    }
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(pidtree::proc_id(parent.pid), Some(parent.id()));
}

#[test]
fn a_loop_that_restarts_its_child_is_ended_whole_and_leaves_nothing_adopted() {
    let started = pidtree::proc_id(std::process::id() as i32).unwrap();
    let a = Shell::run("/bin/sh -c 'while true; do /bin/sleep 0.05; done' & wait");
    let looping = looping_under(a.pid());
    let obs = observe(&[a.pid()]);
    let v = ask(&obs, &Guard::of(&obs, 1, None), &id_of(&looping), true);
    assert_eq!(v.outcome, Outcome::Stopped, "{v:?}");
    // between two sleeps the loop has no child, so its sleep may or may not be in the tree
    assert_eq!(v.signalled.first(), Some(&looping.id()), "{v:?}");
    for id in &v.signalled {
        assert!(ends_within(*id, Duration::from_secs(5)), "{id:?} still runs");
    }
    std::thread::sleep(Duration::from_millis(500));
    // SAFETY: getpgrp has no preconditions
    let pgid = unsafe { libc::getpgrp() };
    let adopted: Vec<ProcRow> = ProcTable::take()
        .rows()
        .iter()
        .filter(|r| r.ppid == 1 && r.comm == "sleep" && r.pgid == pgid && r.start_us >= started.start_us)
        .cloned()
        .collect();
    assert!(adopted.is_empty(), "left for launchd: {adopted:?}");
    assert!(under(a.pid(), "sleep", 0).is_empty(), "the loop started another");
}

#[test]
fn force_kills_what_ignores_sigterm_later_without_holding_the_caller() {
    let a = Shell::run("trap '' TERM; /bin/sh -c 'while true; do /bin/sleep 0.2; done' & wait");
    let looping = looping_under(a.pid());
    let obs = observe(&[a.pid()]);
    let v = ask(&obs, &Guard::of(&obs, 1, None), &id_of(&looping), true);
    assert_eq!(v.outcome, Outcome::Stopped, "{v:?}");
    let (tx, rx) = channel();
    let asked = Instant::now();
    let queued = force::after(Instant::now() + Duration::from_millis(400), v.signalled.clone(), Box::new(move |k| tx.send(k.expect("sent, not dropped")).unwrap()));
    assert!(queued);
    assert!(asked.elapsed() < Duration::from_millis(100), "after() waited {:?}", asked.elapsed());
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(pidtree::proc_id(looping.pid), Some(looping.id()), "SIGTERM is ignored, so it runs until the kill");
    let killed = rx.recv_timeout(Duration::from_secs(5)).expect("the kill came due");
    assert!(killed.iter().any(|(id, r)| *id == looping.id() && r.is_ok()), "{killed:?}");
    assert!(ends_within(looping.id(), Duration::from_secs(5)));
}

#[test]
fn a_force_thread_that_died_is_replaced_by_the_next_request() {
    assert!(force::after(Instant::now(), Vec::new(), Box::new(|_| panic!("a done that panics"))));
    std::thread::sleep(Duration::from_millis(200));
    let (tx, rx) = channel();
    assert!(force::after(Instant::now(), Vec::new(), Box::new(move |k| tx.send(k.is_some()).unwrap())));
    assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(true), "the kill after the panic never came due");
}

#[test]
fn kills_waiting_at_an_exec_are_told_they_were_dropped() {
    let (tx, rx) = channel();
    let later = Instant::now() + Duration::from_secs(60);
    assert!(force::after(later, Vec::new(), Box::new(move |k| tx.send(k.is_none()).unwrap())));
    force::abandon();
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)), Ok(true));
}

#[test]
fn a_same_name_process_in_the_agents_group_is_the_agent() {
    // a row of this process renamed: the test binary's own name reads as a marspot process
    let me = ProcRow { comm: "gemini".into(), ..pidtree::proc_row(std::process::id() as i32).unwrap() };
    let group = Some((me.pgid, me.comm.clone()));
    let guard = Guard { sid: 1, shell: None, agent: None, agent_group: group, ancestors: HashSet::new() };
    assert_eq!(guard.forbids(&me), Some("it is your pane's agent"));
    let other = ProcRow { comm: "node".into(), ..me.clone() };
    assert_eq!(guard.forbids(&other), None, "another name in the group is not the agent");
    let elsewhere = ProcRow { pgid: me.pgid + 1, ..me };
    assert_eq!(guard.forbids(&elsewhere), None);
}

#[test]
fn a_descendant_that_is_another_panes_is_left_alone_with_its_tree() {
    let a = Shell::run("/bin/sh -c '/bin/sh -c \"/bin/sleep 62 & wait\" & /bin/sleep 61 & wait' & wait");
    let _ = under(a.pid(), "sleep", 2);
    let mid = pidtree::proc_row(pidtree::child_pids(a.pid())[0]).unwrap();
    let kids: Vec<ProcRow> = pidtree::child_pids(mid.pid).into_iter().filter_map(pidtree::proc_row).collect();
    let inner = kids.iter().find(|r| r.comm != "sleep").unwrap().clone();
    let inner_sleep = under(inner.pid, "sleep", 1).remove(0);
    let own_sleep = kids.iter().find(|r| r.comm == "sleep").unwrap().clone();
    // pane 2's shell runs under pane 1's
    let obs = observe(&[a.pid(), inner.pid]);
    let v = ask(&obs, &Guard::of(&obs, 1, None), &id_of(&mid), true);
    let alive = [inner.id(), inner_sleep.id()].map(|id| pidtree::proc_id(id.pid) == Some(id));
    // the inner shell outlives the tree it was in, so the shell's cleanup no longer finds it
    for id in [inner_sleep.id(), inner.id()] {
        let _ = pidtree::signal(id, libc::SIGKILL);
    }
    assert_eq!(v.outcome, Outcome::Stopped, "{v:?}");
    assert!(v.signalled.contains(&own_sleep.id()), "{v:?}");
    assert!(!v.signalled.contains(&inner.id()) && !v.signalled.contains(&inner_sleep.id()), "{v:?}");
    assert!(ends_within(mid.id(), Duration::from_secs(5)));
    assert_eq!(alive, [true, true], "pane 2's processes were signalled");
}

#[test]
fn a_process_that_ended_is_gone() {
    let a = Shell::run("/bin/sleep 61 & wait");
    let kid = under(a.pid(), "sleep", 1).remove(0);
    let obs = observe(&[a.pid()]);
    pidtree::signal(kid.id(), libc::SIGKILL).unwrap();
    assert!(ends_within(kid.id(), Duration::from_secs(5)));
    let v = ask(&obs, &Guard::of(&obs, 1, None), &id_of(&kid), true);
    assert_eq!(v.outcome, Outcome::Gone, "{v:?}");
}

#[test]
fn the_ledger_lists_what_was_told_to_each_pane() {
    let dir = std::env::temp_dir().join(format!("zombie-stop-told-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("told.tsv");
    std::fs::write(&path, "4\t12@34\t1700000000\n5\t56@78\t1700000001\n4\tagent:a1\t1700000002\nbroken\n").unwrap();
    let four = told(&path, 4);
    assert!(four.contains("12@34") && four.contains("agent:a1") && four.len() == 2, "{four:?}");
    assert!(told(&dir.join("none.tsv"), 4).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn argv_is_cut_on_a_character_boundary() {
    assert_eq!(clip("abc".into(), 200), "abc");
    assert_eq!(clip("ééé".into(), 3), "é");
}

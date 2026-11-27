use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;

use marspot_peer::control::AgentKind;
use marspot_peer::registry::Entry;

use super::notify;
use super::pending::{GIVE_UP_MS, Notice, SLOW_MS};
use super::table::Event;
use super::tests::{feed, shared_at, started, temp};
use super::tests_review::panes;
use super::{records, service, work};
use crate::observe::Observation;
use crate::plugins::pty_op::front::tests::{shell_on_a_pty, wait_for};

fn record(pid: i32, socket: &str) -> Entry {
    Entry {
        pid,
        session_id: Some(format!("s{pid}")),
        name: Some(format!("own-{pid}")),
        cwd: None,
        socket: Some(socket.into()),
        status: Some("idle".into()),
        proc_start: None,
        started_at_ms: None,
        features: Vec::new(),
        ours: false,
        class: None,
        relay: None,
        owner: None,
        agent: None,
        agent_pid: None,
        spool: None,
    }
}

fn delivered() -> Vec<Event> {
    vec![Event::started(started(10, "claude", AgentKind::Claude)), Event::TaskDelivered]
}

/// A caller in a shadow pane, started under a wrapper (`timeout 420
/// claude`): the program in front is the wrapper, so the pane names
/// leave it out, and its notice goes to its own inbox all the same.
#[test]
fn helpers_a_notice_reaches_a_caller_in_a_shadow_pane_its_name_misses() {
    let (mut sh, mut m) = shell_on_a_pty();
    let shell = sh.id() as i32;
    m.write_all(b"(exec -a timeout /bin/bash -c '(exec -a claude /bin/sleep 30) & wait')\n").unwrap();
    let look = || {
        let mut entry = crate::observe::tests::entry(1, 1, shell);
        entry.shadow_of = 488;
        Observation::from_parts(std::time::Instant::now(), marspot::pidtree::ProcTable::take(), vec![(entry, true)], |r| {
            marspot::pidtree::proc_argv(r.pid).map(std::sync::Arc::<[String]>::from)
        })
    };
    let claude_of = |obs: &Observation| obs.session(1)?.find("claude").map(|p| p.row.pid);
    let ready = wait_for(|| claude_of(&look()).is_some() && look().session(1).is_some_and(|x| x.front().is_some()));
    let obs = look();
    let started_procs: Vec<i32> = obs.session(1).map(|x| x.descendants.iter().map(|p| p.row.pid).collect()).unwrap_or_default();
    let socket = format!("/tmp/mh-notice-{}.sock", std::process::id());
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).unwrap();
    let outcome = claude_of(&obs).map(|claude| {
        let not_pane_agent = obs.session(1).unwrap().agent(|p| p.name() == "claude").is_none();
        // a session elsewhere is no candidate
        let entries = vec![record(claude, &socket), record(1, "/tmp/elsewhere.sock")];
        let sent = notify::deliver_among(&entries, &[], (1, "shot#4113"), "[marspot helper notice] done", &obs);
        let missed = notify::deliver_among(&entries[1..], &[], (1, "shot#4113"), "x", &obs);
        (not_pane_agent, sent, missed)
    });
    let line = outcome.as_ref().filter(|o| o.1.is_ok()).map(|_| {
        let (conn, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(conn).read_line(&mut line).unwrap();
        line
    });
    let _ = sh.kill();
    let _ = sh.wait();
    for pid in started_procs {
        let _ = marspot::pidtree::kill_pid(pid, libc::SIGKILL);
    }
    let _ = std::fs::remove_file(&socket);
    assert!(ready, "the wrapper and its claude never showed up");
    let (not_pane_agent, sent, missed) = outcome.expect("no claude in the pane");
    assert!(not_pane_agent, "claude is not the program in front");
    assert_eq!(sent, Ok(()));
    assert!(line.is_some_and(|l| l.contains("[marspot helper notice] done")));
    assert!(missed.unwrap_err().starts_with("no pane is named \"shot#4113\""), "no session of its own: the name's answer");
}

/// The cap: a notice still refused 30 minutes after its first refusal is
/// dropped, its stop written, and nothing comes due for it again.
#[test]
fn helpers_an_undelivered_notice_is_given_up_at_the_cap() {
    let dir = temp("giveup");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 1, [delivered(), vec![Event::ResultChanged]].concat());
    let front = panes(&[(10, 55)]);
    let t0 = records::now_ms();
    let sha = "cd".repeat(32);
    let n = Notice { parent: 10, body: "x".into(), events: vec![Event::IdleNoticed, Event::ResultDelivered { sha }], tries: 1, ..Notice::default() };
    work::not_delivered(&s, 1, n, "connection refused", &front, t0);
    let mut st = s.lock();
    assert_eq!(st.undelivered.next_give_up_ms(), Some(t0 + GIVE_UP_MS));
    // past the slow retries it waits for a new program, and the clock
    assert_eq!(st.undelivered.retries_due(t0 + SLOW_MS[2]), vec![1]);
    assert_eq!(st.undelivered.retries_due(t0 + GIVE_UP_MS), vec![1], "due once more at the cap");
    let mut n = st.undelivered.take(1).unwrap();
    drop(st);
    n.tries += 1;
    work::not_delivered(&s, 1, n, "connection refused", &front, t0 + GIVE_UP_MS);
    let st = s.lock();
    assert!(!st.undelivered.holds(1), "given up");
    assert_eq!((st.undelivered.next_retry_ms(), st.undelivered.next_give_up_ms()), (None, None));
    drop(st);
    let h = s.table().map[&1].clone();
    assert_eq!(service::state_of(&h), "stopped", "its stop shows");
    assert!(h.result_new, "the result was never delivered");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A parent pane that is gone stops a notice at its first refusal.
#[test]
fn helpers_a_notice_to_a_gone_parent_is_given_up_at_once() {
    let dir = temp("goneparent");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 1, delivered());
    let mut gone = panes(&[(10, 55)]);
    gone.sessions[0].live = false;
    let notice = || Notice { parent: 10, body: "x".into(), events: vec![Event::IdleNoticed], tries: 1, ..Notice::default() };
    // what a put-away observer serves knows no session at all: kept
    work::not_delivered(&s, 1, notice(), "no pane is named \"proj#10\"", &panes(&[]), records::now_ms());
    assert!(s.lock().undelivered.take(1).is_some(), "an empty observation gives nothing up");
    for obs in [panes(&[(11, 55)]), gone] {
        work::not_delivered(&s, 1, notice(), "no pane is named \"proj#10\"", &obs, records::now_ms());
        assert!(!s.lock().undelivered.holds(1));
        assert_eq!(service::state_of(&s.table().map[&1]), "stopped");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// What runs in pane 1 as `script`, looked at once `ready` says its
/// processes are there, and the shell to end.
fn pane_running(script: &[u8], ready: impl Fn(&Observation) -> bool) -> (std::process::Child, Observation, bool) {
    let (sh, mut m) = shell_on_a_pty();
    let shell = sh.id() as i32;
    m.write_all(script).unwrap();
    let look = || {
        let entry = crate::observe::tests::entry(1, 1, shell);
        Observation::from_parts(std::time::Instant::now(), marspot::pidtree::ProcTable::take(), vec![(entry, true)], |r| {
            marspot::pidtree::proc_argv(r.pid).map(std::sync::Arc::<[String]>::from)
        })
    };
    let seen = wait_for(|| ready(&look()));
    (sh, look(), seen)
}

fn end(mut sh: std::process::Child, obs: &Observation) {
    let _ = sh.kill();
    let _ = sh.wait();
    for p in obs.session(1).map(|x| x.descendants.as_slice()).unwrap_or_default() {
        let _ = marspot::pidtree::kill_pid(p.row.pid, libc::SIGKILL);
    }
}

/// The pids in pane 1 whose program is called `name`, with their parents.
fn named(obs: &Observation, name: &str) -> Vec<(i32, i32)> {
    let procs = obs.session(1).map(|x| x.descendants.as_slice()).unwrap_or_default();
    procs.iter().filter(|p| p.name() == name).map(|p| (p.row.pid, p.row.ppid)).collect()
}

/// The outer and the inner of two claudes in pane 1 with a process that
/// is not a claude between them.
///
/// Until a forked child execs it carries its parent's argv: the outer
/// claude's fork that is about to become the inner shell reads as a
/// second claude right under it, so two claudes alone are not yet the
/// nesting.
fn nested(obs: &Observation) -> Option<(i32, i32)> {
    let procs = obs.session(1)?.descendants.as_slice();
    let parent = |pid: i32| procs.iter().find(|p| p.row.pid == pid).map(|p| p.row.ppid);
    let both = named(obs, "claude");
    match both[..] {
        [(a, _), (b, pb)] if parent(pb) == Some(a) && pb != a => Some((a, b)),
        [(a, pa), (b, _)] if parent(pa) == Some(b) && pa != b => Some((b, a)),
        _ => None,
    }
}

/// A claude in the pane that started an agent of its own through a shell
/// of its own, both registered: the notice is the outer one's.
#[test]
fn helpers_a_notice_goes_to_the_outer_of_two_nested_sessions() {
    let script = b"(exec -a claude /bin/bash -c '/bin/bash -c \"(exec -a claude /bin/sleep 30) & wait\" & wait')\n";
    let (sh, obs, seen) = pane_running(script, |o| nested(o).is_some());
    end(sh, &obs);
    assert!(seen, "two claudes with a shell between them never showed up");
    let (outer, inner) = nested(&obs).expect("the nesting stays once it is there");
    let entries = vec![record(inner, "/tmp/inner.sock"), record(outer, "/tmp/outer.sock")];
    assert_eq!(super::inbox_of::in_pane(&entries, &obs, 1).as_deref(), Some("/tmp/outer.sock"));
    assert_eq!(super::inbox_of::in_pane(&entries[..1], &obs, 1).as_deref(), Some("/tmp/inner.sock"), "alone, the inner one is the top");
}

/// Two registered sessions side by side under the shell: neither is the
/// pane's, and the notice goes to none.
#[test]
fn helpers_a_notice_goes_to_neither_of_two_sibling_sessions() {
    let script = b"(exec -a claude /bin/sleep 30) & (exec -a codex /bin/sleep 30) & wait\n";
    let (sh, obs, seen) = pane_running(script, |o| named(o, "claude").len() == 1 && named(o, "codex").len() == 1);
    end(sh, &obs);
    assert!(seen, "the two sessions never showed up");
    let (claude, codex) = (named(&obs, "claude")[0].0, named(&obs, "codex")[0].0);
    let entries = vec![record(claude, "/tmp/claude.sock"), record(codex, "/tmp/codex.sock")];
    assert_eq!(super::inbox_of::in_pane(&entries, &obs, 1), None);
    assert_eq!(super::inbox_of::in_pane(&entries[1..], &obs, 1).as_deref(), Some("/tmp/codex.sock"));
}

use super::*;
use std::path::PathBuf;

pub(crate) fn row(pid: i32, ppid: i32, comm: &str) -> ProcRow {
    ProcRow {
        pid,
        ppid,
        comm: comm.into(),
        start_unix: 0,
        start_us: pid as u64,
        pgid: pid,
        tty_dev: 1,
        tty_fg_pgid: 1,
        status: libc::SSLEEP,
    }
}

pub(crate) fn entry(id: u64, pid: i32, shell: i32) -> SessionEntry {
    SessionEntry {
        id,
        pid,
        socket: PathBuf::from("/nonexistent/sock"),
        cols: 80,
        rows: 24,
        title: String::new(),
        cwd: "/tmp".into(),
        proto_version: 1,
        created_at_unix: 0,
        shm_name: String::new(),
        shell_child_pid: shell,
    }
}

fn argv_of(names: &'static [(i32, &'static str)]) -> impl FnMut(&ProcRow) -> Option<Arc<[String]>> {
    move |r| {
        names
            .iter()
            .find(|(p, _)| *p == r.pid)
            .map(|(_, a)| a.split(' ').map(str::to_string).collect::<Vec<_>>().into())
    }
}

#[test]
fn a_live_session_lists_what_runs_under_its_shell_with_names() {
    let table = ProcTable::from_rows(vec![
        row(1, 0, "launchd"),
        row(10, 1, "marspot-session"),
        row(11, 10, "zsh"),
        row(12, 11, "2.1.177"),
        row(13, 12, "smix-mcp"),
        row(20, 1, "other"),
    ]);
    let obs = Observation::from_parts(
        Instant::now(),
        table,
        vec![(entry(5, 10, 11), true), (entry(6, 99, 20), false)],
        argv_of(&[(12, "/opt/bin/claude --resume x"), (13, "smix-mcp")]),
    );
    let s = obs.session(5).expect("session 5");
    assert_eq!(s.shell.map(|i| i.pid), Some(11));
    let names: Vec<&str> = s.descendants.iter().map(Proc::name).collect();
    assert_eq!(names, vec!["claude", "smix-mcp"]);
    assert_eq!(s.find("claude").map(|p| p.row.pid), Some(12));
    assert_eq!(
        s.find("claude").and_then(Proc::cmdline).as_deref(),
        Some("/opt/bin/claude --resume x")
    );
    let dead = obs.session(6).expect("session 6");
    assert!(dead.shell.is_none() && dead.descendants.is_empty(), "a dead session is not walked");
    assert_eq!(obs.live().count(), 1);
    assert_eq!(obs.shell_pid(6), 20, "the registry's word, live or not");
    assert_eq!(obs.shell_pid(7), 0);
}

/// C8: an entry with no shell pid must not be read as "the shell is
/// launchd", whose subtree is the whole host.
#[test]
fn a_session_without_a_shell_pid_has_no_descendants() {
    let table = ProcTable::from_rows(vec![
        row(0, 0, "kernel_task"),
        row(1, 0, "launchd"),
        row(10, 1, "marspot-session"),
        row(30, 1, "claude"),
        row(31, 0, "x"),
    ]);
    let obs = Observation::from_parts(
        Instant::now(),
        table,
        vec![(entry(1, 10, 0), true), (entry(2, 10, 1), true), (entry(3, 10, -4), true)],
        argv_of(&[(30, "claude")]),
    );
    for sid in [1, 2, 3] {
        let s = obs.session(sid).unwrap();
        assert!(s.shell.is_none(), "session {sid}");
        assert!(s.descendants.is_empty(), "session {sid}");
        assert!(s.find("claude").is_none());
    }
}

#[test]
fn the_argv_cache_reads_once_and_forgets_a_process_that_exited() {
    let mut child = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let pid = child.id() as i32;
    // the child's image is ours until it has exec'd
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut cache = ArgvCache::default();
    let mut got = None;
    while Instant::now() < deadline {
        let table = ProcTable::take();
        let r = table.get(pid).expect("child in table").clone();
        if r.comm == "sleep" {
            got = Some(r);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let r = got.expect("the child exec'd sleep");
    let a = cache.get(&r).expect("argv of the child");
    assert_eq!(&a[..], &["/bin/sleep".to_string(), "30".to_string()][..]);
    let reads = cache.reads();
    assert!(cache.get(&r).is_some());
    assert_eq!(cache.reads(), reads, "a second look is a cache hit");
    let mut renamed = r.clone();
    renamed.comm = "other".into();
    let _ = cache.get(&renamed);
    assert_eq!(cache.reads(), reads + 1, "a new image is read again");

    cache.prune(&ProcTable::take());
    assert_eq!(cache.len(), 1, "a running process stays");
    child.kill().unwrap();
    child.wait().unwrap();
    cache.prune(&ProcTable::take());
    assert_eq!(cache.len(), 0, "an exited process is dropped");
}

#[test]
fn an_environment_is_read_once_per_process_and_forgotten_with_it() {
    let me = pidtree::proc_row(std::process::id() as i32).expect("own row");
    let mut env = EnvCache::default();
    let path = env.value(&me, "PATH");
    assert!(path.is_some(), "the test runs with a PATH");
    assert_eq!(env.value(&me, "PATH"), path);
    assert_eq!(env.value(&me, "MARSPOT_NO_SUCH_VARIABLE"), None);
    assert_eq!(env.value(&me, "PATH"), path);
    assert_eq!(env.value(&me, "MARSPOT_NO_SUCH_VARIABLE"), None);
    assert_eq!(env.reads(), 2, "an unset variable is an answer too");
    let without_me = Observation::from_parts(Instant::now(), ProcTable::from_rows(Vec::new()), Vec::new(), |_| None);
    env.prune(&without_me);
    assert_eq!(env.value(&me, "PATH"), path);
    assert_eq!(env.reads(), 3, "a pruned process is read again");
}

#[test]
fn two_consumers_in_one_interval_cause_one_walk() {
    let obs = Observer::spawn(Duration::from_secs(3600));
    assert_eq!(obs.walks(), 1, "the first observation is built at spawn");
    let a = obs.fresh(INTERVAL);
    let b = std::thread::scope(|s| s.spawn(|| obs.fresh(INTERVAL)).join().unwrap());
    let c = obs.latest();
    assert_eq!(obs.walks(), 1, "fresh enough is shared");
    assert!(Arc::ptr_eq(&a, &b) && Arc::ptr_eq(&b, &c));

    let d = obs.fresh(Duration::ZERO);
    assert_eq!(obs.walks(), 2, "a consumer that needs now gets a new walk");
    assert!(d.at > a.at);
    let e = obs.fresh(Duration::from_secs(1));
    assert!(Arc::ptr_eq(&d, &e), "and the next one shares it");
    assert_eq!(obs.walks(), 2);
}

#[test]
fn the_observer_rebuilds_on_its_interval() {
    let obs = Observer::spawn(Duration::from_millis(20));
    let first = obs.latest();
    let deadline = Instant::now() + Duration::from_secs(5);
    while obs.walks() < 3 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(obs.walks() >= 3, "walks {}", obs.walks());
    assert!(obs.latest().at > first.at);
}

/// A consumer waiting for a fresh observation must not wait forever
/// for an observer that has died.
#[test]
fn a_dead_observer_serves_its_last_observation_instead_of_hanging() {
    let mut builds = 0;
    let obs = Arc::new(Observer::spawn_with(Duration::from_secs(3600), move |_| {
        builds += 1;
        assert!(builds == 1, "injected failure of the second build");
        Observation::from_parts(Instant::now(), ProcTable::from_rows(Vec::new()), Vec::new(), |_| None)
    }));
    let first = obs.latest();
    let (tx, rx) = std::sync::mpsc::channel();
    let waiter = Arc::clone(&obs);
    // detached, so a hang fails the test instead of hanging it
    std::thread::spawn(move || tx.send(waiter.fresh(Duration::ZERO)).unwrap());
    let got = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("fresh() returns after the observer died");
    assert!(Arc::ptr_eq(&got, &first), "the last observation it made");
    let again = obs.fresh(Duration::ZERO);
    assert!(Arc::ptr_eq(&again, &first), "and every later call returns at once");
}

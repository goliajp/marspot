//! A pane's session moving to another account, against a real worker:
//! a real process standing in for the TUI, a real inbox and relay, and
//! a stand-in daemon on the home's control socket that writes down what
//! it is asked.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

use marspot::pidtree::{self, ProcId};
use marspot_peer::envelope::{self, Header, Mode};
use marspot_peer::frame::{self, Inbound, Priority};
use marspot_peer::json::{self, Value};
use marspot_term::test_env::EnvGuard;

use super::bind::Pane;
use super::fake_daemon::{thread, Daemon};
use super::moving::{self, running_descendant, Ask, Held, Loaded, Verdict};
use super::worker::{Worker, CHANNEL};

const SID: u64 = 7;
const T: &str = "01a133f4-8e7b-7331-9630-9c40ff4b467f";

struct Rig {
    root: PathBuf,
    home: PathBuf,
    cwd: String,
    daemon: Daemon,
    tui: Child,
    codex: ProcId,
    w: Worker,
    _env: EnvGuard,
}

/// A process the way the worker sees a codex TUI: its `CODEX_HOME` and
/// its working directory.
fn tui(home: &Path, cwd: &str) -> (Child, ProcId) {
    let child = Command::new("/bin/sleep").arg("60").env("CODEX_HOME", home).current_dir(cwd).spawn().unwrap();
    let id = seen(child.id() as i32);
    (child, id)
}

fn seen(pid: i32) -> ProcId {
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        if let Some(id) = pidtree::proc_id(pid) {
            return id;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the stand-in never showed up")
}

/// Socket paths have a 104-byte ceiling, which the system temp
/// directory leaves no room under.
fn rig() -> Rig {
    let root = PathBuf::from(format!("/tmp/pbm-{}", marspot_peer::rand::hex(6)));
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let home = root.join("c");
    let cwd = root.join("w");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd = cwd.to_string_lossy().into_owned();
    let mut env = EnvGuard::new();
    env.set("HOME", &root)
        .set("MARSPOT_STATE_DIR", root.join("s"))
        .set("MARSPOT_PEER_BIN", marspot_term::test_env::built_binary("marspot-peer"));
    let daemon = Daemon::start(&home);
    daemon.state.lock().unwrap().threads.push((T.into(), thread(&cwd, None, "idle")));
    let (tui, codex) = tui(&home, &cwd);
    let (tx, rx) = sync_channel(CHANNEL);
    let mut w = Worker::new(tx, rx, root.to_string_lossy().into_owned());
    bind(&mut w, &home, &cwd, codex);
    Rig { root, home, cwd, daemon, tui, codex, w, _env: env }
}

fn bind(w: &mut Worker, home: &Path, cwd: &str, codex: ProcId) {
    let pane = Pane {
        sid: SID,
        home: home.to_path_buf(),
        cwd: cwd.to_string(),
        argv_thread: Some(T.into()),
        title_thread: None,
        title_moved: false,
        title_expected: false,
        started_ms: 0,
        up_ms: 0,
        alone: true,
    };
    w.bind(&pane, T, codex, &["codex".into()]).expect("bound");
}

impl Rig {
    fn verdict(&mut self) -> Verdict {
        self.w.on_move(Ask::Begin { sid: SID, codex: self.codex });
        let mut v = moving::verdicts();
        assert_eq!(v.len(), 1, "{v:?}");
        let (sid, codex, verdict) = v.remove(0);
        assert_eq!((sid, codex), (SID, self.codex));
        verdict
    }

    /// Send the pane a message as a peer would and return the receipt
    /// it got back, if any.
    fn message(&mut self, id: &str) -> Option<Value> {
        let me = self.root.join(format!("r-{id}.sock"));
        let listener = UnixListener::bind(&me).unwrap();
        let from = format!("uds:{}", me.display());
        let h = Header { from: from.clone(), from_name: Some("peer".into()), from_mode: Some(Mode::Bypass) };
        let line = frame::user_line(&envelope::encode(&h, "hello"), &from, Priority::Next, id);
        let Ok(Inbound::User(f)) = frame::parse_line(&line) else { panic!("a user frame") };
        let key = self.w.bindings[&SID].key;
        self.w.deliver(key, f, None);
        listener.set_nonblocking(true).unwrap();
        let end = Instant::now() + Duration::from_secs(3);
        while Instant::now() < end {
            if let Ok((c, _)) = listener.accept() {
                c.set_nonblocking(false).unwrap();
                let mut l = String::new();
                BufReader::new(c).read_line(&mut l).unwrap();
                return Some(json::parse(&l).unwrap());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    fn queue(&self, sql: &str) {
        let db = self.home.join("queue_1.sqlite");
        let ok = Command::new("/usr/bin/sqlite3").arg(&db).arg(sql).status().unwrap().success();
        assert!(ok, "{sql}");
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.w.teardown();
        let _ = self.tui.kill();
        let _ = self.tui.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn a_message_while_moving_is_refused_and_never_reaches_the_old_home() {
    let mut r = rig();
    assert_eq!(r.verdict(), Verdict::Go { thread: T.into(), home: r.home.clone() });
    let receipt = r.message("m1").expect("the sender is told");
    assert_eq!(receipt.str_at("status_detail"), Some("refused"), "{}", receipt.to_json());
    assert_eq!(receipt.str_at("orig_msg_id"), Some("m1"));
    assert_eq!(r.daemon.asked("thread/queue/add"), 0, "nothing queued in the home being left");
    assert_eq!(r.verdict(), Verdict::Held(Held::Moving), "one move at a time");

    // the move did not happen and the TUI is still there: unmarked
    r.w.on_move(Ask::End { sid: SID, codex: r.codex });
    assert_eq!(r.message("m2"), None, "an accepted message gets no receipt");
    assert_eq!(r.daemon.asked("thread/queue/add"), 1);
}

#[test]
fn the_new_process_binds_unmarked_and_is_told_to_go_on() {
    let mut r = rig();
    assert!(matches!(r.verdict(), Verdict::Go { .. }));
    // the old TUI ends; its binding goes, and the mark with it
    let _ = r.tui.kill();
    let _ = r.tui.wait();
    let old = r.w.bindings.remove(&SID).unwrap();
    r.w.unbind(SID, old);
    let (tui, codex) = tui(&r.home, &r.cwd.clone());
    r.tui = tui;
    r.codex = codex;
    bind(&mut r.w, &r.home.clone(), &r.cwd.clone(), codex);
    assert_eq!(r.daemon.asked("thread/queue/add"), 1, "the go-on sentence");
    assert_eq!(r.message("m3"), None, "the new binding takes mail");
    assert_eq!(r.daemon.asked("thread/queue/add"), 2);
}

#[test]
fn a_session_at_work_does_not_move() {
    let mut r = rig();
    r.daemon.state.lock().unwrap().threads[0].1 = thread(&r.cwd, None, "active");
    assert_eq!(r.verdict(), Verdict::Held(Held::Busy));
    r.daemon.state.lock().unwrap().threads[0].1 = thread(&r.cwd, None, "idle");
    r.w.bindings.get_mut(&SID).unwrap().active_turn = Some("turn-1".into());
    assert_eq!(r.verdict(), Verdict::Held(Held::Busy), "a turn the worker saw start");
    // not marked: mail still goes in
    assert_eq!(r.message("m4"), None);
    assert_eq!(r.daemon.asked("thread/queue/add"), 1);
}

#[test]
fn a_sub_agent_at_work_holds_the_move() {
    let mut r = rig();
    let child = thread(&r.cwd, Some(T), "active");
    r.daemon.state.lock().unwrap().threads.push(("01a133f4-0000-7331-9630-9c40ff4b467f".into(), child));
    assert_eq!(r.verdict(), Verdict::Held(Held::ChildRunning));
}

#[test]
fn mail_queued_in_the_old_home_holds_the_move() {
    let mut r = rig();
    r.queue(
        "create table queued_items (id text primary key not null, thread_id text not null, payload_json text not null, \
         queue_order integer not null, created_at_ms integer not null, updated_at_ms integer not null);",
    );
    r.queue(&format!("insert into queued_items values ('q1', '{T}', '{{}}', 0, 0, 0);"));
    assert_eq!(r.verdict(), Verdict::Held(Held::Queued));
    assert_eq!(moving::queued(&r.home, "01a133f4-0000-7331-9630-9c40ff4b467f"), Ok(0), "another thread's mail is not this one's");
    r.queue("delete from queued_items;");
    assert!(matches!(r.verdict(), Verdict::Go { .. }));
}

#[test]
fn a_queue_that_cannot_be_read_holds_the_move() {
    let mut r = rig();
    r.queue("create table other (x);");
    assert_eq!(r.verdict(), Verdict::Held(Held::Unknown));
}

fn t(id: &str, parent: Option<&str>, status: Option<&str>) -> Loaded {
    Loaded { id: id.into(), parent: parent.map(Into::into), status: status.map(Into::into) }
}

#[test]
fn a_grandchild_at_work_holds_the_move_and_a_stranger_does_not() {
    let mut loaded = vec![t("T", None, Some("idle")), t("C", Some("T"), Some("idle")), t("G", Some("C"), Some("active"))];
    assert!(running_descendant(&loaded, "T"));
    loaded[2].status = Some("idle".into());
    assert!(!running_descendant(&loaded, "T"));
    loaded.push(t("X", Some("other"), Some("active")));
    assert!(!running_descendant(&loaded, "T"), "another session's sub-agent");
    loaded.push(t("Y", Some("T"), None));
    assert!(running_descendant(&loaded, "T"), "a child with no status counts as working");
}

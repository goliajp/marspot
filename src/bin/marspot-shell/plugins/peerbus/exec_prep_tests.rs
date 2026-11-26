//! The shell replacing its image, played by this test binary: it runs
//! itself again as the old image, which execs itself as the new one.
//! Real relays, a real exec, the real functions on both sides.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

use marspot::pidtree::{self, ProcId};
use marspot_peer::envelope::Mode;
use marspot_peer::registry::{Agent, Record, Status};

use super::super::gemini_bind::GeminiBinding;
use super::super::inbox::Inbox;
use super::super::worker::{Worker, CHANNEL};

const STAGE: &str = "PB_EXEC_PREP_STAGE";
/// The pids the old image hands the new one: the relay of the worker's
/// inbox, the relay of an inbox left open, and a child that lingers
/// after its stdin ends.
const LEFT: &str = "PB_EXEC_PREP_LEFT";
const DIR: &str = "PB_EXEC_PREP_DIR";
const NAME: &str = "plugins::peerbus::exec_prep::tests::a_new_image_waits_for_what_the_old_one_left_and_still_opens_an_inbox";

#[test]
fn a_new_image_waits_for_what_the_old_one_left_and_still_opens_an_inbox() {
    match std::env::var(STAGE).as_deref() {
        Ok("old") => old_image(),
        Ok("new") => new_image(),
        _ => {
            let dir = std::env::temp_dir().join(format!("pb-exec-{}", marspot_peer::rand::hex(6)));
            std::fs::create_dir_all(&dir).unwrap();
            let out = image("old")
                .env(DIR, &dir)
                .env("MARSPOT_PEER_BIN", marspot_term::test_env::built_binary("marspot-peer"))
                .stdin(Stdio::null())
                .output()
                .unwrap();
            let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            std::fs::remove_dir_all(&dir).unwrap();
            assert!(out.status.success(), "{text}");
            assert!(text.contains("new image: done"), "the new image did not run to the end:\n{text}");
        }
    }
}

fn image(stage: &str) -> Command {
    let mut c = Command::new(std::env::current_exe().unwrap());
    c.args([NAME, "--exact", "--nocapture", "--test-threads=1"]).env(STAGE, stage);
    c
}

fn me() -> i32 {
    std::process::id() as i32
}

fn old_image() {
    let dir = PathBuf::from(std::env::var_os(DIR).unwrap());
    let (tx, rx) = sync_channel(CHANNEL);
    // an inbox the worker holds: the exec's preparation closes it
    let held = Inbox::open(&dir, &dir, 4001, 1, tx.clone()).unwrap();
    let held_relay = held.relay_pid();
    let mut w = Worker::new(tx.clone(), rx, format!("/tmp/pb-exec-owner-{}", marspot_peer::rand::hex(6)));
    w.gemini.insert(1, binding(held, &dir));
    std::thread::spawn(move || w.serve());
    super::connect(Some(tx.clone()));

    // what an exec without the close leaves: a relay, and a child that
    // reads its stdin to the end and then takes a while
    let left_open = Inbox::open(&dir, &dir, 4002, 2, tx).unwrap();
    let relay = left_open.relay_pid();
    std::mem::forget(left_open);
    let lingers = Command::new("/bin/sh").args(["-c", "cat >/dev/null; sleep 0.8"]).stdin(Stdio::piped()).spawn().unwrap();
    let lingers_pid = lingers.id();
    std::mem::forget(lingers);
    let mut new = image("new");
    new.env(LEFT, format!("{held_relay},{relay},{lingers_pid}"));
    // the shell's one exec exit, with the hook the plugin registers at start
    super::register();
    let err = crate::platform::exec_hooks::exec_prepared(&mut new);
    panic!("exec: {err}");
}

fn new_image() {
    let left: Vec<i32> = std::env::var(LEFT).unwrap().split(',').map(|p| p.parse().unwrap()).collect();
    let (held, relay, lingers) = (left[0], left[1], left[2]);
    // the relay is a zombie by now, as the 56 under the shell were
    wait_until(Duration::from_secs(5), || ended(relay) == Some(true));
    // and so would the worker's be, had the preparation not closed it
    wait_until(Duration::from_secs(5), || ended(held) != Some(false));
    assert_eq!(ended(held), None, "the worker's relay was waited for before the exec");
    assert_eq!(ended(lingers), Some(false), "still running when the new image looks");

    crate::inherited::adopt();
    assert_eq!(ended(relay), None, "the zombie was waited for at once");
    assert_eq!(crate::inherited::reap_pending(), 1, "the running one is kept for the next tick");

    std::thread::sleep(Duration::from_millis(crate::REDRAW_INTERVAL_MS));
    assert_eq!(crate::inherited::reap_pending(), 0, "taken on the first tick after it ended");
    assert_eq!(ended(lingers), None);
    let children = pidtree::child_pids(me());
    assert!(children.iter().all(|&p| ended(p) == Some(false)), "no zombie child: {children:?}");

    // an inbox still opens: its relay's start is watched with try_wait
    let dir = PathBuf::from(std::env::var_os(DIR).unwrap());
    let (tx, _rx) = sync_channel(4);
    let inbox = Inbox::open(&dir, &dir, 4003, 3, tx).unwrap();
    assert_eq!(pidtree::child_pids(me()), vec![inbox.relay_pid()]);
    inbox.close();
    assert!(pidtree::child_pids(me()).is_empty());
    println!("new image: done");
}

/// A worker that does not take the event: the close gives up at its
/// deadline instead of holding the exec.
#[test]
fn exec_prep_gives_up_on_a_full_channel() {
    let (tx, _rx) = sync_channel(1);
    tx.try_send(super::super::worker::Event::Stop).unwrap();
    super::connect(Some(tx));
    let t = Instant::now();
    let wait = Duration::from_millis(200);
    super::close_inboxes_within(wait);
    let took = t.elapsed();
    assert!(took >= wait && took < wait + Duration::from_secs(2), "{took:?}");
}

fn binding(inbox: Inbox, dir: &Path) -> GeminiBinding {
    let record = Record {
        pid: inbox.relay_pid(),
        agent: Agent::Gemini,
        agent_pid: 0,
        relay: inbox.relay.display().to_string(),
        session_id: "s".into(),
        cwd: "/".into(),
        started_at_ms: 0,
        proc_start: String::new(),
        version: String::new(),
        name: "n".into(),
        name_since_ms: 0,
        socket: inbox.path.display().to_string(),
        status: Status::Unknown,
        class: Mode::Prompting,
        owner: String::new(),
        spool: None,
    };
    GeminiBinding {
        key: 1,
        gemini: ProcId { pid: 0, start_us: 0 },
        class: Mode::Prompting,
        inbox,
        record,
        rate: Default::default(),
        spool: dir.join("spool"),
        subs: Vec::new(),
        ended: None,
    }
}

/// Whether child `pid` has ended, without waiting for it; None when it
/// is not a child of this process (any more).
fn ended(pid: i32) -> Option<bool> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    if unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, flags) } != 0 {
        return None;
    }
    Some(info.si_pid == pid)
}

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

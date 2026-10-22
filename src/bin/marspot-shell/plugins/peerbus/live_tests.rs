//! The worker against a real Codex: a TUI running in a pty, its real
//! app-server daemon, the real registry, and a reply that Codex itself
//! sends with the `marspot-peer` CLI.  Spends a few model tokens.
//!
//! ```text
//! PEERBUS_LIVE_CODEX_PID=<pid of a codex TUI, idle, alone in its cwd> \
//! PEERBUS_LIVE_PEER_BIN=<path to a built marspot-peer> \
//!   cargo test --bin marspot-shell peerbus::live -- --ignored --nocapture
//! ```
//!
//! The test plays the Claude side: it listens on its own socket in the
//! Claude socket directory, writes a Claude-shaped frame to the Codex
//! inbox, and waits for Codex's answer and the delivery receipt.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{channel, sync_channel};
use std::time::{Duration, Instant};

use marspot::pidtree;
use marspot_peer::envelope::{self, Header, Mode};
use marspot_peer::frame::{self, Inbound, Priority};
use marspot_peer::registry;

use super::worker::{Event, Want, Worker, CHANNEL};

#[test]
#[ignore]
fn live_codex_round_trip() {
    let pid: i32 = std::env::var("PEERBUS_LIVE_CODEX_PID").expect("PEERBUS_LIVE_CODEX_PID").parse().unwrap();
    let peer_bin = std::env::var("PEERBUS_LIVE_PEER_BIN").expect("PEERBUS_LIVE_PEER_BIN");
    let codex = pidtree::proc_id(pid).expect("no such process");
    let argv: Vec<String> = pidtree::proc_cmdline(pid).unwrap().split(' ').map(str::to_string).collect();
    let owner = format!("/tmp/peerbus-live-{}", marspot_peer::rand::hex(8));

    let (tx, rx) = sync_channel(CHANNEL);
    let worker = Worker::new(tx.clone(), rx, owner.clone());
    let handle = std::thread::spawn(move || worker.run());
    tx.send(Event::Want(vec![Want { sid: 1, codex, argv, title_thread: None }])).unwrap();

    // the record appears once the worker has bound the thread
    let rec = wait(Duration::from_secs(30), || {
        registry::default_dirs()
            .iter()
            .flat_map(|d| registry::read_all(d))
            .find(|e| e.ours && e.owner.as_deref() == Some(owner.as_str()) && e.pid == pid)
    })
    .expect("worker never bound the codex thread");
    let inbox = rec.socket.clone().unwrap();
    println!("bound: name={:?} class={:?} inbox={inbox}", rec.name, rec.class);
    assert!(rec.is_live(), "record must read as live to Claude Code");
    assert_eq!(rec.class, Some(Mode::Bypass), "this test expects a codex running without approvals");

    // the Claude side: a reply socket next to the inbox
    let dir = std::path::Path::new(&inbox).parent().unwrap();
    let me = dir.join(format!("{}-{}.sock", std::process::id(), marspot_peer::rand::hex(8)));
    let listener = UnixListener::bind(&me).unwrap();
    let (got_tx, got_rx) = channel();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            for line in BufReader::new(conn).lines().map_while(Result::ok) {
                let _ = got_tx.send(line);
            }
        }
    });
    let from = format!("uds:{}", me.display());
    let token = format!("PEERBUS-LIVE-{}", marspot_peer::rand::hex(6));
    let body = format!(
        "Automated test. Run this shell command exactly once and then stop, with no other action:\n\
         {peer_bin} send {from} {token}"
    );
    let h = Header { from: from.clone(), from_name: Some("peerbus-live-test".into()), from_mode: Some(Mode::Bypass) };
    let line = frame::user_line(&envelope::encode(&h, &body), &from, Priority::Next, &marspot_peer::rand::uuid_v4());
    UnixStream::connect(&inbox).unwrap().write_all(line.as_bytes()).unwrap();

    // an accepted message gets no receipt; any receipt here is a failure
    let mut answer = None;
    let deadline = Instant::now() + Duration::from_secs(120);
    while answer.is_none() && Instant::now() < deadline {
        let Ok(l) = got_rx.recv_timeout(Duration::from_secs(1)) else { continue };
        match frame::parse_line(&l).unwrap() {
            Inbound::Control { action, frame } => panic!("unexpected {action}: {}", frame.to_json()),
            Inbound::User(f) => {
                let env = envelope::parse(&f.content).expect("codex's message is an envelope");
                println!("from codex: {}", f.content);
                answer = Some(env);
            }
            other => println!("other frame: {other:?}"),
        }
    }
    let env = answer.expect("codex never answered");
    assert_eq!(env.from.as_deref(), Some(format!("uds:{inbox}").as_str()), "reply address is the codex inbox");
    assert_eq!(env.from_mode, Some(Mode::Bypass));
    assert!(env.body.contains(&token), "{}", env.body);

    // optional: keep the inbox up so a real Claude session can message it
    if let Some(secs) = std::env::var("PEERBUS_LIVE_HOLD_SECS").ok().and_then(|s| s.parse().ok()) {
        println!("holding {secs}s: message {:?} from a Claude session now", rec.name);
        std::thread::sleep(Duration::from_secs(secs));
    }

    // the pane loses its codex: the binding goes, before any shutdown
    tx.send(Event::Want(Vec::new())).unwrap();
    let gone = wait(Duration::from_secs(15), || {
        let left = registry::default_dirs().iter().flat_map(|d| registry::read_all(d)).any(|e| e.owner.as_deref() == Some(owner.as_str()));
        (!left && !std::path::Path::new(&inbox).exists()).then_some(())
    });
    assert!(gone.is_some(), "unbinding left the record or the inbox behind");

    tx.send(Event::Stop).unwrap();
    handle.join().unwrap();
    assert!(!std::path::Path::new(&inbox).exists(), "inbox socket left behind");
    let left = registry::default_dirs().iter().flat_map(|d| registry::read_all(d)).filter(|e| e.owner.as_deref() == Some(owner.as_str())).count();
    assert_eq!(left, 0, "registry record left behind");
    let _ = std::fs::remove_file(&me);
}

fn wait<T>(within: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let end = Instant::now() + within;
    while Instant::now() < end {
        if let Some(v) = f() {
            return Some(v);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    None
}

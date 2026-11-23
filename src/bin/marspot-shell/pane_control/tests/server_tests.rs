use std::io::{BufRead, BufReader, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

use marspot_peer::control::{ErrorCode, Op};

use super::super::caller::{CallerKey, Peer};
use super::super::config::{Config, DenyList, within};
use super::super::conn::Conn;
use super::super::{dispatch, server};
use super::{ctx, entry, hub_at, temp};

#[test]
fn pane_control_deny_list_matches_by_directory() {
    assert!(within("/w/denied", "/w/denied"));
    assert!(within("/w/denied/sub/deeper", "/w/denied"));
    assert!(!within("/w/deniedX", "/w/denied"));
    assert!(!within("/w", "/w/denied"));
    assert!(within("/anything", "/"));
    let dir = temp("deny");
    std::fs::write(dir.join("control-deny.tsv"), "/w/denied/\n").unwrap();
    let mut list = DenyList::at(dir.join("control-deny.tsv"));
    assert!(list.denies("/w/denied/src"));
    assert!(!list.denies("/w/deniedness"));
}

#[test]
fn pane_control_deny_list_checks_the_process_pane_too() {
    let dir = temp("deny2");
    let (hub, _rx) = hub_at(&dir);
    std::fs::write(dir.join("control-deny.tsv"), "/w/denied\n").unwrap();
    let sessions = || vec![entry(3, "/w/denied/sub"), entry(4, "/w/other")];
    // the process descends from pane 3 but names pane 4's thread
    let peer = Peer { pid: Some(900), exe: String::new(), pane: Some(3) };
    let mut conn = Conn::new(&hub, peer, None);
    let claimed = ctx(CallerKey::Pane(4), sessions(), Config::default());
    assert_eq!(dispatch::run(&mut conn, &claimed, &Op::Audit { limit: 1 }).unwrap_err().code, ErrorCode::Disabled);
    let mut free = Conn::new(&hub, Peer { pane: Some(4), ..Peer::default() }, None);
    assert!(dispatch::run(&mut free, &claimed, &Op::Audit { limit: 1 }).is_ok());
}

#[test]
fn pane_control_accept_errors_are_not_fatal() {
    let emfile = std::io::Error::from_raw_os_error(libc::EMFILE);
    assert!(server::after_accept_error(&emfile).is_some_and(|d| !d.is_zero()));
    assert_eq!(server::after_accept_error(&std::io::Error::from_raw_os_error(libc::ECONNABORTED)), None);
}

/// Every descriptor the process may still open, held.
fn hoard() -> Vec<OwnedFd> {
    let mut held = Vec::new();
    while let Ok(f) = std::fs::File::open("/dev/null") {
        held.push(OwnedFd::from(f));
    }
    held
}

/// The listener runs out of descriptors with a connection pending,
/// then gets some back: it still answers the next caller.  Each test
/// runs in a process of its own, so the lowered limit touches nothing
/// else.
#[test]
fn pane_control_listener_survives_running_out_of_descriptors() {
    let dir = temp("emfile");
    let hub: &'static super::super::Hub = Box::leak(Box::new(hub_at(&dir).0));
    let path = dir.join("c.sock");
    let listener = UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || server::accept_loop(hub, listener));
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: plain getrlimit/setrlimit on this process
    unsafe {
        libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim);
        let low = libc::rlimit { rlim_cur: 256.min(lim.rlim_cur), rlim_max: lim.rlim_max };
        assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &low), 0);
    }
    let mut held = hoard();
    held.pop();
    let starved = UnixStream::connect(&path).unwrap();
    // the listener fails to accept a few times over
    std::thread::sleep(Duration::from_millis(400));
    held.truncate(held.len().saturating_sub(64));
    drop(starved);
    let mut client = UnixStream::connect(&path).unwrap();
    client.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    client.write_all(b"{\"v\":1}\n").unwrap();
    let mut line = String::new();
    BufReader::new(&client).read_line(&mut line).unwrap();
    assert!(line.contains("bad_request"), "{line}");
    drop(held);
    // SAFETY: as above
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) };
}

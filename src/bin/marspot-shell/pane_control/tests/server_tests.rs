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
fn pane_control_accept_errors_are_not_fatal_unless_the_listener_is_broken() {
    use server::AfterAccept;
    let err = std::io::Error::from_raw_os_error;
    assert!(matches!(server::after_accept_error(&err(libc::EMFILE)), AfterAccept::RetryAfter(d) if !d.is_zero()));
    assert_eq!(server::after_accept_error(&err(libc::ECONNABORTED)), AfterAccept::Retry);
    for e in [libc::EBADF, libc::EINVAL, libc::ENOTSOCK] {
        assert_eq!(server::after_accept_error(&err(e)), AfterAccept::Stop, "{e}");
    }
}

#[test]
fn pane_control_accept_loop_ends_on_a_socket_that_is_not_listening() {
    // bound but never listened on: accept fails with EINVAL every time
    let dir = temp("notlisten");
    let path = dir.join("s");
    // SAFETY: plain socket(2)/bind(2) on a zeroed sockaddr_un whose path fits
    let fd = unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (d, s) in addr.sun_path.iter_mut().zip(path.as_os_str().as_encoded_bytes()) {
            *d = *s as libc::c_char;
        }
        assert_eq!(libc::bind(fd, (&raw const addr).cast(), std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t), 0);
        fd
    };
    // SAFETY: fd is the socket opened above and owned by nothing else
    let listener = unsafe { <std::os::unix::net::UnixListener as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    let (hub, _rx) = hub_at(&dir);
    let hub: &'static super::super::Hub = Box::leak(Box::new(hub));
    let t = std::time::Instant::now();
    let done = std::thread::spawn(move || server::accept_loop(hub, listener));
    while !done.is_finished() && t.elapsed() < std::time::Duration::from_secs(5) {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(done.is_finished(), "the loop kept going on a broken listener");
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

//! The listener and the connection threads.  A thread per connection
//! (as the command socket does): a wait holds its connection for as
//! long as the caller asked, and must not hold anyone else up.

use std::io::{BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::Ordering;
use std::time::Duration;

use marspot_peer::control::{ErrorCode, LineRead, Reject, Response, read_line};

use super::super::observe;
use super::Hub;
use super::caller::Peer;
use super::conn::Conn;
use super::dispatch;

pub(super) fn socket_path() -> std::path::PathBuf {
    marspot_term::paths::state_root().join("control.sock")
}

pub(super) fn serve(hub: &'static Hub) -> std::io::Result<()> {
    let path = socket_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // the previous owner is this process's predecessor: an execv, a crash
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    std::thread::Builder::new().name("control".into()).spawn(move || accept_loop(hub, listener))?;
    marspot::lx_info!("shell.control.listening", "pane control socket bound", path = path.display().to_string());
    Ok(())
}

/// How long the listener rests after running out of descriptors: the
/// pending connection stays queued, and accepting again at once would
/// fail the same way.
const OUT_OF_FDS_PAUSE: Duration = Duration::from_millis(100);

#[derive(Debug, PartialEq, Eq)]
pub(super) enum AfterAccept {
    Retry,
    RetryAfter(Duration),
    /// The listener itself is broken: every further accept fails alike.
    Stop,
}

/// What to do after `accept` failed with `e`: carry on, after a pause
/// when the process or the system is out of descriptors; stop when the
/// listening socket is no longer one.
pub(super) fn after_accept_error(e: &std::io::Error) -> AfterAccept {
    match e.raw_os_error() {
        Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM) => AfterAccept::RetryAfter(OUT_OF_FDS_PAUSE),
        Some(libc::EBADF | libc::EINVAL | libc::ENOTSOCK | libc::EOPNOTSUPP) => AfterAccept::Stop,
        _ => AfterAccept::Retry,
    }
}

pub(super) fn accept_loop(hub: &'static Hub, listener: UnixListener) {
    for conn in listener.incoming() {
        let mut stream = match conn {
            Ok(s) => s,
            Err(e) => match after_accept_error(&e) {
                AfterAccept::Stop => {
                    marspot::lx_error!("shell.control.listener_broken", &format!("pane control stops accepting: {e}"));
                    return;
                }
                AfterAccept::RetryAfter(pause) => {
                    marspot::lx_warn!("shell.control.accept_failed", &format!("{e}"));
                    std::thread::sleep(pause);
                    continue;
                }
                AfterAccept::Retry => {
                    marspot::lx_warn!("shell.control.accept_failed", &format!("{e}"));
                    continue;
                }
            },
        };
        if admit(hub) {
            hub.connected();
        } else {
            let r = Response::Err(Reject::new(ErrorCode::TooManyConnections, "too many connections are open"));
            let _ = stream.write_all(format!("{}\n", r.to_json()).as_bytes());
            continue;
        }
        let spawned = std::thread::Builder::new().name("control-conn".into()).spawn(move || {
            serve_conn(hub, stream);
            hub.conns.fetch_sub(1, Ordering::AcqRel);
        });
        if spawned.is_err() {
            hub.conns.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Count a new connection, unless the most allowed are open.
fn admit(hub: &Hub) -> bool {
    let max = hub.config().connections;
    hub.conns.try_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < max).then_some(n + 1)).is_ok()
}

fn serve_conn(hub: &'static Hub, stream: UnixStream) {
    let peer = Peer::of(&stream, &observe::shared().fresh(observe::INTERVAL));
    let Ok(read_half) = stream.try_clone() else { return };
    let mut reader = BufReader::new(read_half);
    let mut writer = stream;
    let mut conn = Conn::new(hub, peer, Some(writer.as_raw_fd()));
    loop {
        let line = match read_line(&mut reader) {
            Ok(LineRead::Line(l)) => l,
            Ok(LineRead::Refused) => {
                let r = Response::Err(Reject::new(ErrorCode::BadRequest, "a request line is at most 256 KiB of UTF-8"));
                let _ = writer.write_all(format!("{}\n", r.to_json()).as_bytes());
                return;
            }
            Ok(LineRead::Eof) | Err(_) => return,
        };
        if line.trim().is_empty() {
            continue;
        }
        let resp = dispatch::handle(&mut conn, &line);
        if writer.write_all(format!("{}\n", resp.to_json()).as_bytes()).is_err() {
            return;
        }
    }
}

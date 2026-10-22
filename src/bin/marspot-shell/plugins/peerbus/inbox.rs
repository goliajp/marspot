//! One Codex session's inbox: a socket in the Claude Code socket
//! directory, speaking the frames a Claude Code session accepts.
//!
//! Each accepted connection gets its own short-lived thread; a sender
//! writes a line and hangs up, and a liveness check connects and hangs
//! up without writing anything — both are normal.  Frames go to the
//! worker over its bounded channel; when that is full the sender is
//! told the message was refused, here, rather than queued without
//! bound.

use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use marspot_term::{lx_info, lx_warn};
use marspot_peer::frame::{self, DropReason, Inbound, Receipt};

use super::worker::Event;
use super::deliver::send_receipt;

/// Connections served at once per inbox.
const MAX_CONNS: usize = 8;
/// A sender gets this long to finish its line.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct Inbox {
    pub path: PathBuf,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl Inbox {
    /// Bind `<dir>/<codex pid>-<8 hex>.sock`.  The random part keeps a
    /// new inbox from colliding with a socket file a dead process left.
    pub fn open(dir: &Path, codex_pid: i32, key: u64, tx: SyncSender<Event>) -> std::io::Result<Inbox> {
        let path = dir.join(format!("{codex_pid}-{}.sock", marspot_peer::rand::hex(8)));
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let stop = Arc::new(AtomicBool::new(false));
        let address = format!("uds:{}", path.display());
        let accept = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name(format!("peerbus-inbox-{codex_pid}"))
                .spawn(move || accept_loop(listener, stop, key, address, tx))?
        };
        lx_info!("peerbus.inbox.open", "inbox bound", path = path.display().to_string());
        Ok(Inbox { path, stop, accept: Some(accept) })
    }

    /// Stop accepting, then remove the socket file.  The wake-up
    /// connection is made before the unlink, while the path still
    /// reaches the listener.
    pub fn close(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.path);
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_file(&self.path);
        lx_info!("peerbus.inbox.close", "inbox closed", path = self.path.display().to_string());
    }
}

fn accept_loop(listener: UnixListener, stop: Arc<AtomicBool>, key: u64, address: String, tx: SyncSender<Event>) {
    let active = Arc::new(AtomicUsize::new(0));
    for conn in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(conn) = conn else { continue };
        if active.load(Ordering::SeqCst) >= MAX_CONNS {
            continue; // dropped: the sender sees the connection close
        }
        // read now: once the sender hangs up the kernel no longer says
        active.fetch_add(1, Ordering::SeqCst);
        let peer = peer_pid(&conn);
        let (active, address, tx) = (active.clone(), address.clone(), tx.clone());
        let spawned = std::thread::Builder::new().name("peerbus-conn".into()).spawn(move || {
            serve(conn, peer, key, &address, &tx);
            active.fetch_sub(1, Ordering::SeqCst);
        });
        if spawned.is_err() {
            lx_warn!("peerbus.inbox.spawn_failed", "could not start a connection thread");
        }
    }
}

fn serve(conn: UnixStream, peer_pid: Option<i32>, key: u64, address: &str, tx: &SyncSender<Event>) {
    let _ = conn.set_read_timeout(Some(READ_TIMEOUT));
    let mut reader = BufReader::new(conn.take(u64::MAX));
    let mut line = String::new();
    loop {
        line.clear();
        reader.get_mut().set_limit(frame::MAX_LINE as u64 + 1);
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(n) if n > frame::MAX_LINE => {
                lx_warn!("peerbus.inbox.line_too_long", "dropping connection", bytes = n);
                return;
            }
            Ok(_) => {}
        }
        match frame::parse_line(line.trim_end()) {
            Ok(Inbound::User(f)) => {
                let reply_to = f.from.clone();
                let msg_id = f.msg_id.clone();
                match tx.try_send(Event::Inbound { key, frame: f, peer_pid }) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        lx_warn!("peerbus.inbox.backlog_full", "refusing a message", key = key);
                        if let Some(to) = reply_to {
                            send_receipt(&to, address, Receipt::Dropped(DropReason::QueueFull), msg_id.as_deref(), "recipient inbox is full");
                        }
                    }
                    Err(TrySendError::Disconnected(_)) => return,
                }
            }
            Ok(_) => {} // auth, control and unknown frames: nothing to do
            Err(e) => lx_warn!("peerbus.inbox.bad_frame", &e),
        }
    }
}

/// The connecting process, as the kernel reports it (LOCAL_PEERPID).
/// For the log only: a pid is provenance, not identity.  None once the
/// sender has closed (ENOTCONN), which a fast sender often has.
fn peer_pid(conn: &UnixStream) -> Option<i32> {
    use std::os::fd::AsRawFd;
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(conn.as_raw_fd(), libc::SOL_LOCAL, libc::LOCAL_PEERPID, (&raw mut pid).cast(), &mut len)
    };
    (rc == 0).then_some(pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::mpsc::sync_channel;

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("pb-inbox-{}", marspot_peer::rand::hex(6)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn frames_reach_the_worker_and_close_removes_the_socket() {
        let d = dir();
        let (tx, rx) = sync_channel(4);
        let inbox = Inbox::open(&d, 4242, 7, tx).unwrap();
        // a liveness check: connect and hang up
        drop(UnixStream::connect(&inbox.path).unwrap());
        let mut c = UnixStream::connect(&inbox.path).unwrap();
        c.write_all(b"{\"type\":\"auth\",\"token\":\"x\"}\n").unwrap();
        c.write_all(frame::user_line("hello", "uds:/nowhere.sock", frame::Priority::Next, "m1").as_bytes()).unwrap();
        let Event::Inbound { key, frame, peer_pid } = rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
            panic!("expected an inbound frame")
        };
        drop(c);
        assert_eq!(key, 7);
        assert_eq!(frame.content, "hello");
        assert_eq!(peer_pid, Some(std::process::id() as i32));
        let path = inbox.path.clone();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        inbox.close();
        assert!(!path.exists());
        std::fs::remove_dir_all(&d).unwrap();
    }
}

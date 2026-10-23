//! One Codex session's inbox: a socket in the Claude Code socket
//! directory, speaking the frames a Claude Code session accepts.
//!
//! The public socket is served by the session's relay process
//! (`marspot_peer::relay`), which Claude Code can match against the
//! registry record; the relay passes every connection to a private
//! socket here, and makes every connection this session opens.
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
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Instant;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use marspot_term::{lx_info, lx_warn};
use marspot_peer::envelope;
use marspot_peer::frame::{self, DropReason, Inbound, Receipt};

use super::worker::Event;
use super::deliver::send_receipt;

/// Connections served at once per inbox.
const MAX_CONNS: usize = 8;
/// A sender gets this long to finish its line.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a new relay gets to put its sockets up.
const RELAY_START: Duration = Duration::from_secs(5);

pub(super) struct Inbox {
    /// The public socket peers write to.
    pub path: PathBuf,
    /// The relay's send socket.
    pub relay: PathBuf,
    upstream: PathBuf,
    relay_proc: Child,
    /// Closing it ends the relay.
    relay_stdin: Option<ChildStdin>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl Inbox {
    /// Bind `<dir>/<codex pid>-<8 hex>.sock` through a new relay, with
    /// its private sockets under `private`.  The random part keeps a new
    /// inbox from colliding with a socket file a dead process left.
    pub fn open(dir: &Path, private: &Path, codex_pid: i32, key: u64, tx: SyncSender<Event>) -> std::io::Result<Inbox> {
        let path = dir.join(format!("{codex_pid}-{}.sock", marspot_peer::rand::hex(8)));
        let (upstream, relay) = marspot_peer::relay::private_paths(private, key);
        let _ = std::fs::remove_file(&upstream);
        let _ = std::fs::remove_file(&relay);
        let listener = UnixListener::bind(&upstream)?;
        std::fs::set_permissions(&upstream, std::fs::Permissions::from_mode(0o600))?;
        let bin = peer_bin().ok_or_else(|| std::io::Error::other("marspot-peer is not installed"))?;
        let mut child = Command::new(bin)
            .arg("relay")
            .args([&path, &upstream, &relay])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let deadline = Instant::now() + RELAY_START;
        while !(path.exists() && relay.exists()) {
            if Instant::now() > deadline || child.try_wait()?.is_some() {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&upstream);
                return Err(std::io::Error::other("the relay did not start"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let address = format!("uds:{}", path.display());
        let accept = {
            let (stop, relay) = (stop.clone(), relay.clone());
            std::thread::Builder::new()
                .name(format!("peerbus-inbox-{codex_pid}"))
                .spawn(move || accept_loop(listener, stop, key, address, relay, tx))?
        };
        lx_info!("peerbus.inbox.open", "inbox bound", path = path.display().to_string(), relay_pid = child.id());
        let relay_stdin = child.stdin.take();
        Ok(Inbox { path, relay, upstream, relay_proc: child, relay_stdin, stop, accept: Some(accept) })
    }

    pub fn relay_pid(&self) -> i32 {
        self.relay_proc.id() as i32
    }

    /// End the relay, which removes the public socket, then stop
    /// accepting here.  The wake-up connection is made before the
    /// unlink, while the path still reaches the listener.
    pub fn close(mut self) {
        drop(self.relay_stdin.take());
        let _ = self.relay_proc.wait();
        let _ = std::fs::remove_file(&self.path);
        self.stop.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.upstream);
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_file(&self.upstream);
        lx_info!("peerbus.inbox.close", "inbox closed", path = self.path.display().to_string());
    }
}

/// The `marspot-peer` binary: `MARSPOT_PEER_BIN`, then `PATH`, then
/// where `install-local` puts it.
pub(super) fn peer_bin() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("MARSPOT_PEER_BIN") {
        return Some(PathBuf::from(p));
    }
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join("marspot-peer"))
        .find(|p| p.is_file());
    let home = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/bin/marspot-peer"));
    on_path.or(home.filter(|p| p.is_file()))
}

fn accept_loop(listener: UnixListener, stop: Arc<AtomicBool>, key: u64, address: String, relay: PathBuf, tx: SyncSender<Event>) {
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
        let (active, address, relay, tx) = (active.clone(), address.clone(), relay.clone(), tx.clone());
        let spawned = std::thread::Builder::new().name("peerbus-conn".into()).spawn(move || {
            serve(conn, peer, key, &address, &relay, &tx);
            active.fetch_sub(1, Ordering::SeqCst);
        });
        if spawned.is_err() {
            lx_warn!("peerbus.inbox.spawn_failed", "could not start a connection thread");
        }
    }
}

fn serve(conn: UnixStream, peer_pid: Option<i32>, key: u64, address: &str, relay: &Path, tx: &SyncSender<Event>) {
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
                            send_receipt(relay, &to, address, Receipt::Dropped(DropReason::QueueFull), msg_id.as_deref(), "recipient inbox is full");
                        }
                    }
                    Err(TrySendError::Disconnected(_)) => return,
                }
            }
            Ok(Inbound::Control { action, frame }) => {
                let from = frame.str_at("from").filter(|a| a.starts_with("uds:") && envelope::is_address(a));
                let ev = match (action.as_str(), from) {
                    ("notify_when_idle", Some(from)) => frame
                        .str_at("msg_id")
                        .map(|id| Event::Subscribe { key, from: from.to_string(), msg_id: id.to_string() }),
                    ("peer_idle_notice", Some(from)) => frame.str_at("state").map(|state| Event::Notice {
                        key,
                        from: from.to_string(),
                        state: state.to_string(),
                        finished_at: frame.get("finished_at").and_then(marspot_peer::json::Value::i64),
                    }),
                    _ => None, // receipts and actions this side does not take
                };
                if let Some(ev) = ev
                    && let Err(TrySendError::Disconnected(_)) = tx.try_send(ev)
                {
                    return;
                }
            }
            Ok(_) => {} // auth and unknown frames: nothing to do
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

    /// The relay built with this test binary: `target/<profile>/marspot-peer`.
    fn relay_bin() -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let bin = exe.parent().and_then(Path::parent).unwrap().join("marspot-peer");
        assert!(bin.is_file(), "{} is missing: build the marspot-peer package first", bin.display());
        bin
    }

    #[test]
    fn frames_reach_the_worker_through_the_relay_and_close_removes_its_sockets() {
        // SAFETY: the only test in this binary that reads the variable
        unsafe { std::env::set_var("MARSPOT_PEER_BIN", relay_bin()) };
        let d = dir();
        let (tx, rx) = sync_channel(4);
        let inbox = Inbox::open(&d, &d, 4242, 7, tx).unwrap();
        // a liveness check: connect and hang up
        drop(UnixStream::connect(&inbox.path).unwrap());
        let mut c = UnixStream::connect(&inbox.path).unwrap();
        // what a Claude Code sender checks against the registry record
        assert_eq!(peer_pid(&c), Some(inbox.relay_pid()), "the relay is the process on the public socket");
        c.write_all(b"{\"type\":\"auth\",\"token\":\"x\"}\n").unwrap();
        c.write_all(frame::user_line("hello", "uds:/nowhere.sock", frame::Priority::Next, "m1").as_bytes()).unwrap();
        c.write_all(frame::notify_when_idle_line("uds:/tmp/cc-socks/1.sock", "bypass", "s1").as_bytes()).unwrap();
        drop(c);
        let Event::Inbound { key, frame, .. } = rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
            panic!("expected an inbound frame")
        };
        assert_eq!(key, 7);
        assert_eq!(frame.content, "hello");
        let Event::Subscribe { from, msg_id, .. } = rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
            panic!("expected a subscription")
        };
        assert_eq!((from.as_str(), msg_id.as_str()), ("uds:/tmp/cc-socks/1.sock", "s1"));
        let (path, relay) = (inbox.path.clone(), inbox.relay.clone());
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        inbox.close();
        assert!(!path.exists() && !relay.exists());
        std::fs::remove_dir_all(&d).unwrap();
    }
}

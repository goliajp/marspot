//! RFC-003 step 2c — per-L3 UDS control socket.
//!
//! When `MARSPOT_L3_OWNS_PTY=1`, L3 binds a Unix-domain socket at
//! `sessions/<id>/sock` and writes a registry `entry.toml` so a
//! freshly-spawned L2 (after a silent-update swap) can discover us by
//! scanning `sessions/` and reattach via the recorded socket path.
//!
//! This commit lands the listener + registry plumbing only:
//! - listener binds, accepts in a dedicated thread
//! - every accepted connection is currently logged and dropped
//!   (frame-level protocol arrives in 2d)
//! - registry entry is written atomically on bind and removed via
//!   `Drop` so a clean L3 exit leaves no stale entries behind
//!
//! Why the entry-write happens here and not inside `LocalSession`:
//! the listener owns the socket path; the registry is the
//! pairing-up artefact that ties a process to a socket. Co-locating
//! both in one type makes RAII cleanup atomic — when the listener
//! drops, the entry and the sock file both go.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use marspot_term::{lx_error, lx_event, lx_info, lx_warn};
use marspot_term::session_registry::{
    cleanup_stale_socket, session_dir, session_socket_path,
    write_session_entry, SessionEntry, PROTO_VERSION,
};
use marspot_term::shell_proto::{
    decode_hello, encode_hello_ack, Frame, MsgType,
};


pub struct SessionListener {
    id: u64,
    // Phase 2d will read this to expose the bound path on the public
    // surface; stays attached now so Drop's debug logs can name it.
    #[allow(dead_code)]
    socket_path: PathBuf,
    accept_thread: Option<JoinHandle<()>>,
    /// RFC-003 §6 Amendment 14 — a duplicate of the bound listener fd
    /// kept here (the original moved into the accept thread via
    /// `try_clone`).  The execv self-update path needs to publish the
    /// listener fd into the new image's env, so it must be reachable
    /// from outside the accept thread; both ends close on Drop.
    keep_listener: Option<UnixListener>,
    /// RFC-003 §6 Amendment 14 — when true, Drop is a no-op:
    /// `prepare_for_execv` flips this so the kept listener fd, the
    /// on-disk sock, and the registry entry all survive the image
    /// swap (new image inherits the fd and adopts the entry as-is).
    suppress_drop: bool,
    /// RFC-004 A.3 — exclusive flock on `sessions/<id>/.lock`, held
    /// for the life of this process.  `None` only on the execv-handoff
    /// path, where the pre-execv image's lock fd is still open in our
    /// (same) process — the flock survives the image swap because the
    /// open file description does.  Dropping this releases the lock;
    /// `prepare_for_execv` clears its CLOEXEC + mem::forgets via the
    /// suppress_drop path so ownership carries across the swap.
    dir_lock: Option<std::fs::File>,
}

impl SessionListener {
    /// Bind the listener, write the registry entry, and spawn the
    /// accept loop. On any failure the partial work is rolled back so
    /// a retry has a clean slate.  `ev_tx` is the L3 main loop's
    /// SessionEvent channel; the accept thread reports each validated
    /// connection back via `SessionEvent::NewClient`.
    pub fn bind(
        id: u64,
        cols: u16,
        rows: u16,
        cwd: &str,
        shm_name: &str,
        shell_child_pid: i32,
        ev_tx: Sender<crate::SessionEvent>,
    ) -> io::Result<Self> {
        let dir = session_dir(id);
        std::fs::create_dir_all(&dir)?;
        // RFC-004 A.3 — take exclusive ownership of the session dir
        // BEFORE touching anything in it.  If another L3 holds the
        // lock, unlinking its socket / overwriting its entry.toml
        // would orphan a live session (347-class corruption); refuse
        // to boot instead.
        let dir_lock = match marspot_term::session_registry::try_lock_session_dir(id) {
            Ok(f) => f,
            Err(e) => {
                lx_error!(
                    "l3.session_dir_locked",
                    &format!("{e} — another L3 owns sessions/{id}; refusing to bind"),
                    session_id = id
                );
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("session dir {id} owned by another live L3"),
                ));
            }
        };
        cleanup_stale_socket(id);

        let socket_path = session_socket_path(id);
        let listener = UnixListener::bind(&socket_path)?;
        // Clone before moving into the accept thread: one fd for the
        // thread to accept() on, one for us to keep so execv handoff
        // can pull the raw fd out without going through the thread.
        let listener_for_thread = listener.try_clone()?;
        // Non-blocking accept would let us share a thread with the
        // main loop, but for now a dedicated accept thread keeps the
        // surface simple. One thread per connection is fine at the
        // L3 scale of <100 clients ever.
        let accept_thread = thread::Builder::new()
            .name(format!("l3-uds-accept-{id}"))
            .spawn(move || accept_loop(id, listener_for_thread, ev_tx))?;

        let entry = SessionEntry {
            id,
            pid: std::process::id() as i32,
            socket: socket_path.clone(),
            cols,
            rows,
            title: String::new(),
            cwd: cwd.to_string(),
            proto_version: PROTO_VERSION,
            created_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            shm_name: shm_name.to_string(),
            shell_child_pid,
        };
        if let Err(e) = write_session_entry(&entry) {
            // Narrow rollback so list / scan won't see a phantom
            // entry: unlink the sock + entry.toml only.  This bind
            // may be a RESURRECTION into a dir that already holds
            // scrollback.bin / bytelog — nuking the whole dir here
            // (the old delete_session rollback) would destroy the
            // very history the resurrect exists to restore
            // (RFC-004 invariant 4).
            cleanup_stale_socket(id);
            let _ = std::fs::remove_file(
                marspot_term::session_registry::session_entry_path(id),
            );
            return Err(e);
        }

        lx_event!(
            "L3_UDS_BOUND",
            "L3 control socket up + registry written",
            session_id = id,
            socket = entry.socket.display(),
            proto_version = entry.proto_version
        );

        Ok(Self {
            id,
            socket_path,
            accept_thread: Some(accept_thread),
            keep_listener: Some(listener),
            suppress_drop: false,
            dir_lock: Some(dir_lock),
        })
    }

    /// RFC-003 §6 Amendment 14 — adopt an inherited UDS listener fd
    /// after an L3 execv handoff.  No `bind`, no `write_session_entry`:
    /// the sock file is still on disk (the pre-execv image's
    /// `suppress_drop` keeps it intact), entry.toml's pid is still
    /// ours (PID is preserved across execv), so we just spawn a fresh
    /// accept loop on the inherited fd and return.
    pub fn from_handoff(
        id: u64,
        raw_fd: RawFd,
        shell_child_pid: i32,
        cols: u16,
        rows: u16,
        ev_tx: Sender<crate::SessionEvent>,
    ) -> io::Result<Self> {
        // SAFETY: caller asserts `raw_fd` is a live UnixListener fd
        // inherited across execv (CLOEXEC was cleared by the pre-execv
        // image's `prepare_for_execv`).
        let listener = unsafe { UnixListener::from_raw_fd(raw_fd) };
        let listener_for_thread = listener.try_clone()?;
        let accept_thread = thread::Builder::new()
            .name(format!("l3-uds-accept-{id}"))
            .spawn(move || accept_loop(id, listener_for_thread, ev_tx))?;
        // Re-write entry.toml so newer schema fields (shell_child_pid,
        // etc) land even when an L3 boots via execv handoff rather
        // than fresh bind.  PID is preserved across execv so the
        // socket / pid stay the same; cwd / title we don't carry
        // through the manifest, so we leave them blank — the cc
        // plugin only reads pid / shell_child_pid / shm_name.
        let entry = SessionEntry {
            id,
            pid: std::process::id() as i32,
            socket: session_socket_path(id),
            cols,
            rows,
            title: String::new(),
            cwd: String::new(),
            proto_version: PROTO_VERSION,
            created_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            shm_name: std::env::var("MARSPOT_SHM_NAME").unwrap_or_default(),
            shell_child_pid,
        };
        let _ = write_session_entry(&entry);
        Ok(Self {
            id,
            socket_path: session_socket_path(id),
            accept_thread: Some(accept_thread),
            keep_listener: Some(listener),
            suppress_drop: false,
            // RFC-004 A.3 — the pre-execv image's lock fd is still
            // open in this same process (CLOEXEC cleared before the
            // swap), so the flock is already ours; nothing to acquire.
            dir_lock: None,
        })
    }

    /// RFC-003 §6 Amendment 14 — prep this listener for the L3 execv
    /// self-update.  Clears `FD_CLOEXEC` on the kept fd so the new
    /// image inherits it, sets `suppress_drop` so the on-disk sock +
    /// registry entry survive when our Drop runs (which it will, the
    /// listener is dropped after `prepare` returns), and returns the
    /// raw fd number to publish in the handoff env vars.
    pub fn prepare_for_execv(&mut self) -> io::Result<RawFd> {
        let listener = self
            .keep_listener
            .as_ref()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Other,
                    "prepare_for_execv: no kept listener",
                )
            })?;
        let fd = listener.as_raw_fd();
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        // RFC-004 A.3 — the session-dir flock must survive the image
        // swap: clear CLOEXEC on the lock fd so the kernel doesn't
        // close it (and release the lock) during execv.  The new
        // image never learns the fd number — it doesn't need to; the
        // flock lives on the open file description until process
        // death.  The fd itself is leaked into the new image via the
        // suppress_drop/mem::forget path in Drop.
        if let Some(lock) = self.dir_lock.as_ref() {
            let lfd = lock.as_raw_fd();
            unsafe {
                let flags = libc::fcntl(lfd, libc::F_GETFD);
                if flags < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::fcntl(lfd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
        }
        self.suppress_drop = true;
        Ok(fd)
    }

    #[allow(dead_code)] // used by Phase 2d test helpers
    pub fn id(&self) -> u64 {
        self.id
    }

    #[allow(dead_code)] // used by Phase 2d test helpers
    pub fn socket_path(&self) -> &std::path::Path {
        &self.socket_path
    }
}

impl Drop for SessionListener {
    fn drop(&mut self) {
        if self.suppress_drop {
            // RFC-003 §6 Amendment 14 — execv handoff in progress.
            // The new image inherits the kept listener fd + the
            // on-disk sock + entry.toml as-is; we MUST NOT unbind or
            // delete anything here.  Leak the UnixListener so its own
            // Drop doesn't close the fd before execv runs, and leak
            // the dir-lock File so the flock survives the swap
            // (RFC-004 A.3).
            if let Some(l) = self.keep_listener.take() {
                std::mem::forget(l);
            }
            if let Some(lock) = self.dir_lock.take() {
                std::mem::forget(lock);
            }
            if let Some(h) = self.accept_thread.take() {
                std::mem::drop(h);
            }
            return;
        }
        // RFC-004 invariant 4: destroying the session dir (history!)
        // is reserved for the USER's explicit pane close — and that
        // path runs in L2 (`close_session` → delete_session).  This
        // Drop used to call delete_session too, but the only way an
        // L3 reaches a non-suppressed Drop is an abnormal unwind
        // (panic in the main loop) — exactly when the user most
        // needs the on-disk state to survive for resurrection.  So:
        // unlink only the socket (a dead L3's sock is useless and
        // blocks nothing thanks to cleanup_stale_socket, but tidy is
        // tidy), keep entry.toml + bytelog + scrollback + state.bin.
        // The next L2 boot resurrects or retires the dir.
        cleanup_stale_socket(self.id);
        lx_warn!(
            "session.listener.abnormal_drop",
            "SessionListener dropped outside execv/clean-exit — \
             session dir preserved for resurrection",
            session_id = self.id
        );
        // The accept thread is parked in accept(); it exits when its
        // accept() fails on the unlinked socket.  The OS reaps it
        // shortly after process exit anyway.
        if let Some(h) = self.accept_thread.take() {
            std::mem::drop(h);
        }
    }
}

/// Accept loop. Each accepted connection runs a Hello/HelloAck
/// handshake; on success the validated stream is shipped to the L3
/// main loop via `SessionEvent::NewClient` so main can spawn the
/// control reader against it (same dispatch logic as the inherited-
/// fd path).
fn accept_loop(id: u64, listener: UnixListener, ev_tx: Sender<crate::SessionEvent>) {
    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                let ev_tx_clone = ev_tx.clone();
                thread::Builder::new()
                    .name(format!("l3-uds-conn-{id}"))
                    .spawn(move || {
                        if let Err(e) = handshake_and_handoff(id, stream, ev_tx_clone) {
                            lx_warn!(
                                "session.listener.conn_failed",
                                &format!("{e}"),
                                session_id = id
                            );
                        }
                    })
                    .ok();
            }
            Err(e) => {
                // Bound socket file got removed (Drop in our owner) or
                // accept(2) genuinely errored — either way the listener
                // can't recover here.
                lx_info!(
                    "session.listener.accept_loop_end",
                    &format!("{e}"),
                    session_id = id
                );
                break;
            }
        }
    }
}

/// Per-connection startup: read one Hello frame, reply HelloAck on
/// version match, then hand the validated stream off to the L3 main
/// loop via `SessionEvent::NewClient`.  Main spawns the control
/// reader against it (KeyEvent / GridResize / Paste / GetSelectionText
/// flow through the existing dispatch).
fn handshake_and_handoff(
    id: u64,
    mut stream: UnixStream,
    ev_tx: Sender<crate::SessionEvent>,
) -> io::Result<()> {
    // Bound the handshake so a stalled client doesn't keep an accept
    // thread alive forever.  Reads after the ack run on a fresh
    // timeout (cleared at end of handshake).
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;

    let frame = match Frame::read_from(&mut stream)? {
        Some(f) => f,
        None => {
            // Peer closed before sending Hello — nothing wrong, just
            // an idle probe (e.g. `nc -U` without input).
            return Ok(());
        }
    };
    if frame.msg_type != MsgType::Hello {
        let _ = Frame::new(
            MsgType::Error,
            format!("expected Hello, got {:?}", frame.msg_type).into_bytes(),
        )
        .write_to(&mut stream);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("first frame was {:?}, not Hello", frame.msg_type),
        ));
    }
    let peer_version = decode_hello(&frame.payload)?;
    if peer_version != PROTO_VERSION {
        let _ = Frame::new(
            MsgType::Error,
            format!(
                "proto mismatch: client={peer_version} server={PROTO_VERSION}"
            )
            .into_bytes(),
        )
        .write_to(&mut stream);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("proto mismatch: client={peer_version} server={PROTO_VERSION}"),
        ));
    }

    Frame::new(MsgType::HelloAck, encode_hello_ack(PROTO_VERSION))
        .write_to(&mut stream)?;

    lx_event!(
        "L3_UDS_HELLO",
        "client handshake OK",
        session_id = id,
        proto_version = PROTO_VERSION
    );

    // Hand the stream off to main. The blocking read timeout we set
    // earlier doesn't carry over to the new reader thread (main's
    // spawn_control_reader does its own blocking read), but reset it
    // here defensively so a stale timeout can't sneak through.
    stream.set_read_timeout(None)?;
    if ev_tx.send(crate::SessionEvent::NewClient(stream)).is_err() {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "main loop gone — discarding NewClient",
        ));
    }
    Ok(())
}

/// Helper for clients (the cargo example client + L2 Phase 3 wire) —
/// open a connection to this session's UDS, perform the Hello
/// handshake, and return the validated stream ready for further
/// frames. Lives here so the wire shape has exactly one definition.
#[allow(dead_code)] // wired up by Phase 3 / the cargo example client
pub fn connect_with_handshake(socket_path: &std::path::Path) -> io::Result<UnixStream> {
    use marspot_term::shell_proto::encode_hello;
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    Frame::new(MsgType::Hello, encode_hello(PROTO_VERSION)).write_to(&mut stream)?;
    let reply = Frame::read_from(&mut stream)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::UnexpectedEof, "server closed before HelloAck")
    })?;
    if reply.msg_type == MsgType::Error {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("server error: {}", String::from_utf8_lossy(&reply.payload)),
        ));
    }
    if reply.msg_type != MsgType::HelloAck {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected HelloAck, got {:?}", reply.msg_type),
        ));
    }
    let v = marspot_term::shell_proto::decode_hello_ack(&reply.payload)?;
    if v != PROTO_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("server proto={v} doesn't match expected {PROTO_VERSION}"),
        ));
    }
    stream.set_read_timeout(None)?;
    Ok(stream)
}

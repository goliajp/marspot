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
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use marspot_term::{lx_event, lx_info, lx_warn};
use marspot_term::session_registry::{
    cleanup_stale_socket, delete_session, session_dir, session_socket_path,
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
}

impl SessionListener {
    /// Bind the listener, write the registry entry, and spawn the
    /// accept loop. On any failure the partial work is rolled back so
    /// a retry has a clean slate.
    pub fn bind(id: u64, cols: u16, rows: u16, cwd: &str) -> io::Result<Self> {
        let dir = session_dir(id);
        std::fs::create_dir_all(&dir)?;
        cleanup_stale_socket(id);

        let socket_path = session_socket_path(id);
        let listener = UnixListener::bind(&socket_path)?;
        // Non-blocking accept would let us share a thread with the
        // main loop, but for now a dedicated accept thread keeps the
        // surface simple (Phase 2d adds per-connection frame
        // handlers; one thread per connection is fine at the L3
        // scale of <100 clients ever).
        let accept_thread = thread::Builder::new()
            .name(format!("l3-uds-accept-{id}"))
            .spawn(move || accept_loop(id, listener))?;

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
        };
        if let Err(e) = write_session_entry(&entry) {
            // Rollback so list / scan won't see a phantom entry.
            let _ = delete_session(id);
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
        })
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
        // Best-effort tear-down. delete_session removes the whole
        // per-session dir (entry.toml + sock + bytelog). A
        // crashed-not-cleanly-dropped L3 leaves all three behind; L2
        // notices the dead pid on next scan and prunes.
        if let Err(e) = delete_session(self.id) {
            lx_warn!(
                "session.listener.cleanup_failed",
                &format!("{e}"),
                session_id = self.id
            );
        }
        // The accept thread is parked in accept(); closing the
        // listener (via delete_session unlinking the sock path)
        // unblocks it with an error and it exits.
        if let Some(h) = self.accept_thread.take() {
            // Don't block tear-down on a thread we can't gracefully
            // signal — it'll exit when its accept() fails on the
            // unlinked socket. The OS reaps it shortly after process
            // exit anyway.
            std::mem::drop(h);
        }
    }
}

/// Accept loop. Each accepted connection runs a Hello/HelloAck
/// handshake (step 2d); on success the connection is currently held
/// open by `handshake_and_park` until the peer closes it. Step 2e
/// hands the validated stream off to a control reader so it carries
/// live KeyEvent/Resize/Paste frames into the main loop.
fn accept_loop(id: u64, listener: UnixListener) {
    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                let pid_label = id;
                thread::Builder::new()
                    .name(format!("l3-uds-conn-{pid_label}"))
                    .spawn(move || {
                        if let Err(e) = handshake_and_park(id, stream) {
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
/// version match, then block on read() until the peer closes the
/// connection.  Step 2e replaces the "park until close" tail with
/// the live control-reader dispatch loop.
fn handshake_and_park(id: u64, mut stream: UnixStream) -> io::Result<()> {
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

    // Park-until-close: Phase 2e will replace this with the control
    // reader / writer hand-off.  For 2d we just need to prove the
    // handshake closes cleanly and the peer can hold the connection.
    stream.set_read_timeout(None)?;
    let mut buf = [0u8; 256];
    use std::io::Read;
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {} // drop bytes silently until 2e
        }
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

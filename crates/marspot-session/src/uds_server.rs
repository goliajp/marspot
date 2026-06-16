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
use std::time::{SystemTime, UNIX_EPOCH};

use marspot_term::{lx_event, lx_info, lx_warn};
use marspot_term::session_registry::{
    cleanup_stale_socket, delete_session, session_dir, session_socket_path,
    write_session_entry, SessionEntry, PROTO_VERSION,
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

/// Accept loop — Phase 2c. Future RPCs land in 2d-2f; for now every
/// incoming connection is recorded for forensics and dropped.
fn accept_loop(id: u64, listener: UnixListener) {
    loop {
        match listener.accept() {
            Ok((stream, addr)) => {
                lx_info!(
                    "session.listener.accept",
                    "accepted connection (drop until 2d)",
                    session_id = id,
                    peer = format!("{addr:?}")
                );
                handle_connection_stub(id, stream);
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

/// Phase 2c stub: log + drop. Phase 2d reads a Hello frame and
/// responds with HelloAck.
fn handle_connection_stub(_id: u64, stream: UnixStream) {
    drop(stream);
}

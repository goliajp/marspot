//! RFC-003: L2-side helpers for the per-session UDS control socket.
//!
//! `marspot-session` (L3) binds a Unix-domain socket at
//! `sessions/<id>/sock` and runs the shell_proto wire on every
//! connection (Hello/HelloAck handshake, then live KeyEvent /
//! GridResize / Paste / GetSelectionText frames).  L2 uses this
//! module to open + handshake a connection, plus a polling helper
//! for the post-spawn race ("wait until L3 has written entry.toml
//! and listed itself on its socket").

use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use crate::session_registry::{read_session_entry, session_entry_path};
use crate::shell_proto::{decode_hello_ack, encode_hello, Frame, MsgType, PROTO_VERSION};

const CONNECT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const ENTRY_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Connect to an L3 control socket and run the Hello / HelloAck
/// handshake.  Returns the validated stream ready for live frames.
///
/// Both sides use `shell_proto::PROTO_VERSION` as the agreed wire
/// version; mismatches surface as `InvalidData`.
pub fn connect_with_handshake(socket_path: &Path) -> io::Result<UnixStream> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(CONNECT_HANDSHAKE_TIMEOUT))?;
    Frame::new(MsgType::Hello, encode_hello(PROTO_VERSION)).write_to(&mut stream)?;
    let reply = Frame::read_from(&mut stream)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "server closed before HelloAck",
        )
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
    let v = decode_hello_ack(&reply.payload)?;
    if v != PROTO_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("server proto={v}, expected {PROTO_VERSION}"),
        ));
    }
    stream.set_read_timeout(None)?;
    Ok(stream)
}

/// Block (with a deadline) until the L3 with session id `id` has
/// written its entry.toml so its socket path becomes known.  Returns
/// the discovered socket path or `TimedOut` if L3 never registered.
pub fn wait_for_entry(id: u64, timeout: Duration) -> io::Result<std::path::PathBuf> {
    let deadline = Instant::now() + timeout;
    loop {
        if session_entry_path(id).exists() {
            // Re-read once the file appears so a partial write doesn't
            // leak past us.  read_session_entry rejects malformed
            // contents — caller retries on InvalidData.
            match read_session_entry(id) {
                Ok(entry) => return Ok(entry.socket),
                Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                    // toml is being written right now; try again in a
                    // few ms.
                }
                Err(e) => return Err(e),
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("L3 session {id} never registered an entry.toml"),
            ));
        }
        thread::sleep(ENTRY_POLL_INTERVAL);
    }
}

/// Spawn-then-attach: wait for the freshly-spawned L3 to register,
/// then connect + handshake.  Used by `spawn_l3` after the child is
/// running so the returned control stream is wire-compatible with
/// the pre-RFC-003 socketpair-fd-3 path (same shell_proto frames go
/// through).
pub fn wait_and_connect(id: u64, timeout: Duration) -> io::Result<UnixStream> {
    let socket_path = wait_for_entry(id, timeout)?;
    connect_with_handshake(&socket_path)
}

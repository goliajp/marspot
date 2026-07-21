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
    connect_with_handshake_by(socket_path, Instant::now() + CONNECT_HANDSHAKE_TIMEOUT)
}

/// Same, but bounded by an absolute `deadline` rather than a fresh
/// per-attempt timeout.
///
/// The read timeout has to be clamped to what is left of the caller's
/// budget, not reset to the full handshake allowance.  Otherwise a peer
/// that accepts the connection and then never answers `HelloAck` blocks
/// for the whole 5 s regardless — so a caller asking for 2 s could still
/// wait 5.  `wait_and_connect`'s promise that "the deadline bounds every
/// retry" was only true of the retry loop, not of the attempt inside it.
pub fn connect_with_handshake_by(
    socket_path: &Path,
    deadline: Instant,
) -> io::Result<UnixStream> {
    let mut stream = UnixStream::connect(socket_path)?;
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .min(CONNECT_HANDSHAKE_TIMEOUT);
    if remaining.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "no budget left for the handshake",
        ));
    }
    stream.set_read_timeout(Some(remaining))?;
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
///
/// F3+3.2 — retry on `ConnectionRefused`.  Reattach path is racy
/// during silent updates: the alive_check (`kill 0`) passes while
/// L3 is mid-execv (binary swap), but `connect(2)` lands in the
/// gap between `execv()` and `from_handoff`'s `accept_loop` spawn
/// — typically <100 ms, but under install-storm load (9 L3s
/// simultaneously self-execv'ing) it stretches to several hundred
/// ms.  A single connect was throwing 1/9 reattaches into prune +
/// fresh-spawn territory each install, which the user saw as
/// "missing pane after update".  Retry with 20 ms backoff until
/// the supplied timeout, then surface the last error.
///
/// RFC-004 C.2 — the retry surface widens beyond ConnectionRefused;
/// every transient boot-window shape retries until the deadline:
///
///   - `NotFound`: a resurrect spawn read a STALE entry.toml (dead
///     dir kept on disk by design) whose sock file is gone — the
///     fresh child rebinds momentarily.  The old fail-fast here made
///     "dead dir without a sock file" unrecoverable, and the old
///     boot deleted the session over it.
///   - `UnexpectedEof` / `InvalidData` / `ConnectionReset`: connect
///     landed inside the execv swap or mid-handshake of a booting
///     child.  A REAL proto mismatch also lands here and now costs
///     the full deadline instead of failing fast — acceptable: it
///     only happens on version skew, and the alternative
///     (fail-fast) turned boot races into lost panes.
///
/// The deadline bounds every retry; the last error surfaces.
pub fn wait_and_connect(id: u64, timeout: Duration) -> io::Result<UnixStream> {
    // ONE deadline for both halves.  The doc above says "the deadline
    // bounds every retry", but the code used to poll for entry.toml for
    // up to `timeout` and then start a *fresh* `timeout` for the connect
    // retries — so the real worst case was 2×, and every caller's
    // budget was silently double what it asked for.  On the reconnect
    // path that turned a nominal 2 s into 4 s per pane, serialised
    // across panes.
    let deadline = Instant::now() + timeout;
    let socket_path = wait_for_entry(id, timeout)?;
    // `wait_for_entry` may have consumed most of the budget; whatever is
    // left is what the connect retries get.
    loop {
        match connect_with_handshake_by(&socket_path, deadline) {
            Ok(s) => return Ok(s),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::NotFound
                        | io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::InvalidData
                        | io::ErrorKind::ConnectionReset
                ) && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;

    /// `wait_and_connect` must spend at most the timeout it was given.
    ///
    /// It used to poll for entry.toml for up to `timeout`, then start a
    /// *second* `timeout` for the connect retries — so a caller asking
    /// for 2 s could block for 4.  On L2's main loop, with one call per
    /// pane, that doubled every freeze.
    ///
    /// Uses a session id that will never register, so the call runs to
    /// its deadline and returns TimedOut.
    /// A peer that accepts the connection and then goes silent must not
    /// outlast the caller's budget.
    ///
    /// The sibling test above never reaches `connect` — its session
    /// never registers — so it cannot see this path.  Here the socket
    /// exists and accepts, but nothing ever answers `HelloAck`; before
    /// the read timeout was clamped to the deadline, this blocked for
    /// the full 5 s `CONNECT_HANDSHAKE_TIMEOUT` no matter what the
    /// caller asked for.
    #[test]
    fn handshake_cannot_outlast_the_caller_budget() {
        let dir = std::env::temp_dir().join("marspot-uds-silent-peer-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("sock");

        // Accept connections, then never write anything back.
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let accepted = std::thread::spawn(move || {
            // Hold the accepted stream so the peer stays connected and
            // silent rather than getting an EOF.
            let _held = listener.accept();
            std::thread::sleep(Duration::from_secs(3));
        });

        let budget = Duration::from_millis(400);
        let t0 = Instant::now();
        let r = connect_with_handshake_by(&sock, Instant::now() + budget);
        let elapsed = t0.elapsed();

        assert!(r.is_err(), "a silent peer must not yield a live stream");
        assert!(
            elapsed < CONNECT_HANDSHAKE_TIMEOUT,
            "took {elapsed:?} — the handshake read timeout is still using \
             its own 5 s allowance instead of the caller's deadline"
        );
        assert!(
            elapsed < budget * 3,
            "took {elapsed:?} against a {budget:?} budget"
        );

        let _ = accepted.join();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn wait_and_connect_honours_a_single_deadline() {
        let dir = std::env::temp_dir().join("marspot-uds-deadline-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", &dir) };

        let timeout = Duration::from_millis(300);
        let t0 = Instant::now();
        let r = wait_and_connect(u64::MAX, timeout);
        let elapsed = t0.elapsed();

        assert!(r.is_err(), "a session that never registers must not connect");
        assert!(
            elapsed < timeout * 2,
            "took {elapsed:?} against a {timeout:?} budget — the two halves \
             are still each starting their own deadline"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}

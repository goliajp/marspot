//! RFC-003 step 2f — end-to-end UDS demo client.
//!
//! Spawns a `marspot-session` with `MARSPOT_L3_OWNS_PTY=1`, waits for
//! the registry entry, connects to its UDS, performs the Hello
//! handshake, sends a GridResize, and confirms the GridReady poke
//! comes back. Exercises every wire we touched in Phase 2 (a–e) in
//! one pass; this is the Phase 2 gate test.
//!
//! Runs entirely in a per-process sandbox under `$TMPDIR`; never
//! touches production state.
//!
//! Usage:
//!   cargo run --release -p marspot-session --example l3_uds_handshake_probe
//!
//! Exit 0 = handshake + RPC round-trip OK; non-zero on any failure.

use std::env;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use marspot_term::session_registry::{read_session_entry, session_socket_path};
use marspot_term::shell_proto::{encode_grid_resize, Frame, MsgType, PROTO_VERSION};

fn workspace_root() -> PathBuf {
    // examples/ are compiled into target/release/examples; walk back to
    // the workspace root from the manifest dir.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("workspace root")
}

fn target_release_marspot_session() -> PathBuf {
    workspace_root().join("target/release/marspot-session")
}

fn die(msg: &str) -> ! {
    eprintln!("FAIL: {msg}");
    std::process::exit(1)
}

fn main() {
    let bin = target_release_marspot_session();
    if !bin.exists() {
        die(&format!("marspot-session release binary missing at {bin:?} — run `cargo build --release -p marspot-session` first"));
    }

    let pid = std::process::id();
    let sandbox = env::temp_dir().join(format!("rfc003-handshake-probe-{pid}"));
    let _ = std::fs::remove_dir_all(&sandbox);
    std::fs::create_dir_all(sandbox.join("logs")).expect("mkdir sandbox/logs");
    std::fs::create_dir_all(sandbox.join("sessions")).expect("mkdir sandbox/sessions");

    let session_id: u64 = 42;

    let child = Command::new(&bin)
        .env("MARSPOT_STATE_DIR", &sandbox)
        .env("MARSPOT_L3_OWNS_PTY", "1")
        .env("MARSPOT_SESSION_ID", session_id.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn marspot-session");

    // Clean up the child process on any path out of main, so a failed
    // probe doesn't leave orphans.
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _guard = ChildGuard(child);
    // The guard owns `child` — re-borrow for waitpid timing.

    // Wait up to 5 s for the registry entry to land.
    let sock_path = {
        env::set_var("MARSPOT_STATE_DIR", &sandbox);
        session_socket_path(session_id)
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(entry) = read_session_entry(session_id) {
            assert_eq!(entry.id, session_id);
            assert_eq!(entry.proto_version, PROTO_VERSION);
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if !sock_path.exists() {
        die(&format!("registry entry / sock never appeared at {sock_path:?}"));
    }
    println!("OK registry entry id={session_id} proto={PROTO_VERSION}");

    // Connect + Hello.
    let mut stream = match marspot_session_test_helpers::connect_with_handshake(&sock_path) {
        Ok(s) => s,
        Err(e) => die(&format!("connect_with_handshake: {e}")),
    };
    println!("OK Hello/HelloAck");

    // Resize 100 × 30. Then expect a GridReady back from main loop.
    Frame::new(MsgType::GridResize, encode_grid_resize(100, 30))
        .write_to(&mut stream)
        .expect("write GridResize");

    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("set timeout");
    let f = match Frame::read_from(&mut stream) {
        Ok(Some(f)) => f,
        Ok(None) => die("server EOF before GridReady"),
        Err(e) => die(&format!("read GridReady: {e}")),
    };
    if f.msg_type != MsgType::GridReady {
        die(&format!("expected GridReady, got {:?}", f.msg_type));
    }
    println!("OK GridResize → GridReady round-trip");

    drop(stream);
    println!("PASS l3_uds_handshake_probe");
}

mod marspot_session_test_helpers {
    use std::io;
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::time::Duration;

    use marspot_term::shell_proto::{
        decode_hello_ack, encode_hello, Frame, MsgType, PROTO_VERSION,
    };

    pub fn connect_with_handshake(socket_path: &Path) -> io::Result<UnixStream> {
        let mut stream = UnixStream::connect(socket_path)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        Frame::new(MsgType::Hello, encode_hello(PROTO_VERSION)).write_to(&mut stream)?;
        let reply = Frame::read_from(&mut stream)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "server closed before HelloAck",
            )
        })?;
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
}

//! Production-path bulk-cat throughput: shell → core → **L3**.
//!
//! `bin/measure.sh` drives `mcli`, where the parser and the renderer
//! share one process and one thread.  That is not the architecture the
//! product ships: since 2026-06-13 every pane is its own
//! `marspot-session` (L3) that owns the pty and publishes into shm,
//! and the number a user actually experiences comes from that path.
//!
//! This probe measures it headlessly: spawn one L3 in a sandbox, hand
//! it a trial script through `MARSPOT_SHELL`, and read the elapsed time
//! the script's own `/usr/bin/time -p` recorded.
//!
//! The trial ends with a DSR round-trip (`CSI 6 n`) for the same reason
//! every other live measurement here does: `cat` returning proves the
//! bytes reached the pty, not that the terminal consumed them.  L3 owns
//! the pty and answers capability queries itself, so the reply is that
//! session certifying it drained the corpus.
//!
//! This replaces a probe of the same name that was deleted with the L4
//! shelld in RFC-003 Phase 6g (888bf1b, 2026-06-17).  `bin/measure-l3.sh`
//! and the `--full` gate kept calling it; with the source gone they ran
//! a stale binary that failed every trial, and
//! `bench/results/l3-throughput.json` sat frozen at its last success
//! (2026-06-14) looking exactly like fresh data.
//!
//! Usage (the shape `bin/measure-l3.sh` expects):
//!   l3_throughput <session-bin> <scenario-path> <repeats>
//! Prints elapsed nanoseconds on stdout, or nothing on failure.

use std::env;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use marspot_term::session_registry::{read_session_entry, session_socket_path};
use marspot_term::shell_proto::{encode_grid_resize, Frame, MsgType};

fn die(msg: &str) -> ! {
    eprintln!("FAIL: {msg}");
    std::process::exit(1)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 4 {
        die("usage: l3_throughput <session-bin> <scenario-path> <repeats>");
    }
    let session_bin = PathBuf::from(&args[1]);
    let scenario = match std::fs::canonicalize(&args[2]) {
        Ok(p) => p,
        Err(e) => die(&format!("resolve {}: {e}", args[2])),
    };
    let repeats: usize = args[3].parse().unwrap_or(1);
    if !session_bin.exists() {
        die(&format!("marspot-session missing at {session_bin:?}"));
    }

    let pid = std::process::id();
    let sandbox = env::temp_dir().join(format!("l3-throughput-{pid}"));
    let _ = std::fs::remove_dir_all(&sandbox);
    std::fs::create_dir_all(sandbox.join("logs")).expect("mkdir sandbox/logs");
    std::fs::create_dir_all(sandbox.join("sessions")).expect("mkdir sandbox/sessions");

    // The trial script, byte-identical in shape to the one bin/_lib.sh
    // writes for every other terminal: cat the corpus `repeats` times,
    // then make the terminal certify it consumed the lot.
    let marker = sandbox.join("marker.txt");
    let script = sandbox.join("trial.sh");
    {
        let mut f = std::fs::File::create(&script).expect("create trial script");
        let arg = scenario.to_string_lossy();
        let cats = std::iter::repeat(arg.as_ref())
            .take(repeats.max(1))
            .collect::<Vec<_>>()
            .join(" ");
        write!(
            f,
            "#!/bin/bash\nexec 2> {marker}\n/usr/bin/time -p /bin/bash -c '\n  /bin/cat {cats}\n  stty raw -echo 2>/dev/null\n  printf \"\\033[6n\" > /dev/tty\n  IFS=\"[;\" read -t 5 -srd R _ _row _col < /dev/tty\n  stty sane 2>/dev/null\n  printf \"cpr=%s,%s\\n\" \"$_row\" \"$_col\" >&2\n'\n",
            marker = marker.display(),
            cats = cats,
        )
        .expect("write trial script");
    }
    let mut perms = std::fs::metadata(&script).expect("stat script").permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
    }
    std::fs::set_permissions(&script, perms).expect("chmod script");

    let session_id: u64 = 1;
    let child = Command::new(&session_bin)
        .env("MARSPOT_STATE_DIR", &sandbox)
        .env("MARSPOT_L3_OWNS_PTY", "1")
        .env("MARSPOT_SESSION_ID", session_id.to_string())
        // `SHELL`, not `MARSPOT_SHELL`: an L3 spawns `$SHELL` directly
        // (local_session.rs) and never consults the MARSPOT_SHELL
        // override that `marspot_term::session::Session` — the path
        // mcli and the L2 in-process backend take — honours.  Pointing
        // MARSPOT_SHELL at the trial script leaves the session sitting
        // in an interactive zsh waiting for input, which looks exactly
        // like a slow terminal until the probe times out.
        .env("SHELL", &script)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| die(&format!("spawn marspot-session: {e}")));
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _guard = ChildGuard(child);

    // SAFETY: single-threaded at this point; the registry helpers read
    // this to locate the sandbox.
    unsafe { env::set_var("MARSPOT_STATE_DIR", &sandbox) };
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if read_session_entry(session_id).is_ok() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    if read_session_entry(session_id).is_err() {
        die("L3 never registered");
    }

    // An L3 does not open its pty on registration — it waits to be told
    // how big the grid is, because spawning a shell into a
    // zero-dimensioned terminal would make every app that queries the
    // window size misbehave.  So the probe has to do what a core does:
    // connect, Hello, then GridResize.  Without this the session sits
    // idle forever and the trial "produces no timing".
    let sock = session_socket_path(session_id);
    let mut stream = match handshake::connect(&sock) {
        Ok(s) => s,
        Err(e) => die(&format!("connect_with_handshake: {e}")),
    };
    if let Err(e) = Frame::new(MsgType::GridResize, encode_grid_resize(122, 39))
        .write_to(&mut stream)
    {
        die(&format!("GridResize: {e}"));
    }

    // Keep reading the control socket, or this probe measures its own
    // backpressure.  An L3 pushes frames at its core (GridReady and
    // friends); a client that connects and then never reads fills the
    // socket buffer and every subsequent write from the session blocks
    // — on the same thread that parses.  The first version of this
    // probe did exactly that and reported the production path at 26
    // MB/s with 75 % of the session's samples in `write`, which reads
    // like a damning finding about L3 and was a bug in the harness.
    // A real core drains this socket; so does this.
    match stream.try_clone() {
        Ok(mut rx) => {
            thread::spawn(move || {
                while let Ok(Some(_frame)) = Frame::read_from(&mut rx) {}
            });
        }
        Err(e) => die(&format!("try_clone control stream: {e}")),
    }

    // The session runs the script once its pty is up; wait for the
    // marker to carry a complete `time` record.
    let deadline = Instant::now() + Duration::from_secs(300);
    let mut text = String::new();
    while Instant::now() < deadline {
        if let Ok(s) = std::fs::read_to_string(&marker) {
            if s.contains("real") {
                text = s;
                break;
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    if text.is_empty() {
        die("trial produced no timing");
    }
    if !text.lines().any(|l| l.starts_with("cpr=") && l[4..5].chars().next().is_some_and(|c| c.is_ascii_digit())) {
        die("no DSR reply — the session never confirmed it consumed the corpus");
    }
    let secs = text
        .lines()
        .find_map(|l| l.strip_prefix("real"))
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or_else(|| die("could not parse `real` from the trial marker"));
    println!("{}", (secs * 1e9) as u64);
    let _ = std::fs::remove_dir_all(&sandbox);
}

/// The Hello handshake, inlined because `marspot-session` ships as a
/// binary with no lib target for an example to link against — the same
/// reason `l3_uds_handshake_probe` carries its own copy.
mod handshake {
    use std::io;
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::time::Duration;

    use marspot_term::shell_proto::{
        decode_hello_ack, encode_hello, Frame, MsgType, PROTO_VERSION,
    };

    pub fn connect(socket_path: &Path) -> io::Result<UnixStream> {
        let mut stream = UnixStream::connect(socket_path)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        Frame::new(MsgType::Hello, encode_hello(PROTO_VERSION)).write_to(&mut stream)?;
        let reply = Frame::read_from(&mut stream)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "server closed before HelloAck")
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

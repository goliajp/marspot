//! The built `marspot-peer` against a control socket that listens but
//! never answers: `--help`, `-h` and an unknown option end the process
//! before it connects, and words after `--` reach the request.

use std::io::{BufRead, BufReader, ErrorKind};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Sandbox {
    dir: PathBuf,
    listener: UnixListener,
}

impl Sandbox {
    fn new() -> Sandbox {
        // short: a unix socket path is limited to 104 bytes
        let dir = PathBuf::from(format!("/tmp/mpp-help-{}", marspot_peer::rand::hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let listener = UnixListener::bind(dir.join("control.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        Sandbox { dir, listener }
    }

    fn run(&self, line: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_marspot-peer"))
            .args(line.split_whitespace())
            .env("MARSPOT_STATE_DIR", &self.dir)
            .env_remove("CODEX_THREAD_ID")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn connected(&self) -> bool {
        match self.listener.accept() {
            Ok(_) => true,
            Err(e) if e.kind() == ErrorKind::WouldBlock => false,
            Err(e) => panic!("accept: {e}"),
        }
    }

    /// Run `line`, take its one request and hang up; the request line.
    fn request_of(&self, line: &str) -> (String, Output) {
        self.listener.set_nonblocking(false).unwrap();
        let listener = self.listener.try_clone().unwrap();
        let taker = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut req = String::new();
            BufReader::new(s).read_line(&mut req).unwrap();
            req
        });
        let out = self.run(line);
        let req = taker.join().unwrap();
        self.listener.set_nonblocking(true).unwrap();
        (req, out)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const LINES: &[(&str, u8)] = &[
    ("", 1),
    ("list", 1),
    ("whoami", 1),
    ("send", 1),
    ("relay a b", 1),
    ("claude-hook", 1),
    ("gemini-hook", 1),
    ("kimi-hook", 1),
    ("pane", 2),
    ("pane list", 2),
    ("pane open", 2),
    ("pane send spg", 2),
    ("pane keys spg", 2),
    ("pane run spg", 2),
    ("pane read spg", 2),
    ("pane resize spg", 2),
    ("pane place spg", 2),
    ("pane close spg", 2),
    ("pane audit", 2),
    ("helper", 2),
    ("helper start --agent codex", 2),
    ("helper list", 2),
    ("helper stop", 2),
    ("procs", 2),
    ("procs stop", 2),
];

#[test]
fn cli_help_the_binary_prints_usage_and_never_connects() {
    let sb = Sandbox::new();
    for (line, usage_code) in LINES {
        // after the target these take text: a help word there is a mistake
        let help_at = if matches!(*line, "pane send spg" | "pane run spg") { line.trim_end_matches(" spg") } else { line };
        for help in ["--help", "-h"] {
            let out = sb.run(&format!("{help_at} {help}"));
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert_eq!(out.status.code(), Some(0), "{line} {help}");
            assert!(stdout.starts_with("usage:"), "{line} {help}: {stdout:?}");
            assert!(out.stderr.is_empty(), "{line} {help}");
        }
        let out = sb.run(&format!("{line} --bogus"));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(i32::from(*usage_code)), "{line} --bogus: {stderr}");
        assert!(stderr.contains("--bogus") && stderr.contains("usage:"), "{line} --bogus: {stderr:?}");
        assert!(out.stdout.is_empty(), "{line} --bogus");
        assert!(!sb.connected(), "{line}: a connection was made");
    }
}

#[test]
fn cli_help_words_after_double_dash_reach_the_request() {
    let sb = Sandbox::new();
    for (line, want) in [
        ("pane open -- cmd --help", r#""command":"cmd --help""#),
        ("pane send spg -- -x", r#""text":"-x""#),
        ("pane send spg git log -5", r#""text":"git log -5""#),
        ("pane run spg ls -h", r#""command":"ls -h""#),
        ("helper start --agent codex -- --help me", r#""task":"--help me""#),
        ("helper start --agent codex fix --help", r#""task":"fix --help""#),
    ] {
        let (req, out) = sb.request_of(line);
        assert!(req.contains(want), "{line}: {req}");
        // hung up before an answer: the outcome is unknown
        assert_eq!(out.status.code(), Some(1), "{line}: {}", String::from_utf8_lossy(&out.stderr));
    }
}

/// A config that passes `mcp` a word it does not know still gets a
/// server: one warning, then the answer to `initialize`.
#[test]
fn cli_help_mcp_with_an_unknown_word_still_serves() {
    let sb = Sandbox::new();
    let out = sb.run("mcp --help");
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("usage:"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_marspot-peer"))
        .args(["mcp", "-v"])
        .env("MARSPOT_STATE_DIR", &sb.dir)
        .env_remove("CODEX_THREAD_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let init = r#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{"clientInfo":{"name":"codex-mcp-client"}}}"#;
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), format!("{init}\n").as_bytes()).unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(stdout.contains(r#""id":7"#) && stdout.contains("serverInfo"), "{stdout}");
    assert_eq!(stderr.lines().filter(|l| l.contains("-v")).count(), 1, "{stderr}");
}

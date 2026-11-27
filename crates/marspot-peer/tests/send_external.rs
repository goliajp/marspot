//! The built `marspot-peer send --from` against a sandbox registry: a
//! Claude record and a Codex record, each standing on a `sleep` process,
//! with a listening socket each and a pane table naming both.  Nothing
//! outside the sandbox's own `HOME` is read or written.

use std::io::{BufRead, BufReader};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use marspot_peer::envelope::{self, Mode};
use marspot_peer::frame::{self, Inbound};
use marspot_peer::registry::{self, Agent, Record, Status};
use marspot_peer::{panes, proc};

struct Sandbox {
    dir: PathBuf,
    /// The stand-ins the records name, killed by their own pids.
    children: Vec<Child>,
    lines: Vec<(String, mpsc::Receiver<String>)>,
}

impl Sandbox {
    fn new() -> Sandbox {
        // short: a unix socket path is limited to 104 bytes
        let dir = PathBuf::from(format!("/tmp/mpp-ext-{}", marspot_peer::rand::hex(6)));
        let sessions = dir.join(".claude/sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let mut sb = Sandbox { dir, children: Vec::new(), lines: Vec::new() };
        let claude = sb.stand_in();
        let codex = sb.stand_in();
        let (c_sock, x_sock) = (sb.listen("c.sock"), sb.listen("x.sock"));
        let claude_rec = format!(
            r#"{{"pid":{claude},"sessionId":"s1","procStart":"{}","entrypoint":"cli","name":"marspot-6a","messagingSocketPath":"{c_sock}","status":"idle"}}"#,
            proc::lstart(claude).unwrap()
        );
        std::fs::write(registry::record_path(&sessions, claude), claude_rec).unwrap();
        let rec = Record {
            pid: codex,
            agent: Agent::Codex,
            agent_pid: codex,
            relay: sb.dir.join("r.sock").display().to_string(),
            session_id: "t1".into(),
            cwd: "/".into(),
            started_at_ms: 0,
            proc_start: proc::lstart(codex).unwrap(),
            version: "0".into(),
            name: "marspot#9906".into(),
            name_since_ms: 0,
            socket: x_sock.clone(),
            status: Status::Idle,
            class: Mode::Bypass,
            owner: "sandbox".into(),
            spool: None,
        };
        registry::write(&sessions, &rec, 0).unwrap();
        let table = format!(
            r#"{{"owner":"sandbox","pid":{claude},"procStart":"{}","panes":[{{"name":"marspot#1024","pid":{claude},"socket":"{c_sock}","code":1024,"base":"marspot","agent":"claude"}},{{"name":"marspot#9906","pid":{codex},"socket":"{x_sock}","code":9906,"base":"marspot","agent":"codex"}}]}}"#,
            proc::lstart(claude).unwrap()
        );
        std::fs::write(panes::path_for(&sessions, "sandbox"), table).unwrap();
        sb
    }

    fn stand_in(&mut self) -> i32 {
        let child = Command::new("sleep").arg("120").stdin(Stdio::null()).spawn().unwrap();
        let pid = child.id() as i32;
        self.children.push(child);
        pid
    }

    /// A socket that hands back the first line written to it.
    fn listen(&mut self, name: &str) -> String {
        let path = self.dir.join(name);
        let listener = UnixListener::bind(&path).unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let Ok((s, _)) = listener.accept() else { return };
            let mut line = String::new();
            if BufReader::new(s).read_line(&mut line).is_ok() {
                let _ = tx.send(line);
            }
        });
        let path = path.display().to_string();
        self.lines.push((path.clone(), rx));
        path
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_marspot-peer"));
        cmd.args(args).env("HOME", &self.dir).env_remove("CLAUDE_CONFIG_DIR");
        for k in ["CODEX_THREAD_ID", "CLAUDECODE", "MARSPOT_STATE_DIR"] {
            cmd.env_remove(k);
        }
        cmd.envs(env.iter().copied()).stdin(Stdio::null()).output().unwrap()
    }

    /// The frame that reached `socket`, or None within a moment.
    fn got(&self, socket_name: &str) -> Option<String> {
        let path = self.dir.join(socket_name).display().to_string();
        let (_, rx) = self.lines.iter().find(|(p, _)| *p == path).unwrap();
        rx.recv_timeout(Duration::from_millis(500)).ok()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        for c in &mut self.children {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn header_of(line: &str) -> envelope::Parsed {
    match frame::parse_line(line.trim_end()).unwrap() {
        Inbound::User(f) => envelope::parse(&f.content).unwrap(),
        other => panic!("{other:?}"),
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn external_send_reaches_claude_and_codex_panes_as_external() {
    let sb = Sandbox::new();
    for (to, sock) in [("marspot#1024", "c.sock"), ("codex:marspot", "x.sock")] {
        let out = sb.run(&["send", "--from", "devops-cron", to, "runner", "report"], &[]);
        assert!(out.status.success(), "{to}: {}", stderr(&out));
        assert!(String::from_utf8_lossy(&out.stdout).starts_with("sent "), "{to}");
        let h = header_of(&sb.got(sock).unwrap_or_else(|| panic!("{to}: nothing arrived")));
        assert_eq!(h.from_name.as_deref(), Some("external:devops-cron"), "{to}");
        assert_eq!(h.from_mode, Some(Mode::Bypass), "{to}");
        // the reply address sits in this user's own 0700 directory, never in /tmp
        let reply = h.from.as_deref().and_then(|f| f.strip_prefix("uds:")).unwrap_or_else(|| panic!("{to}: no address"));
        let dir = std::path::Path::new(reply).parent().unwrap();
        assert!(reply.contains("/marspot-external-") && reply.ends_with(".sock") && reply.len() < 104 && !reply.starts_with("/tmp/"), "{to}: {reply}");
        let meta = std::fs::metadata(dir).unwrap();
        assert_eq!((meta.mode() & 0o777, meta.uid()), (0o700, unsafe { libc::getuid() }), "{to}: {}", dir.display());
        assert_eq!(h.body, "runner report", "{to}");
    }
}

#[test]
fn external_send_under_a_pane_or_session_name_is_refused() {
    let sb = Sandbox::new();
    for name in ["marspot", "marspot-6a"] {
        let out = sb.run(&["send", "--from", name, "marspot#1024", "hi"], &[]);
        assert_eq!(out.status.code(), Some(3), "{name}");
        assert!(stderr(&out).contains("must not pass for a pane or session"), "{name}: {}", stderr(&out));
    }
    // a pane's base#code form never gets that far: '#' is not a sender name's character
    for name in ["marspot#1024", "marspot#9906"] {
        let out = sb.run(&["send", "--from", name, "marspot#1024", "hi"], &[]);
        assert_eq!(out.status.code(), Some(3), "{name}");
        assert!(stderr(&out).contains("letters, digits"), "{name}: {}", stderr(&out));
    }
    let out = sb.run(&["send", "--from", "cron\u{1b}[2J", "marspot#1024", "hi"], &[]);
    assert_eq!(out.status.code(), Some(3));
    assert!(stderr(&out).contains("letters, digits"), "{}", stderr(&out));
    assert_eq!(sb.got("c.sock"), None, "nothing was sent");
}

#[test]
fn external_send_inside_a_session_is_refused() {
    let sb = Sandbox::new();
    for (k, v) in [("CODEX_THREAD_ID", "t1"), ("CLAUDECODE", "1")] {
        let out = sb.run(&["send", "--from", "devops-cron", "marspot#1024", "hi"], &[(k, v)]);
        assert_eq!(out.status.code(), Some(3), "{k}");
        let err = stderr(&out);
        assert!(err.contains("--from is only for senders outside every agent session") && err.contains(k), "{k}: {err}");
    }
    assert_eq!(sb.got("c.sock"), None, "nothing was sent");
    // without --from and outside every session, the error says how
    let out = sb.run(&["send", "marspot#1024", "hi"], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("send with --from NAME"), "{}", stderr(&out));
}

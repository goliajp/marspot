//! A typed command line runs alone, on a real shell.
//!
//! The 2026-11-15 incident, reproduced: mouse reports typed into the
//! shell's line while claude exited, then the resume line typed after
//! them.  The shell has `3` aliased to `cd -3` and a directory stack,
//! as oh-my-zsh sets it up; the residue's `;`-separated `3` would move
//! the shell.

use super::*;
use crate::plugins::pty_op::{OpEnv, OpRunner, PtyIo};
use crate::plugins::{LogLevel, PaneSession, PaneSessionHost};
use marspot::pidtree::{self, ProcId, SignalError};
use marspot_term::pty::{Pty, PtyConfig, TerminalSize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// A shell on a real pty, and everything it has printed.
struct Shell {
    pty: Arc<Pty>,
    out: Arc<Mutex<Vec<u8>>>,
}

impl Shell {
    fn spawn(program: &str, args: &[&str], cwd: &std::path::Path) -> Self {
        let pty = Arc::new(
            Pty::spawn(PtyConfig {
                program: program.into(),
                args: args.iter().map(|a| a.to_string()).collect(),
                size: TerminalSize {
                    cols: 200,
                    rows: 50,
                    pixel_width: 0,
                    pixel_height: 0,
                },
                argv0: None,
                cwd: Some(cwd.display().to_string()),
                env_remove_prefixes: Vec::new(),
                env_set: Vec::new(),
            })
            .expect("spawn the shell"),
        );
        let out = Arc::new(Mutex::new(Vec::new()));
        // a copy of the master for the reader, so the pty itself has one
        // owner and its drop hangs the shell up
        let fd = unsafe { libc::dup(pty.raw_master()) };
        assert!(fd >= 0, "dup the master");
        let o = Arc::clone(&out);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
                if n <= 0 {
                    break;
                }
                o.lock().unwrap().extend_from_slice(&buf[..n as usize]);
            }
            unsafe { libc::close(fd) };
        });
        Self { pty, out }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.out.lock().unwrap()).into_owned()
    }

    fn type_in(&self, bytes: &[u8]) {
        self.pty.write_shared(bytes).expect("write to the pty");
    }

    /// Wait until the output, from `from` on, contains `needle`.
    fn wait_for(&self, from: usize, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if self.text().get(from..).is_some_and(|t| t.contains(needle)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("never saw {needle:?}; output:\n{}", self.text());
    }

    fn len(&self) -> usize {
        self.out.lock().unwrap().len()
    }
}

struct Io(Arc<Pty>);
impl PtyIo for Io {
    fn send(&self, _sid: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.0.write_shared(bytes).map(|_| ())
    }
    fn hold(&self, _sid: u64, _on: bool) -> std::io::Result<()> {
        Ok(())
    }
    fn paste(&self, _sid: u64, text: &str) -> std::io::Result<()> {
        self.0.write_shared(text.as_bytes()).map(|_| ())
    }
    fn reset_mouse_reporting(&self, _sid: u64) -> std::io::Result<()> {
        Ok(())
    }
}

/// The op's view of the real shell: its clock, its output, and who
/// holds its terminal, asked of the kernel as in production.
struct Env {
    io: Arc<dyn PtyIo>,
    out: Arc<Mutex<Vec<u8>>>,
}
impl OpEnv for Env {
    fn io(&self) -> &Arc<dyn PtyIo> {
        &self.io
    }
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
    fn signal(&self, _id: ProcId, _sig: i32) -> Result<(), SignalError> {
        Err(SignalError::Gone)
    }
    fn pid_alive(&self, id: ProcId) -> bool {
        !matches!(pidtree::signal(id, 0), Err(SignalError::Gone))
    }
    fn find_descendant(&self, _under: i32, _pred: fn(&pidtree::ProcRow) -> bool) -> Option<ProcId> {
        None
    }
    fn find_in_front(&self, under: i32, pred: fn(&pidtree::ProcRow) -> bool) -> Option<ProcId> {
        crate::plugins::pty_op::front::program_in_front(under, pred)
    }
    fn front(&self, shell: i32) -> crate::plugins::pty_op::front::Front {
        crate::plugins::pty_op::front::of(shell)
    }
    fn output_len(&self, _sid: u64) -> u64 {
        self.out.lock().unwrap().len() as u64
    }
    fn output_tail(&self, _sid: u64, bytes: usize) -> Vec<u8> {
        let o = self.out.lock().unwrap();
        o[o.len().saturating_sub(bytes)..].to_vec()
    }
    fn pane_modes(&self, _sid: u64) -> Option<(u64, u32)> {
        None
    }
    fn pane_screen(&self, _sid: u64) -> Option<String> {
        None
    }
    fn pane_unsent(&self, _sid: u64) -> Option<String> {
        None
    }
}

#[derive(Default)]
struct Host {
    ended: Mutex<bool>,
    log: Mutex<Vec<String>>,
}
impl PaneSessionHost for Host {
    fn shelld_session_id(&self) -> u64 {
        1
    }
    fn end(&self) {
        *self.ended.lock().unwrap() = true;
    }
    fn set_badge(&self, _t: &str) {}
    fn log(&self, _l: LogLevel, tag: &str, msg: &str) {
        self.log.lock().unwrap().push(format!("{tag} {msg}"));
    }
}

/// Run `op` against `shell` the way the host ticks it.
fn run(op: PtyOp, shell: &Shell) -> Vec<String> {
    let env = Env {
        io: Arc::new(Io(Arc::clone(&shell.pty))),
        out: Arc::clone(&shell.out),
    };
    let mut r = OpRunner::new(op, Box::new(env));
    let host = Host::default();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !*host.ended.lock().unwrap() {
        assert!(
            Instant::now() < deadline,
            "the op never ended: {:?}",
            host.log.lock().unwrap()
        );
        r.on_tick(&host);
        std::thread::sleep(Duration::from_millis(16));
    }
    host.log.into_inner().unwrap()
}

/// What the pane's shell is given before the op runs: aliases and a
/// stack of directories, as oh-my-zsh leaves them.
fn incident(program: &str, args: &[&str], setup: &str) {
    let tmp = marspot_term::test_env::sandbox_state_dir();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    for d in ["a", "b", "c", "home", "elsewhere"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    let home = root.join("home");
    let shell = Shell::spawn(program, args, &home);
    let setup = setup.replace("{root}", &root.display().to_string());
    shell.type_in(format!("{setup}; print_done() {{ echo SETUP-DONE; }}; print_done\r").as_bytes());
    shell.wait_for(0, "SETUP-DONE");
    let mark = shell.len();
    shell.type_in(b"echo AT:$PWD\r");
    shell.wait_for(mark, &format!("AT:{}", home.display()));

    // the residue: reports the line editor took apart, and the text it
    // kept, exactly as it sat in the user's line
    shell.type_in(b"\x1b[<35;3;1M\x1b[<35;2;2M35;3;1M35;2;2M");
    std::thread::sleep(Duration::from_millis(300));

    let line = PtyCommand::new("/bin/echo")
        .arg("RAN-ALONE")
        .in_dir(Some(root.join("elsewhere").display().to_string()))
        .clear_screen_first(true)
        .to_bytes()
        .unwrap();
    let mark = shell.len();
    let log = run(PtyOp::new("test.resume").shell_line(shell.pty.child_pid(), line, "resume"), &shell);
    assert!(
        log.iter().any(|l| l.contains("Done")),
        "the op finished: {log:?}"
    );
    shell.wait_for(mark, "RAN-ALONE");

    let mark = shell.len();
    shell.type_in(b"echo AT:$PWD\r");
    // the echo of the typed line says `AT:$PWD`; only the answer has a path
    shell.wait_for(mark, "AT:/");
    let after = shell.text()[mark..].to_string();
    assert!(
        after.contains(&format!("AT:{}", home.display())),
        "the shell stayed where it was; output after the op:\n{after}\nall:\n{}",
        shell.text()
    );
    let all = shell.text();
    assert!(
        !all.contains("not found"),
        "nothing but the command ran:\n{all}"
    );
}

const ZSH_SETUP: &str = "setopt autopushd pushdsilent; alias 1='cd -1' 2='cd -2' 3='cd -3'; \
                         cd {root}/a; cd {root}/b; cd {root}/c; cd {root}/home; PS1='P> '";

#[test]
fn a_line_full_of_mouse_reports_does_not_run_with_the_command_in_zsh() {
    incident("/bin/zsh", &["-f", "-i"], ZSH_SETUP);
}

#[test]
fn a_line_full_of_mouse_reports_does_not_run_with_the_command_in_zsh_vi_mode() {
    incident(
        "/bin/zsh",
        &["-f", "-i"],
        &format!("bindkey -v; {ZSH_SETUP}"),
    );
}

#[test]
fn a_line_full_of_mouse_reports_does_not_run_with_the_command_in_bash() {
    incident(
        "/bin/bash",
        &["--norc", "--noprofile", "-i"],
        "alias 1='cd {root}/a' 2='cd {root}/b' 3='cd {root}/c'; cd {root}/home; PS1='P> '",
    );
}

/// The 2026-11-17 incident: the pane's shell had a program in front of
/// it when a command line for the shell was due, and the program's own
/// redraw passed for the shell's new prompt.  Neither the interrupt nor
/// the line may reach the program: it keeps running, it reads nothing,
/// and the run ends saying who held the terminal.
#[test]
fn nothing_is_typed_while_another_program_holds_the_terminal() {
    use crate::plugins::pty_op::front::{self, Front};
    let tmp = marspot_term::test_env::sandbox_state_dir();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let got = root.join("got");
    let shell = Shell::spawn("/bin/zsh", &["-f", "-i"], &root);
    let pid = shell.pty.child_pid();
    shell.type_in(b"PS1='P> '; echo SETUP-DONE\r");
    shell.wait_for(0, "SETUP-DONE");
    // a program that keeps whatever reaches it, and that ^C would end
    shell.type_in(format!("/bin/cat > '{}'\r", got.display()).as_bytes());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !matches!(front::of(pid), Front::Program { ref comm, .. } if comm == "cat") {
        assert!(Instant::now() < deadline, "cat never took the terminal: {:?}", front::of(pid));
        std::thread::sleep(Duration::from_millis(20));
    }
    let Front::Program { pid: cat, .. } = front::of(pid) else { unreachable!() };
    let mark = shell.len();

    let line = PtyCommand::new("/bin/echo").arg("TYPED-INTO-CAT").to_bytes().unwrap();
    let log = run(PtyOp::new("test.resume").shell_line(pid, line, "resume"), &shell);

    assert!(!log.iter().any(|l| l.contains("Done")), "the op must not finish as done: {log:?}");
    assert!(
        log.iter().any(|l| l.starts_with("test.resume.not_typed") && l.contains("(cat) holds the terminal")),
        "the run says who held the terminal: {log:?}"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(front::of(pid), Front::Program { pid: cat, comm: "cat".into() }, "cat was not interrupted");
    let echoed = shell.text()[mark..].to_string();
    assert!(!echoed.contains("TYPED-INTO-CAT") && !echoed.contains("^C"), "the terminal echoed input: {echoed:?}");
    // end cat with an end-of-file, then read what it was given
    shell.type_in(b"\x04");
    shell.wait_for(mark, "P> ");
    assert_eq!(std::fs::read_to_string(&got).unwrap(), "", "cat was given input");
}

/// The program a script waited for exits before the script types at
/// it.  The paste must not reach the shell left holding the terminal,
/// which would read it as commands; the run ends saying why.
#[test]
fn nothing_is_pasted_once_the_awaited_program_has_exited() {
    use crate::plugins::pty_op::front::{self, Front};
    let tmp = marspot_term::test_env::sandbox_state_dir();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let shell = Shell::spawn("/bin/zsh", &["-f", "-i"], &root);
    let pid = shell.pty.child_pid();
    shell.type_in(b"PS1='P> '; echo SETUP-DONE\r");
    shell.wait_for(0, "SETUP-DONE");
    shell.type_in(b"/bin/sleep 30\r");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !matches!(front::of(pid), Front::Program { ref comm, .. } if comm == "sleep") {
        assert!(Instant::now() < deadline, "sleep never took the terminal: {:?}", front::of(pid));
        std::thread::sleep(Duration::from_millis(20));
    }
    let Front::Program { pid: sleeper, .. } = front::of(pid) else { unreachable!() };
    let mark = shell.len();
    // between finding the program and typing at it, the program goes:
    // an interrupt from the terminal, as a person's ^C would
    let pty = Arc::clone(&shell.pty);
    let gone: crate::plugins::pty_op::CallFn = Arc::new(move || {
        pty.write_shared(b"\x03").map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while front::of(pid) != Front::Shell {
            if Instant::now() > deadline {
                return Err("the shell never took its terminal back".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    });
    let op = PtyOp::new("test.paste")
        .step(Step::await_process(pid, |r| r.comm == "sleep"))
        .step(Step::call(gone))
        .step(Step::paste("echo PASTED-INTO-SHELL\r"));
    let log = run(op, &shell);

    assert!(!log.iter().any(|l| l.contains("Done")), "the op must not finish as done: {log:?}");
    assert!(
        log.iter().any(|l| l.starts_with("test.paste.not_typed") && l.contains(&format!("pid {sleeper}"))),
        "the run says the program it found is gone: {log:?}"
    );
    std::thread::sleep(Duration::from_millis(300));
    let after = shell.text()[mark..].to_string();
    assert!(!after.contains("PASTED-INTO-SHELL"), "the paste reached the shell: {after:?}");
}

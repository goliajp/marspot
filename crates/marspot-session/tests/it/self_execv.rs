//! A pane's L3 replaced under it by its own execv, end to end.
//!
//! The real `marspot-session` binary, a real shell on its PTY, and the
//! real requests: a `SessionUpdate` frame from L2, a SIGTERM from a
//! sender that predates it, a SIGHUP for a close.  Asked to update to a
//! different build in `binaries/current/`, the process must come back
//! as itself (same pid, same shell, same screen) running the new image,
//! once however often it is asked; asked to update to the build it runs,
//! nothing may happen; closed, it must go whatever is staged.

use crate::support;

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marspot_term::session_registry::{read_session_entry, session_socket_path};
use marspot_term::shell_proto::{Frame, MsgType, encode_inject_input};
use marspot_term::test_env::{EnvGuard, shell_without_rc_files};
use support::{COLS, ROWS, Session};

const ID: u64 = 4100;

/// `MARSPOT_STATE_DIR` at a directory of this test's own under `/tmp`.
///
/// Not the system temp dir: on macOS that is a long path under
/// `/var/folders`, and the session's socket inside it overflows the
/// 104 bytes a socket path may have, so the L3 cannot bind and exits.
struct Sandbox {
    env: EnvGuard,
    dir: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = PathBuf::from(format!(
            "/tmp/msx-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut env = EnvGuard::new();
        env.set("MARSPOT_STATE_DIR", &dir);
        Self { env, dir }
    }

    fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn bin() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_marspot-session"))
}

fn wait_for<T>(secs: u64, what: &str, f: impl FnMut() -> Option<T>) -> T {
    wait_for_or(secs, what, f, String::new)
}

/// [`wait_for`], with `more` said too when it times out.
fn wait_for_or<T>(
    secs: u64,
    what: &str,
    mut f: impl FnMut() -> Option<T>,
    more: impl Fn() -> String,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f() {
            return v;
        }
        if Instant::now() >= deadline {
            let state = std::env::var_os("MARSPOT_STATE_DIR").map(PathBuf::from);
            let logs = state.as_deref().map(log_tail).unwrap_or_default();
            panic!(
                "timed out after {secs}s waiting for {what}{}; the sandbox's logs:\n{logs}",
                more()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A pane with a plain shell on it, settled at its prompt.
fn start(sandbox: &mut Sandbox) -> Session {
    let shell = shell_without_rc_files(sandbox.path(), "/bin/zsh");
    sandbox.env.set("SHELL", &shell);
    let mut s = Session::spawn_with_id(bin(), COLS, ROWS, Some(ID));
    let state = sandbox.path().to_path_buf();
    wait_for(10, "the session's first frame", || {
        if let Some(st) = s.child.try_wait().unwrap() {
            panic!("the L3 exited ({st}) at boot; its log:\n{}", log_tail(&state));
        }
        (s.reader.seq() != 0).then_some(())
    });
    // the prompt, so the shell is reading its line
    wait_for(10, "the shell's prompt", || {
        let (_, (col, _)) = s.try_screen_and_cursor(COLS, ROWS)?;
        (col > 0).then_some(())
    });
    s
}

/// Have the shell print its own pid behind `tag`, and read it back
/// off the screen.
fn shell_pid(s: &Session, tag: &str) -> i32 {
    let f = echo_pid(tag);
    s.send_frame(f.msg_type, f.payload);
    wait_for(10, &format!("{tag}= and the prompt after it"), || read_echoed_pid(s, tag))
}

fn echo_pid(tag: &str) -> Frame {
    // Ctrl-U first: whatever was typed on the line goes
    Frame::new(
        MsgType::InjectInput,
        encode_inject_input(0, format!("\u{15}echo {tag}=$$\r").as_bytes()),
    )
}

/// The pid behind `tag=` once the prompt after it is drawn too: a
/// caller that reads the screen or the cursor right after must not race
/// the shell drawing it.
fn read_echoed_pid(s: &Session, tag: &str) -> Option<i32> {
    let prefix = format!("{tag}=");
    let (rows, (col, row)) = s.try_screen_and_cursor(COLS, ROWS)?;
    let at = rows.iter().position(|l| l.starts_with(&prefix))?;
    (row as usize > at && col > 0).then(|| rows[at][prefix.len()..].trim().parse().expect("a pid"))
}

/// The test's connection to the session's socket, reopened the way L2
/// reopens its own when the L3 closes it.
///
/// A connection accepted just before an execv is not handed over: the
/// new image closes it.  L2 sees the EOF and reconnects.  What must
/// never happen is input lost on a connection that stays open.
struct Link {
    stream: UnixStream,
}

impl Link {
    fn open() -> Self {
        let stream = support::reattach(&session_socket_path(ID), 15)
            .expect("the new image accepts a reattach on the session's socket");
        Self { stream }
    }

    /// The L3 closed its end: reading finds EOF.
    fn closed(&self) -> bool {
        use std::io::Read;
        self.stream.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 4096];
        let closed = loop {
            match (&self.stream).read(&mut buf) {
                Ok(0) => break true,
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break false,
                Err(_) => break true,
            }
        };
        self.stream.set_nonblocking(false).unwrap();
        closed
    }

    fn send_until<T>(
        &mut self,
        s: &Session,
        what: &str,
        send: impl Fn(&UnixStream),
        mut done: impl FnMut() -> Option<T>,
    ) -> T {
        send(&self.stream);
        let mut reconnects = 0;
        let screen = || format!("; the screen: {:#?}", s.try_screen_and_cursor(COLS, ROWS));
        wait_for_or(10, what, || {
            if let Some(v) = done() {
                return Some(v);
            }
            if self.closed() {
                reconnects += 1;
                assert!(reconnects <= 20, "the L3 kept closing the connection");
                *self = Self::open();
                send(&self.stream);
            }
            None
        }, screen)
    }
}

/// Copy the built binary into `current/`, with one byte of its
/// fingerprint changed when `differ`.  A changed byte voids the code
/// signature and macOS will not exec it, so the copy is re-signed
/// adhoc and verified before anything is signalled.
fn stage(state: &Path, differ: bool) -> PathBuf {
    let dir = state.join("binaries/current");
    std::fs::create_dir_all(&dir).unwrap();
    let dst = dir.join("marspot-session");
    let mut bytes = std::fs::read(bin()).unwrap();
    if differ {
        let needle = marspot_term::MARSPOT_FP_TERM.as_bytes();
        let at = bytes
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("this build's fingerprint is in the binary");
        let i = at + "MARSPOT_FP=".len();
        bytes[i] = if bytes[i] == b'0' { b'1' } else { b'0' };
    }
    // A new file renamed into place: the L3 may be running the old one,
    // and macOS kills a process whose executable is rewritten in place.
    let tmp = dir.join("marspot-session.tmp");
    std::fs::write(&tmp, &bytes).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
    let ok = |args: &[&str]| {
        std::process::Command::new("/usr/bin/codesign")
            .args(args)
            .arg(&tmp)
            .output()
            .is_ok_and(|o| o.status.success())
    };
    assert!(ok(&["--force", "--sign", "-"]), "re-sign the staged copy");
    assert!(ok(&["-v"]), "the staged copy verifies");
    std::fs::rename(&tmp, &dst).unwrap();
    dst
}

/// The last lines the sandbox's processes logged, for a failure message.
fn log_tail(state: &Path) -> String {
    let mut out = String::new();
    for e in std::fs::read_dir(state.join("logs")).into_iter().flatten().flatten() {
        let body = std::fs::read_to_string(e.path()).unwrap_or_default();
        let lines: Vec<&str> = body.lines().collect();
        out.push_str(&format!("== {}\n", e.path().display()));
        for l in &lines[lines.len().saturating_sub(80)..] {
            out.push_str(l);
            out.push('\n');
        }
    }
    out
}

fn signal(pid: u32, sig: i32) {
    assert_eq!(unsafe { libc::kill(pid as i32, sig) }, 0, "signal {sig} to {pid}");
}

fn sigterm(pid: u32) {
    signal(pid, libc::SIGTERM);
}

/// How the test asks the L3 to update.
#[derive(Clone, Copy, PartialEq)]
enum Ask {
    /// what L2 sends
    Frame,
    /// what a sender from before the frame sends
    Sigterm,
}

fn ask(s: &Session, how: Ask) {
    match how {
        Ask::Frame => s.send_frame(MsgType::SessionUpdate, Vec::new()),
        Ask::Sigterm => sigterm(s.pid()),
    }
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Ask the pane's L3 to update to a different build, and check that it
/// came back as itself: same pid, now running the staged file, the same
/// shell on its PTY, the same screen, and typing still reaching it.
///
/// With `burst` the request keeps coming every few milliseconds, the
/// way several senders in one install deliver it.  Frames keep coming
/// after the swap too, and must change nothing; SIGTERMs stop at the
/// swap -- one that reaches the new image with its own build in
/// `current/` is "end", by design -- so the ones that matter are those
/// sent while the swap was under way.  Either way there is one execv.
fn hand_over(sandbox: &mut Sandbox, how: Ask, burst: bool) {
    let mut s = start(sandbox);
    let pid = s.pid();
    let shell_before = shell_pid(&s, "before");
    let screen_before = s.screen(COLS, ROWS);
    let staged = stage(sandbox.path(), true).canonicalize().unwrap();
    let state = sandbox.path().to_path_buf();
    let running = || {
        marspot_term::platform::pid_exe_path(pid as i32).and_then(|p| p.canonicalize().ok())
    };
    assert_ne!(running().as_deref(), Some(staged.as_path()));

    ask(&s, how);
    // The new image is running under the same pid.  execv keeps the
    // pid whatever happens, so the executable is what says it ran.
    let mut after_swap = 0;
    wait_for(30, "the pid to run the staged image", || {
        if let Some(st) = s.child.try_wait().unwrap() {
            panic!(
                "the L3 exited ({st}) instead of handing over; its log:\n{}",
                log_tail(&state)
            );
        }
        let swapped = running().as_deref() == Some(staged.as_path());
        if burst && !(swapped && how == Ask::Sigterm) {
            ask(&s, how);
            std::thread::sleep(Duration::from_millis(3));
            after_swap += swapped as u32;
            return (swapped && (how == Ask::Sigterm || after_swap > 100)).then_some(());
        }
        swapped.then_some(())
    });
    // L2 reconnects over the listener after a swap; so does the test.
    let mut link = Link::open();
    // Typing reaches the same shell, through the new image.  Its echo
    // is also the first frame that is certainly the new image's: the
    // region keeps the old image's last frame until something replaces
    // it, and a screen identical to the one before publishes nothing.
    let (_, (c0, r0)) = wait_for(10, "a frame after the swap", || {
        s.try_screen_and_cursor(COLS, ROWS)
    });
    link.send_until(
        &s,
        "the echo of a key typed after the swap",
        |mut stream| {
            // a write to a stream the L3 just closed fails; the link
            // reconnects on that
            let _ = support::key_frame('q').write_to(&mut stream);
        },
        || {
            let (rows, _) = s.try_screen_and_cursor(COLS, ROWS)?;
            (rows[r0 as usize].chars().nth(c0 as usize) == Some('q')).then_some(())
        },
    );
    if !burst {
        // the screen it publishes is the one from before, plus the key
        let mut expected = screen_before.clone();
        let mut line: Vec<char> = expected[r0 as usize].chars().collect();
        line.resize(line.len().max(c0 as usize + 1), ' ');
        line[c0 as usize] = 'q';
        expected[r0 as usize] = line.into_iter().collect::<String>().trim_end().to_string();
        assert_eq!(s.screen(COLS, ROWS), expected, "the screen came back as it was");
    }
    let shell_after = link.send_until(
        &s,
        "after= and the prompt after it",
        |mut stream| {
            // a write to a stream the L3 just closed fails; the link
            // reconnects on that
            let _ = echo_pid("after").write_to(&mut stream);
        },
        || read_echoed_pid(&s, "after"),
    );
    assert_eq!(shell_after, shell_before, "the same shell is on the PTY");
    assert_eq!(s.child.try_wait().unwrap(), None);
    assert_eq!(running().as_deref(), Some(staged.as_path()));
    assert_eq!(count_in_log(&state, "L3_EXECV_INVOKE"), 1, "one execv, however often it was asked");
    s.kill();
}

/// The running image changes, nothing else does.
#[test]
fn an_update_replaces_the_image_and_keeps_the_pane() {
    let mut sandbox = Sandbox::new();
    hand_over(&mut sandbox, Ask::Frame, false);
}

/// Many requests, from several senders, before and after the swap: one
/// execv, and the pane lives.
#[test]
fn repeated_update_requests_move_the_pane_once() {
    let mut sandbox = Sandbox::new();
    hand_over(&mut sandbox, Ask::Frame, true);
}

/// An L2 or script from before `SessionUpdate` asks with SIGTERM.
#[test]
fn a_sigterm_from_an_older_sender_still_updates() {
    let mut sandbox = Sandbox::new();
    hand_over(&mut sandbox, Ask::Sigterm, false);
}

/// A SIGTERM arriving between the old image's execv and the new one's
/// handler is not a kill: it lost five panes on 2026-09-07, when two
/// fan-outs reached the same L3 in one install.  Nor is it a close:
/// it asked for the update that was already happening.
#[test]
fn sigterms_across_the_execv_do_not_kill_the_pane() {
    let mut sandbox = Sandbox::new();
    hand_over(&mut sandbox, Ask::Sigterm, true);
}

/// Asked to move to the build it already runs, an L3 does nothing: no
/// execv, no exit.  A second fan-out in one install asks exactly this.
#[test]
fn an_update_to_the_running_build_does_nothing() {
    let mut sandbox = Sandbox::new();
    let mut s = start(&mut sandbox);
    let state = sandbox.path().to_path_buf();
    let shell = shell_pid(&s, "before");
    stage(&state, false);
    s.send_frame(MsgType::SessionUpdate, Vec::new());
    wait_for(10, "the L3 to answer the request", || {
        (count_in_log(&state, "L3_UPDATE_NOOP") > 0).then_some(())
    });
    assert_eq!(s.child.try_wait().unwrap(), None, "still running");
    assert_eq!(count_in_log(&state, "L3_EXECV_PROBE_START"), 0, "no move was started");
    assert_eq!(shell_pid(&s, "after"), shell, "the same shell");
    s.kill();
}

/// The log line of the `n`th signal the L3 received.
fn signal_line(state: &Path, n: usize) -> Option<String> {
    std::fs::read_to_string(state.join("logs/marspot.log"))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains("\tL3_SIGNAL\t"))
        .nth(n)
        .map(str::to_string)
}

/// A SIGTERM from a process that is not launchd ends nothing, with
/// nothing staged or with the running build staged: the Dock sends one
/// to every process in the app's coalition when the user force-quits
/// any program started from a pane, and on 2026-11-11 that ended six
/// panes.  The L3 logs who sent it, and a close still closes: the L3
/// goes, its shell with it, and no live process is left for the
/// session directory to point at.
#[test]
fn a_sigterm_from_a_stranger_does_not_end_the_pane() {
    let mut sandbox = Sandbox::new();
    let mut s = start(&mut sandbox);
    let state = sandbox.path().to_path_buf();
    let shell = shell_pid(&s, "before");
    let entry = read_session_entry(ID).expect("the L3 registered itself");
    assert_eq!(entry.pid, s.pid() as i32);

    sigterm(s.pid());
    let line = wait_for(10, "the first signal to be logged", || signal_line(&state, 0));
    assert!(line.contains("sig=TERM"), "{line}");
    assert!(line.contains(&format!("from_pid={}\t", std::process::id())), "{line}");
    assert!(line.contains("taken_as=update"), "{line}");
    stage(&state, false);
    sigterm(s.pid());
    wait_for(10, "the second signal to be logged", || signal_line(&state, 1));
    wait_for(10, "the L3 to find nothing to update to", || {
        (count_in_log(&state, "L3_UPDATE_NOOP") == 2).then_some(())
    });
    assert_eq!(s.child.try_wait().unwrap(), None, "still running");
    assert_eq!(count_in_log(&state, "L3_EXECV_PROBE_START"), 0, "no move was started");
    assert_eq!(shell_pid(&s, "after"), shell, "the same shell");

    signal(s.pid(), libc::SIGHUP);
    let status = wait_for(20, "the L3 to exit", || s.child.try_wait().unwrap());
    assert_eq!(status.code(), Some(0), "a clean exit");
    let line = signal_line(&state, 2).expect("the close is logged");
    assert!(line.contains("sig=HUP") && line.contains("taken_as=close"), "{line}");
    wait_for(10, "the shell to go with it", || (!alive(shell)).then_some(()));
    assert!(
        !marspot_term::session_registry::pid_is_live_session(entry.pid),
        "the registry's pid is no longer a live session"
    );
}

/// The process group, session and controlling terminal of `pid`.
fn group_session_tty(pid: i32) -> (i32, i32, String) {
    let out = std::process::Command::new("/bin/ps")
        .args(["-o", "tty=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let tty = String::from_utf8_lossy(&out.stdout).trim().to_string();
    unsafe { (libc::getpgid(pid), libc::getsid(pid), tty) }
}

/// An L3 shares no process group and no session with whatever started
/// it, so a signal sent to the app's group passes it by; and leading a
/// session does not make the pane's terminal its controlling terminal,
/// which would send it the hangups meant for the shell.  An update
/// keeps all three.
#[test]
fn the_l3_leads_its_own_session_and_has_no_terminal() {
    let mut sandbox = Sandbox::new();
    let mut s = start(&mut sandbox);
    let state = sandbox.path().to_path_buf();
    let pid = s.pid() as i32;
    let own = (pid, pid, "??".to_string());
    assert_ne!(unsafe { libc::getpgid(0) }, pid, "the test is in another group");
    assert_eq!(group_session_tty(pid), own);
    let shell = shell_pid(&s, "sh");
    assert_ne!(group_session_tty(shell).1, pid, "the shell leads the terminal's session");

    stage(&state, true);
    s.send_frame(MsgType::SessionUpdate, Vec::new());
    wait_for(20, "the new image to arm its handlers", || {
        (count_in_log(&state, "L3_SIGTERM_INSTALLED") == 2).then_some(())
    });
    assert_eq!(group_session_tty(pid), own, "after an update");
    s.kill();
}

/// A close is a close whatever is staged.  It was a SIGTERM, which with
/// another build staged re-execed the pane instead of ending it.
#[test]
fn a_close_ends_the_pane_even_with_another_build_staged() {
    let mut sandbox = Sandbox::new();
    let mut s = start(&mut sandbox);
    let state = sandbox.path().to_path_buf();
    let shell = shell_pid(&s, "sh");
    stage(&state, true);
    signal(s.pid(), libc::SIGHUP);
    let status = wait_for(20, "the L3 to exit", || s.child.try_wait().unwrap());
    assert_eq!(status.code(), Some(0), "a clean exit");
    assert_eq!(count_in_log(&state, "L3_EXECV_PROBE_START"), 0, "no move was started");
    wait_for(10, "the shell to go with it", || (!alive(shell)).then_some(()));
}

/// The registry entry says which build the L3 runs, which is also how
/// L2 knows it takes `SessionUpdate`; after an update it says the new
/// one.
#[test]
fn the_registry_entry_names_the_running_build() {
    let mut sandbox = Sandbox::new();
    let mut s = start(&mut sandbox);
    let state = sandbox.path().to_path_buf();
    let entry = read_session_entry(ID).expect("the L3 registered itself");
    assert_eq!(entry.build, marspot_term::build::own());
    let staged = stage(&state, true);
    let want = marspot_term::build::of_file(&staged).expect("the staged build");
    s.send_frame(MsgType::SessionUpdate, Vec::new());
    wait_for(20, "the entry to name the staged build", || {
        read_session_entry(ID).ok().filter(|e| e.build == want).map(|_| ())
    });
    s.kill();
}

/// The fewest descriptors `pid` holds over 300 ms: one opened for a
/// moment (a log write, a file read at boot) is gone by then, one that
/// leaked is not.
fn open_fds(pid: u32) -> usize {
    let deadline = Instant::now() + Duration::from_millis(300);
    let mut fewest = usize::MAX;
    while Instant::now() < deadline {
        // PROC_PIDLISTFDS: one 8-byte `proc_fdinfo` per open descriptor
        let mut buf = vec![0u8; 8 * 4096];
        let n = unsafe {
            libc::proc_pidinfo(pid as i32, 1, 0, buf.as_mut_ptr().cast(), buf.len() as i32)
        };
        assert!(n > 0, "list the fds of {pid}");
        fewest = fewest.min(n as usize / 8);
        std::thread::sleep(Duration::from_millis(10));
    }
    fewest
}

fn count_in_log(state: &Path, tag: &str) -> usize {
    std::fs::read_to_string(state.join("logs/marspot.log"))
        .unwrap_or_default()
        .matches(tag)
        .count()
}

/// An L3 lives through many updates, and each one is an execv: nothing
/// it carries across may pile up.  The control stream used to: each
/// image adopted the inherited descriptor, read on it, and handed the
/// next image a duplicate, so one more descriptor stayed open for
/// good with every update.
#[test]
fn updates_do_not_accumulate_descriptors() {
    let mut sandbox = Sandbox::new();
    let mut s = start(&mut sandbox);
    let state = sandbox.path().to_path_buf();
    let pid = s.pid();
    // two builds in turn, so every request is a real move
    let swap = |s: &mut Session, n: usize| {
        stage(&state, n % 2 == 1);
        // the last thing a new image does at boot, after its file
        // watcher has opened what it holds for good
        let before = count_in_log(&state, "SESSION_OWNER");
        s.send_frame(MsgType::SessionUpdate, Vec::new());
        wait_for(20, &format!("update {n} to finish its boot"), || {
            assert_eq!(s.child.try_wait().unwrap(), None, "the L3 exited");
            (count_in_log(&state, "SESSION_OWNER") > before).then_some(())
        });
    };
    swap(&mut s, 1);
    let after_one = open_fds(pid);
    for n in 2..=6 {
        swap(&mut s, n);
    }
    assert_eq!(open_fds(s.pid()), after_one, "open descriptors after five more updates");
    s.kill();
}

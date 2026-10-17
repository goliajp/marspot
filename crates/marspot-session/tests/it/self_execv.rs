//! A pane's L3 replaced under it by its own execv, end to end.
//!
//! The real `marspot-session` binary, a real shell on its PTY, and a
//! real SIGTERM: what the update fan-out does to every pane.  Against
//! a different build in `binaries/current/` the process must come back
//! as itself (same pid, same shell, same screen) running the new
//! image; with nothing staged, or the same build staged, the same
//! signal means "quit" and the process must go.

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
/// A SIGTERM that reaches an image which already runs the staged build
/// still re-execs it, and a connection accepted just before that execv
/// is not handed over: the new image closes it.  L2 sees the EOF and
/// reconnects; a key written in between is lost there too.  What must
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
    std::fs::write(&dst, &bytes).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755)).unwrap();
    let ok = |args: &[&str]| {
        std::process::Command::new("/usr/bin/codesign")
            .args(args)
            .arg(&dst)
            .output()
            .is_ok_and(|o| o.status.success())
    };
    assert!(ok(&["--force", "--sign", "-"]), "re-sign the staged copy");
    assert!(ok(&["-v"]), "the staged copy verifies");
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

fn sigterm(pid: u32) {
    assert_eq!(
        unsafe { libc::kill(pid as i32, libc::SIGTERM) },
        0,
        "SIGTERM {pid}"
    );
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// SIGTERM the pane's L3 against what is staged, and check that it
/// came back as itself: same pid, now running the staged file, the same
/// shell on its PTY, the same screen, and typing still reaching it.
///
/// With `burst` the signal keeps coming every few milliseconds until the
/// new image runs and for a while after, the way two fan-outs in one
/// install deliver it: some land while the old image probes, some
/// between its execv and the new image's handler, some on the new one.
fn hand_over(sandbox: &mut Sandbox, differ: bool, burst: bool) {
    let mut s = start(sandbox);
    let pid = s.pid();
    let shell_before = shell_pid(&s, "before");
    let screen_before = s.screen(COLS, ROWS);
    let staged = stage(sandbox.path(), differ).canonicalize().unwrap();
    let state = sandbox.path().to_path_buf();
    let running = || {
        marspot_term::platform::pid_exe_path(pid as i32).and_then(|p| p.canonicalize().ok())
    };
    assert_ne!(running().as_deref(), Some(staged.as_path()));

    sigterm(pid);
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
        if burst {
            sigterm(pid);
            std::thread::sleep(Duration::from_millis(3));
            after_swap += swapped as u32;
            return (after_swap > 100).then_some(());
        }
        swapped.then_some(())
    });
    // L2 reconnects over the listener after a swap; so does the test.
    let mut link = Link::open();
    // Typing reaches the same shell, through the new image.  Its echo
    // is also the first frame that is certainly the new image's: the
    // region keeps the old image's last frame until something replaces
    // it, and a screen identical to the one before publishes nothing.
    // A frame that is not the echo can come first: a SIGTERM of the
    // burst that reached the new image re-execs it, and it republishes.
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
    s.kill();
}

/// The running image changes, nothing else does.
#[test]
fn an_update_replaces_the_image_and_keeps_the_pane() {
    let mut sandbox = Sandbox::new();
    hand_over(&mut sandbox, true, false);
}

/// A SIGTERM arriving between the old image's execv and the new one's
/// handler waits instead of killing the process: lost five panes on
/// 2026-09-07, when two fan-outs reached the same L3 in one install.
#[test]
fn sigterms_across_the_execv_do_not_kill_the_pane() {
    let mut sandbox = Sandbox::new();
    hand_over(&mut sandbox, true, true);
}

/// Nothing staged: SIGTERM is the user closing the pane, and the L3
/// takes its shell with it.  The session directory stays, for the next
/// L2 to resurrect or retire; what must not stay is a live process.
#[test]
fn with_nothing_staged_sigterm_quits() {
    let mut sandbox = Sandbox::new();
    let mut s = start(&mut sandbox);
    let shell = shell_pid(&s, "sh");
    let entry = read_session_entry(ID).expect("the L3 registered itself");
    assert_eq!(entry.pid, s.pid() as i32);
    sigterm(s.pid());
    let status = wait_for(20, "the L3 to exit", || s.child.try_wait().unwrap());
    assert_eq!(status.code(), Some(0), "a clean exit");
    wait_for(10, "the shell to go with it", || (!alive(shell)).then_some(()));
    assert!(
        !marspot_term::session_registry::pid_is_live_session(entry.pid),
        "the registry's pid is no longer a live session"
    );
}

/// A copy of the running build in `current/` is taken for an update
/// too: the fingerprint read from the file never equals the running
/// one (see `a_copy_of_the_running_build_counts_as_an_update` in the
/// handoff module).  Pinned, because a duplicate fan-out's second
/// SIGTERM lands on exactly this state and needs the pane to survive.
#[test]
fn with_the_same_build_staged_sigterm_reexecs_the_pane() {
    let mut sandbox = Sandbox::new();
    hand_over(&mut sandbox, false, false);
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
fn reexecs_do_not_accumulate_descriptors() {
    let mut sandbox = Sandbox::new();
    let mut s = start(&mut sandbox);
    let state = sandbox.path().to_path_buf();
    stage(&state, false);
    let pid = s.pid();
    let swap = |s: &mut Session, n: usize| {
        let before = count_in_log(&state, "L3_SIGTERM_INSTALLED");
        sigterm(pid);
        wait_for(20, &format!("re-exec {n} to arm its handler"), || {
            assert_eq!(s.child.try_wait().unwrap(), None, "the L3 exited");
            (count_in_log(&state, "L3_SIGTERM_INSTALLED") > before).then_some(())
        });
    };
    swap(&mut s, 1);
    let after_one = open_fds(pid);
    for n in 2..=6 {
        swap(&mut s, n);
    }
    assert_eq!(open_fds(s.pid()), after_one, "open descriptors after five more re-execs");
    s.kill();
}

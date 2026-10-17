//! A pane's L3 replaced under it by its own execv, end to end.
//!
//! The real `marspot-session` binary, a real shell on its PTY, and a
//! real SIGTERM: what the update fan-out does to every pane.  Against
//! a different build in `binaries/current/` the process must come back
//! as itself (same pid, same shell, same screen) running the new
//! image; with nothing staged, or the same build staged, the same
//! signal means "quit" and the process must go.

#[path = "../examples/support/mod.rs"]
mod support;

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

fn wait_for<T>(secs: u64, what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {secs}s waiting for {what}"
        );
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
        let (col, _) = s.cursor();
        (col > 0).then_some(())
    });
    s
}

fn line_starting(s: &Session, prefix: &str) -> Option<String> {
    s.try_screen(COLS, ROWS)?
        .into_iter()
        .find(|l| l.starts_with(prefix))
}

/// Have the shell print its own pid behind `tag`, and read it back
/// off the screen.
fn shell_pid(s: &Session, tag: &str, via: Option<&UnixStream>) -> i32 {
    // Ctrl-U first: whatever was typed on the line goes
    let cmd = format!("\u{15}echo {tag}=$$\r");
    match via {
        None => s.send_frame(MsgType::InjectInput, encode_inject_input(0, cmd.as_bytes())),
        Some(stream) => {
            let mut w = stream;
            Frame::new(MsgType::InjectInput, encode_inject_input(0, cmd.as_bytes()))
                .write_to(&mut w)
                .expect("write on the reattached stream");
        }
    }
    let line = wait_for(10, &format!("{tag}= on screen"), || {
        line_starting(s, &format!("{tag}="))
    });
    line[tag.len() + 1..].trim().parse().expect("a pid")
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
        for l in &lines[lines.len().saturating_sub(25)..] {
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
    let shell_before = shell_pid(&s, "before", None);
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
    let stream = support::reattach(&session_socket_path(ID), 15)
        .expect("the new image accepts a reattach on the session's socket");
    // Typing reaches the same shell, through the new image.  Its echo
    // is also the first frame that is certainly the new image's: the
    // region keeps the old image's last frame until something replaces
    // it, and a screen identical to the one before publishes nothing.
    let (c0, r0) = s.cursor();
    let seq = s.reader.seq();
    support::send_char_on(&stream, 'q');
    s.wait_seq_past(seq, "the echo of a key typed after the swap");
    assert_eq!(s.cell(c0, r0, COLS), 'q', "a key typed after the swap echoes");
    if !burst {
        // the screen it publishes is the one from before, plus the key
        let mut expected = screen_before.clone();
        let mut line: Vec<char> = expected[r0 as usize].chars().collect();
        line.resize(line.len().max(c0 as usize + 1), ' ');
        line[c0 as usize] = 'q';
        expected[r0 as usize] = line.into_iter().collect::<String>().trim_end().to_string();
        assert_eq!(s.screen(COLS, ROWS), expected, "the screen came back as it was");
    }
    let shell_after = shell_pid(&s, "after", Some(&stream));
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
    let shell = shell_pid(&s, "sh", None);
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

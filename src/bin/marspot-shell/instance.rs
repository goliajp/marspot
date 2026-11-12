//! One shell per state directory.
//!
//! A second launch against the same state dir used to come up as a full
//! app: its own core, the same saved windows, attached to the same
//! sessions, and the command socket of the first one unlinked from
//! under it.  Now the first shell holds an exclusive `flock` on a file
//! in the state root for as long as it lives, and a launch that finds
//! it held asks the holder to come to the front and exits.
//!
//! The lock belongs to the open file, so it goes when the process does
//! and there is no stale lock to clean up.  The descriptor is
//! close-on-exec: no child can keep the lock alive after the shell is
//! gone, and the shell's own `execv` (self-update, the redirect into
//! `binaries/current`) drops it and the new image takes it again at the
//! top of `main`.  A launch that slips into that gap becomes the app
//! and the re-exec'd shell bows out to it: still one instance.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use marspot::{lx_event, lx_warn};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationOptions, NSRunningApplication};

static LOCK: OnceLock<File> = OnceLock::new();
/// Whether a session started this shell at the request of one outside
/// its coalition.
static HEIR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn is_heir() -> bool {
    HEIR.load(Ordering::Relaxed)
}
/// Pid of a launch that asked us to come forward; 0 when none has.
static ASKED_BY: AtomicU32 = AtomicU32::new(0);
/// The dev seams (`MARSPOT_DEV_*`) the last such launch was started
/// with.  A resident shell that becomes the app on its request takes
/// them over: a test starts the app with a seam set, and the process
/// that answers is one that was started before it.
static ASKED_WITH: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());
const SEAM_PREFIX: &str = "MARSPOT_DEV_";

/// What a launch that stands aside sends: its pid, then its seams.
fn front_request(pid: u32, env: impl Iterator<Item = (String, String)>) -> String {
    let mut text = format!("{pid}\n");
    for (k, v) in env.filter(|(k, v)| k.starts_with(SEAM_PREFIX) && !v.contains('\n')) {
        text.push_str(&format!("{k}={v}\n"));
    }
    text
}

fn parse_front_request(text: &str) -> Option<(u32, Vec<(String, String)>)> {
    let mut lines = text.lines();
    let pid = lines.next()?.trim().parse().ok()?;
    let seams = lines
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| k.starts_with(SEAM_PREFIX))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Some((pid, seams))
}

/// The seams of the launch that last asked this shell to come forward.
pub(crate) fn asked_with() -> Vec<(String, String)> {
    ASKED_WITH.lock().map(|g| g.clone()).unwrap_or_default()
}

fn lock_path() -> PathBuf {
    marspot_term::paths::state_root().join("shell.lock")
}

fn front_socket_path() -> PathBuf {
    marspot_term::paths::state_root().join("front.sock")
}

pub(crate) enum Claim {
    Ours(File),
    /// Another process holds it; its pid, when the file already says.
    Held(Option<u32>),
}

/// Take the lock at `path`, or say who has it.
pub(crate) fn claim(path: &Path) -> io::Result<Claim> {
    // std opens with O_CLOEXEC
    let mut file = File::options().read(true).write(true).create(true).truncate(false).open(path)?;
    let me = std::process::id();
    let mut unnamed_tries = 0;
    loop {
        // SAFETY: flock on a descriptor this function owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(me.to_string().as_bytes())?;
            return Ok(Claim::Ours(file));
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::WouldBlock {
            return Err(err);
        }
        let mut text = String::new();
        file.seek(SeekFrom::Start(0))?;
        file.read_to_string(&mut text)?;
        match text.trim().parse::<u32>().ok() {
            // our own pid: we are the image a holder exec'd into, and a
            // child it forked just before has not reached its own exec
            // yet, so its copy of the old descriptor is still open
            Some(pid) if pid == me => {}
            Some(pid) => return Ok(Claim::Held(Some(pid))),
            // locked a moment ago, pid not written yet
            None if unnamed_tries < 20 => unnamed_tries += 1,
            None => return Ok(Claim::Held(None)),
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// How long an heir waits for the shell it takes over from to let go.
const HEIR_PATIENCE: Duration = Duration::from_secs(5);

/// A shell a session started on request: say that it is here, then
/// take the lock when the shell that asked lets go of it.
fn claim_as_heir(path: &Path) -> io::Result<Claim> {
    let heir_file = marspot_term::owner::heir_path();
    let deadline = std::time::Instant::now() + HEIR_PATIENCE;
    loop {
        let claimed = claim(path)?;
        if matches!(claimed, Claim::Ours(_)) || std::time::Instant::now() >= deadline {
            let _ = std::fs::remove_file(&heir_file);
            return Ok(claimed);
        }
        if !heir_file.exists() {
            std::fs::write(&heir_file, std::process::id().to_string())?;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Become the shell of this state dir, or hand over to the one that is
/// and exit.  Nothing but the log line is written by a launch that
/// loses.
pub(crate) fn hold_or_exit() {
    let path = lock_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let heir = std::env::var_os(marspot_term::owner::HEIR_ENV).is_some();
    // SAFETY: startup path in main(), before any thread is spawned.
    unsafe { std::env::remove_var(marspot_term::owner::HEIR_ENV) };
    HEIR.store(heir, Ordering::Relaxed);
    let claimed = if heir { claim_as_heir(&path) } else { claim(&path) };
    match claimed {
        Ok(Claim::Ours(file)) => {
            let _ = LOCK.set(file);
        }
        // started by a session to answer for it: another shell does
        Ok(Claim::Held(holder)) if heir || crate::resident::asked() => {
            lx_event!(
                "SHELL_NOT_NEEDED",
                "started to answer for the sessions, and a shell already does; exiting",
                holder_pid = holder.unwrap_or(0)
            );
            std::process::exit(0);
        }
        Ok(Claim::Held(holder)) => {
            lx_event!(
                "SHELL_ALREADY_RUNNING",
                "this state dir already has a shell; asked it to come forward and exiting",
                holder_pid = holder.unwrap_or(0)
            );
            // a holder that is still booting has no socket yet, and
            // will put its own window in front when it gets there
            if let Ok(mut stream) = UnixStream::connect(front_socket_path()) {
                let _ = stream.write_all(front_request(std::process::id(), std::env::vars()).as_bytes());
            }
            // this launch is the one the system just let come forward;
            // pass that on by name as well
            if let Some(app) = holder
                .and_then(|pid| NSRunningApplication::runningApplicationWithProcessIdentifier(pid as libc::pid_t))
            {
                app.activateWithOptions(NSApplicationActivationOptions::empty());
            }
            std::process::exit(0);
        }
        Err(e) => panic!("cannot take the instance lock at {}: {e}", path.display()),
    }
}

/// Listen for launches that found the lock held.  `wake` gets the main
/// loop to look at what arrived.  A shell that cannot bind still runs;
/// a second launch then exits without bringing it forward.
pub(crate) fn serve_front(wake: impl Fn() + Send + 'static) {
    let path = front_socket_path();
    // ours to replace: we hold the lock, so whoever bound it is gone
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            lx_warn!("shell.front.bind_failed", &format!("{e}"));
            return;
        }
    };
    let serve = move || {
        for mut stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
            let mut text = String::new();
            if (&mut stream).take(16 * 1024).read_to_string(&mut text).is_err() {
                continue;
            }
            if let Some((pid, seams)) = parse_front_request(&text) {
                if let Ok(mut with) = ASKED_WITH.lock() {
                    *with = seams;
                }
                ASKED_BY.store(pid, Ordering::Release);
                wake();
            }
        }
    };
    if let Err(e) = std::thread::Builder::new().name("l1-front".into()).spawn(serve) {
        lx_warn!("shell.front.thread_failed", &format!("{e}"));
    }
}

/// Main thread: if a second launch asked, come to the front.
pub(crate) fn answer_front() {
    let pid = ASKED_BY.swap(0, Ordering::AcqRel);
    if pid == 0 {
        return;
    }
    lx_event!("SHELL_ACTIVATED_BY", "a second launch asked this shell to come forward", pid = pid);
    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    let windows = app.windows();
    // with every window in the dock, activating alone shows nothing
    if !windows.iter().any(|w| w.isVisible())
        && let Some(w) = windows.iter().find(|w| w.isMiniaturized())
    {
        w.deminiaturize(None);
    }
    #[allow(deprecated)] // `activate()` only asks; the launch that stood aside was the active app
    app.activateIgnoringOtherApps(true);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::process::{Command, Stdio};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("marspot-lock-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("shell.lock")
    }

    fn held_by(path: &Path) -> Option<u32> {
        match claim(path).unwrap() {
            Claim::Held(pid) => pid,
            Claim::Ours(_) => panic!("the lock was free"),
        }
    }

    /// Run this test binary again as `role`, which the helper test
    /// below acts out.
    fn helper(role: &str, path: &Path) -> std::process::Child {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "instance::tests::helper_process", "--nocapture"])
            .env("MARSPOT_LOCK_ROLE", role)
            .env("MARSPOT_LOCK_PATH", path)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// The line a helper prints once it has done its part.
    fn said(child: &mut std::process::Child, key: &str) -> u32 {
        let out = std::io::BufReader::new(child.stdout.take().unwrap());
        for line in out.lines() {
            let line = line.unwrap();
            if let Some(v) = line.strip_prefix(key) {
                return v.trim().parse().unwrap();
            }
        }
        panic!("the helper never said {key}");
    }

    /// Not a test of its own: the other half of the two below.
    #[test]
    fn helper_process() {
        use std::os::unix::process::CommandExt;
        let Ok(role) = std::env::var("MARSPOT_LOCK_ROLE") else { return };
        let path = PathBuf::from(std::env::var("MARSPOT_LOCK_PATH").unwrap());
        match role.as_str() {
            // hold the lock, start a child the way the shell starts
            // its own, and stay
            "hold" => {
                let Claim::Ours(_file) = claim(&path).unwrap() else { panic!("not free") };
                let mut child = Command::new("/bin/sleep").arg("60").spawn().unwrap();
                println!("child={}", child.id());
                let _ = child.wait();
            }
            // hold the lock, then become a new image of this binary
            "exec" => {
                let Claim::Ours(_file) = claim(&path).unwrap() else { panic!("not free") };
                let err = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "instance::tests::helper_process", "--nocapture"])
                    .env("MARSPOT_LOCK_ROLE", "after-exec")
                    .exec();
                panic!("exec failed: {err}");
            }
            "after-exec" => {
                let Claim::Ours(_file) = claim(&path).unwrap() else { panic!("lost it across exec") };
                println!("held-again={}", std::process::id());
                std::thread::sleep(Duration::from_secs(60));
            }
            other => panic!("unknown role {other}"),
        }
    }

    #[test]
    fn a_launch_that_stands_aside_passes_on_its_pid_and_its_seams_only() {
        let env = [("MARSPOT_DEV_CLOSE_SEQUENCE", "2,1"), ("HOME", "/Users/x"), ("MARSPOT_STATE_DIR", "/tmp/s")];
        let text = front_request(77, env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        assert_eq!(text, "77\nMARSPOT_DEV_CLOSE_SEQUENCE=2,1\n");
        assert_eq!(parse_front_request(&text), Some((77, vec![("MARSPOT_DEV_CLOSE_SEQUENCE".into(), "2,1".into())])));
        // a shell from before the seams were sent
        assert_eq!(parse_front_request("77"), Some((77, vec![])));
        assert_eq!(parse_front_request("HOME=/x\n"), None);
    }

    /// A second claim is told who holds the lock, and the holder's
    /// child does not keep it once the holder is gone -- the bench-lock
    /// failure, where an inherited descriptor outlived its owner.
    #[test]
    fn a_second_claim_names_the_holder_and_a_child_does_not_keep_the_lock() {
        let path = scratch("child");
        let mut holder = helper("hold", &path);
        let child = said(&mut holder, "child=") as libc::pid_t;
        assert_eq!(held_by(&path), Some(holder.id()));
        holder.kill().unwrap();
        holder.wait().unwrap();
        // SAFETY: signal 0 only asks whether the pid exists.
        assert_eq!(unsafe { libc::kill(child, 0) }, 0, "the child is still running");
        let free = matches!(claim(&path).unwrap(), Claim::Ours(_));
        // SAFETY: the pid of the sleep the helper started and reported.
        unsafe { libc::kill(child, libc::SIGKILL) };
        assert!(free, "the holder's child kept the lock");
    }

    #[test]
    fn the_same_pid_holds_it_again_after_exec() {
        let path = scratch("exec");
        let mut holder = helper("exec", &path);
        let pid = said(&mut holder, "held-again=");
        assert_eq!(pid, holder.id(), "exec keeps the pid");
        assert_eq!(held_by(&path), Some(pid));
        holder.kill().unwrap();
        holder.wait().unwrap();
    }
}

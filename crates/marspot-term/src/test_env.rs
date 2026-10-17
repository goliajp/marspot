//! What a test needs to change the process environment, own a scratch
//! directory and spawn children without leaving any of it behind.
//!
//! Compiled into every build so the tests of the crates above this one
//! can use it; nothing outside a test calls it.
//!
//! The environment is one table per process.  Under `cargo test` the
//! tests of a binary run on threads of that process, so two tests that
//! each set `MARSPOT_STATE_DIR` race, and one that removes it on the way
//! out sends the other's writes to the user's real state root.  Every
//! test that changes a variable holds [`EnvGuard`], which takes the one
//! process-wide lock and puts back what it changed when it is dropped,
//! panics included.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::{Mutex, MutexGuard};

static ENV: Mutex<()> = Mutex::new(());

std::thread_local! {
    static HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Exclusive use of the process environment for one test.
///
/// Every variable it sets or removes is recorded the first time it is
/// touched and restored on drop.  [`EnvGuard::preserve`] records one
/// without changing it, for code under test that sets it itself.
pub struct EnvGuard {
    saved: Vec<(OsString, Option<OsString>)>,
    _lock: MutexGuard<'static, ()>,
}

impl EnvGuard {
    /// Take the lock.  A test that panicked while holding it does not
    /// poison it for the rest: its guard has already restored what it
    /// changed by the time the next one gets in.
    ///
    /// Panics when this thread already holds one, which would otherwise
    /// deadlock.
    pub fn new() -> Self {
        assert!(
            !HELD.with(|h| h.replace(true)),
            "this thread already holds an EnvGuard; take one per test"
        );
        let lock = ENV.lock().unwrap_or_else(|p| p.into_inner());
        Self {
            saved: Vec::new(),
            _lock: lock,
        }
    }

    pub fn set(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.preserve(key.as_ref());
        // SAFETY: every writer of the environment in a test holds ENV.
        unsafe { std::env::set_var(key, value) };
        self
    }

    pub fn remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.preserve(key.as_ref());
        // SAFETY: as in `set`.
        unsafe { std::env::remove_var(key) };
        self
    }

    pub fn preserve(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        let key = key.as_ref();
        if !self.saved.iter().any(|(k, _)| k == key) {
            self.saved.push((key.to_owned(), std::env::var_os(key)));
        }
        self
    }
}

impl Default for EnvGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.saved.drain(..).rev() {
            // SAFETY: the lock is still held; it is released after this.
            match value {
                Some(v) => unsafe { std::env::set_var(&key, v) },
                None => unsafe { std::env::remove_var(&key) },
            }
        }
        HELD.with(|h| h.set(false));
    }
}

/// A fresh directory under the system temp dir, removed on drop.
///
/// The name carries the pid, a per-process counter and the clock, so two
/// runs, two worktrees or two tests of one binary never share one.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let path = std::env::temp_dir().join(format!(
            "marspot-test-{tag}-{}-{}-{nanos}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create the test's temp dir");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn join(&self, p: impl AsRef<Path>) -> PathBuf {
        self.path.join(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// `MARSPOT_STATE_DIR` pointed at a directory of this test's own.
///
/// `env` is declared first so it is dropped first: the variable goes
/// back before the directory it named is removed.
pub struct StateSandbox {
    pub env: EnvGuard,
    pub dir: TempDir,
}

impl StateSandbox {
    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

/// One struct rather than a `(guard, dir)` pair: `let (_g, _) = …` would
/// drop the directory on the spot and leave the variable naming nothing.
pub fn sandbox_state_dir() -> StateSandbox {
    let dir = TempDir::new("state");
    let mut env = EnvGuard::new();
    env.set("MARSPOT_STATE_DIR", dir.path());
    StateSandbox { env, dir }
}

/// A script that starts `shell` without reading any startup file, for
/// a test that needs a real interactive shell but not the user's one.
///
/// The user's rc files make a test slow, and make what it sees depend
/// on whose machine it runs on.  A script rather than the shell itself
/// because `$SHELL` is one path with no arguments, and because a pane
/// starts its shell with a `-` in argv[0], which makes zsh and bash read
/// the login files: the kernel hands a script's interpreter its own
/// argv[0], so that dash never reaches the shell.
pub fn shell_without_rc_files(dir: &Path, shell: &str) -> PathBuf {
    let flags = match Path::new(shell).file_name().and_then(|n| n.to_str()) {
        Some("zsh") => "-f",
        Some("bash") => "--norc --noprofile",
        _ => "",
    };
    let name = Path::new(shell)
        .file_name()
        .map_or_else(|| "shell".into(), |n| n.to_string_lossy().into_owned());
    let script = dir.join(format!("{name}-without-rc"));
    std::fs::write(&script, format!("#!/bin/sh\nexec {shell} {flags}\n"))
        .expect("write the shell script");
    std::fs::set_permissions(
        &script,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .expect("make the shell script executable");
    script
}

/// A child that is killed and reaped when the test is done with it,
/// however the test ends.
///
/// [`ChildGuard::group`] is for a child spawned with
/// `process_group(0)`: a shell script's children are not the script, and
/// killing only the script leaves them running.  The group is killed
/// while its leader is still unreaped, so the id cannot have been
/// reused by then.
pub struct ChildGuard {
    child: Child,
    group: bool,
}

impl ChildGuard {
    pub fn new(child: Child) -> Self {
        Self {
            child,
            group: false,
        }
    }

    pub fn group(child: Child) -> Self {
        Self { child, group: true }
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }
}

impl std::ops::Deref for ChildGuard {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.child
    }
}

impl std::ops::DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let pid = self.child.id() as libc::pid_t;
        if self.group && unreaped(pid) {
            unsafe { libc::killpg(pid, libc::SIGKILL) };
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// True while `pid` is our child and has not been reaped: alive, or a
/// zombie whose id the kernel still holds.
fn unreaped(pid: libc::pid_t) -> bool {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let r = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
        )
    };
    r == 0
}

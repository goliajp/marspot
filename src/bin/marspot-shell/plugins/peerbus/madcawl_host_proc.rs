//! The process side of the madcawl host: binding its socket, starting
//! it with the listener as fd 3, ending and reaping it, and the pid file
//! an image left behind when it replaced itself by execv.

use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use marspot_term::{lx_info, lx_warn};

use super::madcawl_host::Config;

/// How long a host is given to leave after SIGTERM.
const GRACE: Duration = Duration::from_secs(2);
/// The host's own log is cut back to nothing at a start past this.
const LOG_CAP: u64 = 1 << 20;

/// Bind `socket` mode 0600 from the moment it can be reached: bound in
/// a private directory first, then renamed over whatever is at the
/// path, a stale socket of an earlier run included.  The path is
/// marspot's alone.  The umask is not touched: it belongs to every
/// thread of the process.
pub(super) fn bind(socket: &Path) -> io::Result<UnixListener> {
    let dir = socket.parent().ok_or_else(|| io::Error::other("socket path has no directory"))?;
    fs::create_dir_all(dir)?;
    let private = dir.join(format!(".madcawl-bind-{}", std::process::id()));
    let _ = fs::remove_dir_all(&private);
    fs::DirBuilder::new().mode(0o700).create(&private)?;
    let at = private.join("s");
    let bound = UnixListener::bind(&at).and_then(|l| {
        fs::set_permissions(&at, fs::Permissions::from_mode(0o600))?;
        fs::rename(&at, socket)?;
        Ok(l)
    });
    let _ = fs::remove_file(&at);
    let _ = fs::remove_dir(&private);
    bound
}

/// Start a host serving the listener `lfd`; its pid.
pub(super) fn spawn(cfg: &Config, lfd: RawFd) -> io::Result<i32> {
    if let Some(dir) = cfg.host_log.parent() {
        fs::create_dir_all(dir)?;
    }
    let log = fs::OpenOptions::new().create(true).append(true).open(&cfg.host_log)?;
    if log.metadata()?.len() > LOG_CAP {
        log.set_len(0)?;
    }
    let mut cmd = Command::new(&cfg.shell);
    cmd.arg("--serve")
        .arg(&cfg.socket)
        .args(["--listen-fd", "3", "--exit-idle"])
        .arg(cfg.idle_secs.to_string())
        .arg("--allow")
        .arg(&cfg.home)
        .arg("--audit")
        .arg(&cfg.audit)
        .current_dir(&cfg.home)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    let limit = marspot_term::inherited_fds::close_limit();
    // SAFETY: dup2 and fcntl only, both async-signal-safe; the listener
    // is close-on-exec in the shell and becomes fd 3 without the flag
    // in this child alone. Any other descriptor another thread of the
    // shell holds without the flag for a moment (a socketpair before
    // its flag is set, the instance lock around an execv) is marked so
    // the host does not keep it for its lifetime.
    unsafe {
        cmd.pre_exec(move || {
            let r = if lfd == 3 { libc::fcntl(3, libc::F_SETFD, 0) } else { libc::dup2(lfd, 3) };
            if r < 0 {
                return Err(io::Error::last_os_error());
            }
            marspot_term::inherited_fds::close_inherited(limit, &[3]);
            Ok(())
        });
    }
    // the child is waited for by pid (`reap`); dropping the handle
    // neither waits nor kills
    Ok(cmd.spawn()?.id() as i32)
}

/// Reap our child `pid` if it has exited already.
pub(super) fn try_reap(pid: i32) -> Option<ExitStatus> {
    let mut st = 0;
    // SAFETY: one child of ours, by pid, without blocking
    (unsafe { libc::waitpid(pid, &mut st, libc::WNOHANG) } == pid).then(|| ExitStatus::from_raw(st))
}

/// Wait for our child `pid` to exit and reap it.
pub(super) fn reap(pid: i32) -> Option<ExitStatus> {
    let mut st = 0;
    loop {
        // SAFETY: waits for one child of ours, by pid
        let r = unsafe { libc::waitpid(pid, &mut st, 0) };
        if r == pid {
            return Some(ExitStatus::from_raw(st));
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return None;
        }
    }
}

/// SIGTERM our child `pid`, SIGKILL it if it is still there after
/// `GRACE`, and reap it.
pub(super) fn terminate(pid: i32) -> Option<ExitStatus> {
    let kq = Kq::new().ok();
    let watched = kq.as_ref().is_some_and(|k| k.exit(pid, 1).is_ok());
    // SAFETY: the pid is our unreaped child, so it cannot have been reused
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let gone = if watched {
        matches!(kq.as_ref().map(|k| k.wait(Some(GRACE))), Some(Ok(Some(1))))
    } else {
        // no kqueue to wait in: look now and then, on this teardown path only
        let until = std::time::Instant::now() + GRACE;
        loop {
            if let Some(s) = try_reap(pid) {
                return Some(s);
            }
            if std::time::Instant::now() >= until {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    };
    if !gone {
        // SAFETY: as above
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    reap(pid)
}

/// Record the running host for the image that may replace this one.
pub(super) fn write_pid_file(path: &Path, pid: i32) {
    let start = marspot::pidtree::proc_row(pid).map_or(0, |r| r.start_us);
    if let Err(e) = fs::write(path, format!("{pid} {start}\n")) {
        lx_warn!("madcawl.host.pid_file_failed", &e.to_string(), path = path.display());
    }
}

/// End the host a previous image of this process started, if the pid
/// file names one that is still our child.
///
/// It is ended, not adopted: the listener it was handed was
/// close-on-exec in that image and is gone from this one, so this image
/// can neither wait on it nor hand it to a later host, while the old
/// host would keep accepting on a socket file this image replaces; and
/// it runs the madcawl programs of before the update.  What ending it
/// costs is its open sessions, which `madcawl-mcp` reconnects from on
/// the next call.
pub(super) fn end_previous(pid_file: &Path) {
    let Ok(text) = fs::read_to_string(pid_file) else { return };
    let _ = fs::remove_file(pid_file);
    let mut words = text.split_whitespace().map(str::parse::<u64>);
    let (Some(Ok(pid)), Some(Ok(start))) = (words.next(), words.next()) else { return };
    // a pid of 0 or below would wait for, or signal, a whole group
    let Ok(pid) = i32::try_from(pid) else { return };
    if pid <= 0 {
        return;
    }
    // an inherited child: from here on this function alone waits for it
    if crate::inherited::disown(pid) {
        return lx_info!("madcawl.host.previous_ended", "the previous image's host had exited; reaped with the inherited children", pid = pid);
    }
    let mut st = 0;
    // SAFETY: a non-blocking look at one pid; ECHILD when it is not ours
    let r = unsafe { libc::waitpid(pid, &mut st, libc::WNOHANG) };
    if r == pid {
        return lx_info!("madcawl.host.previous_ended", "the previous image's host had exited; reaped", pid = pid);
    }
    if r != 0 {
        return;
    }
    if marspot::pidtree::proc_row(pid).map(|p| p.start_us) != Some(start) {
        return lx_warn!("madcawl.host.previous_kept", "a child of ours with that pid started at another time", pid = pid);
    }
    let status = terminate(pid).map_or_else(|| "unknown".to_string(), |s| s.to_string());
    lx_info!("madcawl.host.previous_ended", "ended the previous image's host", pid = pid, status = status);
}

/// A kqueue of the read and process-exit events the host thread waits on.
pub(super) struct Kq(OwnedFd);

impl Kq {
    pub(super) fn new() -> io::Result<Kq> {
        // SAFETY: no arguments; a new fd or -1
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh fd nobody else owns; kqueues are not inherited
        Ok(Kq(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    fn change(&self, ident: usize, filter: i16, flags: u16, fflags: u32, token: usize) -> io::Result<()> {
        let ev = libc::kevent { ident, filter, flags, fflags, data: 0, udata: token as *mut libc::c_void };
        // SAFETY: one change in, no events out
        let r = unsafe { libc::kevent(self.0.as_raw_fd(), &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// Add, enable or disable the read event of `fd`.
    pub(super) fn read(&self, fd: usize, flags: u16, token: usize) -> io::Result<()> {
        self.change(fd, libc::EVFILT_READ, flags, 0, token)
    }

    /// Watch for `pid` exiting, once; ESRCH when it already has.
    pub(super) fn exit(&self, pid: i32, token: usize) -> io::Result<()> {
        self.change(pid as usize, libc::EVFILT_PROC, libc::EV_ADD | libc::EV_ONESHOT, libc::NOTE_EXIT, token)
    }

    /// The token of the next event; None when `timeout` passed first.
    pub(super) fn wait(&self, timeout: Option<Duration>) -> io::Result<Option<usize>> {
        let ts = timeout.map(|t| libc::timespec { tv_sec: t.as_secs() as libc::time_t, tv_nsec: t.subsec_nanos() as libc::c_long });
        // SAFETY: kevent is plain data; all zeroes is a valid value
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        loop {
            let tp = ts.as_ref().map_or(std::ptr::null(), |t| t as *const libc::timespec);
            // SAFETY: room for one event
            let n = unsafe { libc::kevent(self.0.as_raw_fd(), std::ptr::null(), 0, &mut ev, 1, tp) };
            match n {
                0 => return Ok(None),
                n if n > 0 => return Ok(Some(ev.udata as usize)),
                _ if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => continue,
                _ => return Err(io::Error::last_os_error()),
            }
        }
    }
}

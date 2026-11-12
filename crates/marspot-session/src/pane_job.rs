//! A pane's shell started as a launchd job of its own.
//!
//! macOS puts an app launch and everything it starts into one
//! coalition, and a process never leaves the one it was born in.  A
//! program started in a pane that shows up in the Dock is listed there
//! with that coalition, and quitting its icon sends SIGTERM to every
//! member: on 2026-11-11 that reached six sessions and every program in
//! every pane.  Only launchd starts a process in a coalition of its
//! own, so the shell is handed to launchd: what a pane starts then
//! shares a coalition with that pane and nothing else.
//!
//! The job is a host that starts the shell on the pane's terminal,
//! says which pid it is and exits (`marspot_term::pane_host`).  The
//! session then takes the job off launchd's list: the shell stays, in
//! the coalition the job was given, and nothing of the job is left for
//! the system to end when the app quits -- launchd stops the jobs an
//! app asked for along with the app.
//!
//! The host is the app's main executable, where the app that started
//! this session left word of it: what the pane runs is then charged to
//! the app by the privacy system, as it is for a forked shell.  When
//! the app named none, this binary is the host: the coalition without
//! the app's name.
//!
//! What it costs, and why it is off unless `MARSPOT_PANE_JOB=1`: the
//! scheduler places each coalition's threads by how busy that coalition
//! is.  A program that only feeds the terminal is not busy, and in a
//! coalition of its own it is put on the efficiency cores once the
//! sessions fill the performance cores: four panes printing at full
//! speed together took in a third less (180 MB/s against 267).  One or
//! two panes, and programs that compute, run as before.  When launchd
//! does not take the job, the shell is forked the way it always was.
//!
//! Starting the session itself under launchd, so that it shares the
//! pane's coalition with what writes to it, does not undo this.  Each
//! pane's coalition is then judged on its own, none is busy enough,
//! and from four panes up the sessions go to the efficiency cores with
//! their writers: 100 MB/s at four, 83 at nine.  Started as an
//! application in front (`pane_host::FRONT_ROLE`) four panes keep
//! their speed, five lose a fifth and nine a half.  The throughput
//! probe measures both (`MARSPOT_PROBE_SESSION_JOB=1`).

// built off macOS for its tests only: nothing there starts a job
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::io;
use std::path::PathBuf;

use marspot_term::pty::{Pty, PtyConfig};

/// The session whose pane the shell is for.
pub(crate) struct ForSession(pub(crate) u64);

impl ForSession {
    /// The shell `config` describes, on a terminal of its own.
    pub(crate) fn spawn_shell(self, config: PtyConfig) -> io::Result<Pty> {
        spawn_shell(self.0, config)
    }
}

pub(crate) const HOST_ARG: &str = marspot_term::pane_host::ARG;

/// This binary as the host: what `MARSPOT_PANE_JOB=1` falls back to.
pub(crate) fn host_main() {
    #[cfg(target_os = "macos")]
    if let Some(spec) = std::env::args_os().nth(2) {
        marspot_term::pane_host::run(std::path::Path::new(&spec));
    }
}

#[cfg(target_os = "macos")]
use macos::spawn_shell;

#[cfg(not(target_os = "macos"))]
fn spawn_shell(_id: u64, config: PtyConfig) -> io::Result<Pty> {
    Pty::spawn(config)
}

/// The executable the job runs, or none when the shell is to be forked.
/// `switch` is `MARSPOT_PANE_JOB`, `named` what the app left in
/// `MARSPOT_PANE_HOST`.
fn host_program(switch: Option<&str>, named: Option<PathBuf>, own: impl FnOnce() -> Option<PathBuf>) -> Option<PathBuf> {
    match switch {
        Some("1") => named.or_else(own),
        _ => None,
    }
}

/// The environment `config` asks for, as `NAME=value` entries: what
/// this process has, less the prefixes, then the names set on purpose.
fn child_env(config: &PtyConfig, inherited: impl Iterator<Item = (Vec<u8>, Vec<u8>)>) -> Vec<Vec<u8>> {
    let set = |name: &[u8]| config.env_set.iter().any(|(k, _)| k.as_bytes() == name);
    let mut env: Vec<Vec<u8>> = inherited
        .filter(|(k, _)| !config.env_remove_prefixes.iter().any(|p| k.starts_with(p.as_bytes())) && !set(k))
        .map(|(k, v)| [k.as_slice(), b"=", v.as_slice()].concat())
        .collect();
    env.extend(config.env_set.iter().map(|(k, v)| format!("{k}={v}").into_bytes()));
    env
}

#[cfg(target_os = "macos")]
mod macos {
    use std::io;
    use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
    use std::os::unix::ffi::OsStringExt;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use marspot_term::launchd_job;
    use marspot_term::pane_host::{self, Spec};
    use marspot_term::pty::{Pty, PtyConfig};
    use marspot_term::{lx_event, lx_warn};

    use super::{child_env, host_program};

    pub(crate) fn spawn_shell(id: u64, config: PtyConfig) -> io::Result<Pty> {
        // this crate's unit tests are a harness, which is no pane host
        let host = if cfg!(test) {
            None
        } else {
            host_program(
                std::env::var("MARSPOT_PANE_JOB").ok().as_deref(),
                std::env::var_os(pane_host::PROGRAM_ENV).map(PathBuf::from),
                || std::env::current_exe().ok(),
            )
        };
        let Some(host) = host else { return Pty::spawn(config) };
        match spawn_as_job(id, &config, &host) {
            Ok(pty) => Ok(pty),
            Err(e) => {
                lx_warn!("PANE_JOB_FALLBACK", "launchd did not start the pane's shell; forking it", session = id, err = e);
                Pty::spawn(config)
            }
        }
    }

    fn spawn_as_job(id: u64, config: &PtyConfig, host: &Path) -> io::Result<Pty> {
        let (master, slave, tty) = open_terminal(config)?;
        let mut argv = vec![config.argv0.clone().unwrap_or_else(|| config.program.clone()).into_bytes()];
        argv.extend(config.args.iter().map(|a| a.clone().into_bytes()));
        let spec = Spec {
            tty,
            reply: Vec::new(),
            cwd: config.cwd.clone().unwrap_or_default().into_bytes(),
            program: config.program.clone().into_bytes(),
            limits: pane_host::own_limits(),
            argv,
            env: child_env(config, std::env::vars_os().map(|(k, v)| (k.into_vec(), v.into_vec()))),
        };
        let begun = Instant::now();
        let started = launchd_job::start(&id.to_string(), host, spec)?;
        let shell = started.pid;
        // the shell holds the terminal open now; ours would keep a
        // hangup from ever reaching us
        drop(slave);
        lx_event!(
            "PANE_JOB_STARTED",
            "launchd started the pane's shell, in a coalition of its own",
            session = id,
            label = started.label,
            host = host.display(),
            shell_pid = shell,
            shell_coalition = launchd_job::coalition_of(shell),
            own_coalition = launchd_job::coalition_of(std::process::id() as i32),
            load_ms = started.load_ms,
            answer_ms = started.answer_ms,
            ms = begun.elapsed().as_millis()
        );
        Ok(Pty::from_raw_master(master.into_raw_fd(), shell))
    }

    /// A pseudo-terminal of the size `config` asks for: both ends and
    /// the path of the far one.
    fn open_terminal(config: &PtyConfig) -> io::Result<(OwnedFd, OwnedFd, Vec<u8>)> {
        // SAFETY: plain libc calls on a descriptor made here.  NOCTTY
        // throughout: this process leads a session and must not acquire
        // the pane's terminal as its own.
        unsafe {
            let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            if m < 0 {
                return Err(io::Error::last_os_error());
            }
            let master = OwnedFd::from_raw_fd(m);
            if libc::grantpt(m) != 0 || libc::unlockpt(m) != 0 {
                return Err(io::Error::last_os_error());
            }
            // ptsname answers from a buffer of its own; copied before
            // anything else can ask
            let name = libc::ptsname(m);
            if name.is_null() {
                return Err(io::Error::last_os_error());
            }
            let tty = std::ffi::CStr::from_ptr(name).to_bytes().to_vec();
            let s = libc::open(name, libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
            if s < 0 {
                return Err(io::Error::last_os_error());
            }
            let slave = OwnedFd::from_raw_fd(s);
            let ws = libc::winsize {
                ws_row: config.size.rows,
                ws_col: config.size.cols,
                ws_xpixel: config.size.pixel_width,
                ws_ypixel: config.size.pixel_height,
            };
            if libc::ioctl(m, libc::TIOCSWINSZ, &ws) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok((master, slave, tty))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_gets_the_inherited_environment_less_the_prefixes_plus_what_is_set() {
        let config = PtyConfig {
            env_remove_prefixes: vec!["MARSPOT_".to_string()],
            env_set: vec![("PATH".to_string(), "/x:/bin".to_string()), ("MARSPOT_TOKEN".to_string(), "t".to_string())],
            ..PtyConfig::default()
        };
        let inherited = [("PATH", "/bin"), ("MARSPOT_SESSION_ID", "9"), ("HOME", "/Users/x")];
        let env = child_env(&config, inherited.iter().map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec())));
        assert_eq!(env, vec![b"HOME=/Users/x".to_vec(), b"PATH=/x:/bin".to_vec(), b"MARSPOT_TOKEN=t".to_vec()]);
    }

    /// Asked for, the app's executable when the app named it and this
    /// binary when it did not; not asked for, nothing.
    #[test]
    fn the_host_is_the_app_when_it_named_itself() {
        let app = || Some(PathBuf::from("/A/Marspot.app/Contents/MacOS/marspot-shell"));
        let own = || Some(PathBuf::from("/s/marspot-session"));
        assert_eq!(host_program(None, app(), own), None);
        assert_eq!(host_program(None, None, own), None);
        assert_eq!(host_program(Some("1"), None, own), own());
        assert_eq!(host_program(Some("1"), app(), own), app());
        assert_eq!(host_program(Some("0"), app(), own), None);
    }
}

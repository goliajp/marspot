//! The madcawl host (`madcawl-shell --serve`) that the `madcawl` MCP
//! server of every agent reaches through `MADCAWL_SOCKET`, started the
//! way launchd starts a socket-activated job: the shell binds the
//! socket itself, and only when a connection waits on it does it start
//! a host and hand it the listener as fd 3.  So there is no host
//! process until an agent's first madcawl tool call, never more than
//! one, and the host leaves by itself after five idle minutes; the next
//! call starts another.
//!
//! One thread per bound socket blocks in kqueue with no timeout, on the
//! listener while no host runs and on the host's exit while one does,
//! so an idle shell spends nothing on it.  Hosts that keep failing stop
//! the thread (`BRAKE`) until the next config pass.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use marspot_term::{lx_info, lx_warn};

use super::madcawl_host_proc::{self as proc, Kq};
use super::mcp_servers;

/// Hosts in a row that fail, or exit too soon, before the thread gives
/// up and closes the socket.
const BRAKE: u32 = 5;

/// Where and how a host is started; `of_env` in the app, built by hand
/// in tests.
#[derive(Clone, Debug)]
pub(super) struct Config {
    pub shell: PathBuf,
    pub socket: PathBuf,
    /// The running host's pid and start time, for the image that
    /// replaces this one by execv.
    pub pid_file: PathBuf,
    pub host_log: PathBuf,
    pub audit: PathBuf,
    pub home: PathBuf,
    pub idle_secs: u32,
    /// A host that exits sooner than this counts against the brake.
    pub quick: Duration,
    /// Pages in content processes of their own (`--isolate`); only the
    /// sandbox web pane asks for it, since an installed host serves
    /// every agent's MCP session and an older one refuses the flag.
    pub isolate: bool,
}

impl Config {
    fn of_env() -> Option<Config> {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        let root = marspot_term::paths::state_root();
        let logs = marspot_term::paths::log_dir();
        Some(Config {
            shell: home.join(mcp_servers::MADCAWL_SHELL_BIN),
            socket: mcp_servers::madcawl_socket(),
            pid_file: root.join("madcawl-host.pid"),
            host_log: logs.join("madcawl-host.log"),
            audit: logs.join("madcawl-audit.log"),
            home,
            idle_secs: 300,
            quick: Duration::from_secs(10),
            isolate: false,
        })
    }
}

struct Running {
    wake: UnixStream,
    thread: JoinHandle<()>,
}

/// The bound socket and its thread, while both madcawl programs are
/// installed.
#[derive(Default)]
pub(super) struct Host {
    run: Option<Running>,
    previous_ended: bool,
}

impl Host {
    /// Bind or close the socket after what is installed now; run by
    /// the peerbus worker at start and on every config pass.
    pub(super) fn reconcile(&mut self) {
        let Some(mut cfg) = Config::of_env() else { return };
        // the sandbox shares the developer's HOME, and the agents there
        // are registered with the installed app's socket, not this one:
        // it runs a host only for the web pane its dev seam opens, from
        // the program that seam names
        if std::env::var_os("MARSPOT_DEV_SANDBOX").is_some() {
            let (Some(_), Some(program)) = (marspot::web_pane::dev_url(), marspot::web_pane::dev_host_program()) else {
                return;
            };
            cfg.shell = program;
            cfg.isolate = true;
            let present = cfg.shell.is_file();
            return self.apply(cfg, present);
        }
        let present = mcp_servers::madcawl_installed(&cfg.home);
        self.apply(cfg, present);
    }

    pub(super) fn apply(&mut self, cfg: Config, present: bool) {
        if !self.previous_ended {
            proc::end_previous(&cfg.pid_file);
            self.previous_ended = true;
        }
        // a thread that gave up has closed the socket; this pass starts
        // from zero
        if self.run.as_ref().is_some_and(|r| r.thread.is_finished()) {
            self.stop();
        }
        match (present, self.run.is_some()) {
            (true, false) => self.start(cfg),
            (false, true) => self.stop(),
            _ => {}
        }
    }

    fn start(&mut self, cfg: Config) {
        let listener = match proc::bind(&cfg.socket) {
            Ok(l) => l,
            Err(e) => return lx_warn!("madcawl.host.bind_failed", &e.to_string(), socket = cfg.socket.display()),
        };
        let spawned = UnixStream::pair().and_then(|(wake, waker)| {
            let thread = std::thread::Builder::new()
                .name("madcawl-host".into())
                .spawn(move || serve(&cfg, listener, waker))?;
            Ok(Running { wake, thread })
        });
        match spawned {
            Ok(r) => self.run = Some(r),
            Err(e) => lx_warn!("madcawl.host.thread_failed", &e.to_string()),
        }
    }

    /// End the host, if one runs, close and unlink the socket.
    pub(super) fn stop(&mut self) {
        if let Some(r) = self.run.take() {
            let _ = (&r.wake).write_all(&[0]);
            let _ = r.thread.join();
        }
    }

    #[cfg(test)]
    pub(super) fn bound(&self) -> bool {
        self.run.as_ref().is_some_and(|r| !r.thread.is_finished())
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stop();
    }
}

const LISTENER: usize = 1;
const WAKE: usize = 2;
const CHILD: usize = 3;

fn serve(cfg: &Config, listener: UnixListener, wake: UnixStream) {
    lx_info!("madcawl.host.bound", "waiting for the first madcawl connection", socket = cfg.socket.display());
    match Kq::new() {
        Ok(kq) => run(cfg, &kq, &listener, &wake),
        Err(e) => lx_warn!("madcawl.host.kqueue_failed", &e.to_string()),
    }
    drop(listener);
    let _ = std::fs::remove_file(&cfg.socket);
}

/// Start a host for each wait on the listener, until told to stop or
/// the brake gives up.
fn run(cfg: &Config, kq: &Kq, listener: &UnixListener, wake: &UnixStream) {
    let lfd = listener.as_raw_fd() as usize;
    let armed = kq.read(lfd, libc::EV_ADD, LISTENER).and_then(|()| kq.read(wake.as_raw_fd() as usize, libc::EV_ADD, WAKE));
    if let Err(e) = armed {
        return lx_warn!("madcawl.host.kqueue_failed", &e.to_string());
    }
    let mut bad = 0;
    loop {
        if !matches!(kq.wait(None), Ok(Some(LISTENER))) {
            return lx_info!("madcawl.host.stopped", "the shell closed the socket with no host running");
        }
        let started = Instant::now();
        let (failed, last) = match proc::spawn(cfg, listener.as_raw_fd()) {
            Ok(pid) => match watch(cfg, kq, lfd, pid, started) {
                Some(outcome) => outcome,
                None => return,
            },
            Err(e) => {
                lx_warn!("madcawl.host.spawn_failed", &e.to_string(), program = cfg.shell.display());
                (true, format!("not started: {e}"))
            }
        };
        bad = if failed { bad + 1 } else { 0 };
        if bad >= BRAKE {
            return lx_warn!(
                "madcawl.host.gave_up",
                "hosts kept failing; socket closed until the next config pass",
                in_a_row = bad,
                last_status = last
            );
        }
    }
}

/// Wait for the host `pid` to exit, with the listener left to it.
/// Whether it failed, and how it exited; None when the shell said stop
/// and it was ended.
fn watch(cfg: &Config, kq: &Kq, lfd: usize, pid: i32, started: Instant) -> Option<(bool, String)> {
    proc::write_pid_file(&cfg.pid_file, pid);
    lx_info!("madcawl.host.spawn", "a connection is waiting; host started", pid = pid);
    let _ = kq.read(lfd, libc::EV_DISABLE, LISTENER);
    // ESRCH: it is gone already, and reaping it does not block; a child
    // that exited just before the registration may never be reported,
    // so it is looked for once after it
    let registered = kq.exit(pid, CHILD).is_ok();
    let early = if registered { proc::try_reap(pid) } else { None };
    let stop = early.is_none() && registered && !matches!(kq.wait(None), Ok(Some(CHILD)));
    let status = match early {
        Some(s) => Some(s),
        None if stop => proc::terminate(pid),
        None => proc::reap(pid),
    };
    let _ = std::fs::remove_file(&cfg.pid_file);
    let lifetime = started.elapsed();
    let status_text = status.map_or_else(|| "unknown".to_string(), |s| s.to_string());
    lx_info!(
        "madcawl.host.exit",
        if stop { "host ended by the shell" } else { "host exited" },
        pid = pid,
        status = status_text,
        lifetime_ms = lifetime.as_millis()
    );
    if stop {
        return None;
    }
    let _ = kq.read(lfd, libc::EV_ENABLE, LISTENER);
    Some((!status.is_some_and(|s| s.success()) || lifetime < cfg.quick, status_text))
}

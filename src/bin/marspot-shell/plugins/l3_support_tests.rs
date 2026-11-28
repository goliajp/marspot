//! Real session processes for the plugins' tests: a short state root, a
//! session process on a pane id with the shell a test gives it, typing
//! into it the way the core forwards input, and a plugin host whose op
//! queue runs against it.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marspot_term::shell_proto::{encode_inject_input, encode_pane_hold_grid, Frame, MsgType};

use crate::plugins::{pty_op, LogLevel, PaneSession, PaneSessionHost, PluginError, PluginHost};

/// A state root with a short path: a session binds its socket under it
/// and macOS caps a socket path at 104 bytes.
pub(crate) struct Root(pub(crate) PathBuf);

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One session process, killed and reaped when dropped.
pub(crate) struct L3 {
    pub(crate) child: std::process::Child,
    pub(crate) control: UnixStream,
}

impl Drop for L3 {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn no_cloexec(fd: i32) {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert!(flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } >= 0);
}

/// Session `sid` on the built session binary, its shell `shell`, with
/// `env` added to its environment.
pub(crate) fn spawn_l3(sid: u64, shell: &Path, path: &str, env: &[(&str, &str)]) -> L3 {
    let region = marspot_term::grid_shm::create_region(80, 24).unwrap();
    let (control, child_end) = UnixStream::pair().unwrap();
    no_cloexec(region.as_raw_fd());
    no_cloexec(child_end.as_raw_fd());
    let child = std::process::Command::new(marspot_term::test_env::built_binary("marspot-session"))
        .env("MARSPOT_SHELL_CONTROL_FD", child_end.as_raw_fd().to_string())
        .env("MARSPOT_SHM_FD", region.as_raw_fd().to_string())
        .env("MARSPOT_SESSION_ID", sid.to_string())
        .env("SHELL", shell)
        .env("PATH", path)
        .envs(env.iter().copied())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // what the session says on its socket is not read here; left unread
    // it would fill the socket and stall the session
    let mut drain = control.try_clone().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while drain.read(&mut buf).is_ok_and(|n| n > 0) {}
    });
    L3 { child, control }
}

/// Typing into the session the way the core forwards it.
pub(crate) struct Proxy(pub(crate) Mutex<UnixStream>);

impl Proxy {
    pub(crate) fn of(l3: &L3) -> Proxy {
        Proxy(Mutex::new(l3.control.try_clone().unwrap()))
    }
    fn frame(&self, t: MsgType, payload: Vec<u8>) -> std::io::Result<()> {
        Frame::new(t, payload).write_to(&mut *self.0.lock().unwrap()).map(|_| ())
    }
}

impl crate::plugins::claudecode::InjectInputProxy for Proxy {
    fn inject_input(&self, sid: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.frame(MsgType::InjectInput, encode_inject_input(sid, bytes))
    }
    fn hold_grid(&self, sid: u64, on: bool) -> std::io::Result<()> {
        self.frame(MsgType::PaneHoldGrid, encode_pane_hold_grid(sid, on))
    }
    fn paste(&self, sid: u64, text: &str) -> std::io::Result<()> {
        self.inject_input(sid, text.as_bytes())
    }
    fn reset_mouse_reporting(&self, _: u64) -> std::io::Result<()> {
        Ok(())
    }
}

impl pty_op::PtyIo for Proxy {
    fn send(&self, sid: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.frame(MsgType::InjectInput, encode_inject_input(sid, bytes))
    }
    fn hold(&self, sid: u64, on: bool) -> std::io::Result<()> {
        self.frame(MsgType::PaneHoldGrid, encode_pane_hold_grid(sid, on))
    }
    fn paste(&self, sid: u64, text: &str) -> std::io::Result<()> {
        self.send(sid, text.as_bytes())
    }
    fn program_input(&self, _: u64, _: pty_op::Typed<'_>) -> std::io::Result<()> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "no program input in these tests"))
    }
    fn reset_mouse_reporting(&self, _: u64) -> std::io::Result<()> {
        Ok(())
    }
}

/// A route to no pane, for tests that only look at what was asked.
pub(crate) struct NoPane;

impl pty_op::PtyIo for NoPane {
    fn send(&self, _: u64, _: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
    fn hold(&self, _: u64, _: bool) -> std::io::Result<()> {
        Ok(())
    }
    fn paste(&self, _: u64, _: &str) -> std::io::Result<()> {
        Ok(())
    }
    fn program_input(&self, _: u64, _: pty_op::Typed<'_>) -> std::io::Result<()> {
        Ok(())
    }
    fn reset_mouse_reporting(&self, _: u64) -> std::io::Result<()> {
        Ok(())
    }
}

/// `f`'s first Some within 15 s, asked every 100 ms.
pub(crate) fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    eprintln!("waiting for {what}");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A plugin host whose op queue types into real panes, with the launch
/// log as its after-typed hook as in production, recording what it was
/// asked and what was logged.
pub(crate) struct OpsHost {
    pub(crate) state_dir: PathBuf,
    ops: Mutex<pty_op::PtyOps>,
    sessions: Mutex<Vec<(u64, Box<dyn PaneSession>)>>,
    /// What each op types, as (pane, every `Send` step joined).
    pub(crate) sent: Mutex<Vec<(u64, String)>>,
    /// Every op's step labels.
    pub(crate) submitted: Mutex<Vec<(u64, Vec<&'static str>)>>,
    pub(crate) logged: Mutex<Vec<(String, String)>>,
}

impl OpsHost {
    pub(crate) fn new(state_dir: PathBuf, io: Arc<dyn pty_op::PtyIo>) -> OpsHost {
        OpsHost {
            state_dir,
            ops: Mutex::new(pty_op::PtyOps::new(io).with_after_typed(crate::plugins::launches::after_typed())),
            sessions: Mutex::default(),
            sent: Mutex::default(),
            submitted: Mutex::default(),
            logged: Mutex::default(),
        }
    }

    /// Start what was queued and step what runs, as the supervisor does.
    pub(crate) fn drive(&self) {
        self.ops.lock().unwrap().pump(self);
        for (sid, s) in self.sessions.lock().unwrap().iter_mut() {
            s.on_tick(&Pane(*sid));
        }
    }
}

struct Pane(u64);

impl PaneSessionHost for Pane {
    fn shelld_session_id(&self) -> u64 {
        self.0
    }
    fn end(&self) {}
    fn set_badge(&self, _: &str) {}
    fn log(&self, _: LogLevel, _: &str, _: &str) {}
}

impl PluginHost for OpsHost {
    fn state_dir(&self) -> Result<PathBuf, PluginError> {
        std::fs::create_dir_all(&self.state_dir)?;
        Ok(self.state_dir.clone())
    }
    fn log(&self, _: LogLevel, event: &str, line: &str) {
        self.logged.lock().unwrap().push((event.to_string(), line.to_string()));
    }
    fn submit_pty_op_at(&self, sid: u64, op: pty_op::PtyOp, start_at: usize) -> Result<(), PluginError> {
        self.submitted.lock().unwrap().push((sid, op.steps.iter().map(|s| s.label).collect()));
        let sent: Vec<String> = op
            .steps
            .iter()
            .filter_map(|s| match &s.kind {
                pty_op::StepKind::Send(b) => Some(String::from_utf8_lossy(b).into_owned()),
                _ => None,
            })
            .collect();
        self.sent.lock().unwrap().push((sid, sent.join("\n")));
        self.ops.lock().unwrap().submit_at(sid, op, start_at).map(|_| ()).ok_or_else(|| PluginError::Other("queue full".into()))
    }
    fn begin_pane_session(&self, sid: u64, session: Box<dyn PaneSession>) -> Result<(), PluginError> {
        self.sessions.lock().unwrap().push((sid, session));
        Ok(())
    }
}

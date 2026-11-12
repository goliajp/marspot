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
//! The job is this binary again, as a host that opens the pane's
//! terminal, starts the shell under it and waits.  The host stays for
//! what the session did as the shell's parent and no longer can: it
//! ends a shell that outlives its session, and it takes the job off
//! launchd's list.
//!
//! What it costs: nothing under a launchd job has the app as its
//! responsible process, so a privacy prompt names the program that
//! asked, as it does in a pane that outlived the app that opened it.
//!
//! Off unless `MARSPOT_PANE_JOB=1`.  When launchd does not take the
//! job, the shell is forked the way it always was.

// built off macOS for its tests only: nothing there starts a job
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::io;

use marspot_term::pty::{Pty, PtyConfig};

pub(crate) const HOST_ARG: &str = "--pane-host";

/// The session whose pane the shell is for.
pub(crate) struct ForSession(pub(crate) u64);

impl ForSession {
    /// The shell `config` describes, on a terminal of its own.
    pub(crate) fn spawn_shell(self, config: PtyConfig) -> io::Result<Pty> {
        spawn_shell(self.0, config)
    }
}

#[cfg(target_os = "macos")]
mod host;
#[cfg(target_os = "macos")]
pub(crate) use host::host_main;
#[cfg(target_os = "macos")]
use macos::spawn_shell;

#[cfg(not(target_os = "macos"))]
pub(crate) fn host_main() {}

#[cfg(not(target_os = "macos"))]
fn spawn_shell(_id: u64, config: PtyConfig) -> io::Result<Pty> {
    Pty::spawn(config)
}

/// What the host needs to start the shell, as the file it is handed.
#[derive(Debug, PartialEq, Eq, Default)]
struct Spec {
    /// The session's pid: the host stops the shell when it is gone.
    session: i32,
    tty: Vec<u8>,
    reply: Vec<u8>,
    label: Vec<u8>,
    cwd: Vec<u8>,
    program: Vec<u8>,
    argv: Vec<Vec<u8>>,
    env: Vec<Vec<u8>>,
}

const SPEC_MAGIC: &[u8] = b"marspot-pane-job 1";

impl Spec {
    /// NUL-terminated fields: none of them can hold a NUL, and an
    /// environment value can hold anything else.
    fn encode(&self) -> Vec<u8> {
        let (session, argc) = (self.session.to_string().into_bytes(), self.argv.len().to_string().into_bytes());
        let head = [SPEC_MAGIC, &session, &self.tty, &self.reply, &self.label, &self.cwd, &self.program, &argc];
        let mut out = Vec::new();
        for field in head.into_iter().chain(self.argv.iter().map(Vec::as_slice)).chain(self.env.iter().map(Vec::as_slice)) {
            out.extend_from_slice(field);
            out.push(0);
        }
        out
    }

    fn decode(bytes: &[u8]) -> Option<Spec> {
        let mut fields = bytes.strip_suffix(&[0])?.split(|b| *b == 0).map(<[u8]>::to_vec);
        if fields.next()? != SPEC_MAGIC {
            return None;
        }
        let session: i32 = std::str::from_utf8(&fields.next()?).ok()?.parse().ok()?;
        let (tty, reply, label, cwd, program) = (fields.next()?, fields.next()?, fields.next()?, fields.next()?, fields.next()?);
        let argc: usize = std::str::from_utf8(&fields.next()?).ok()?.parse().ok()?;
        let mut argv = Vec::new();
        for _ in 0..argc {
            argv.push(fields.next()?);
        }
        Some(Spec { session, tty, reply, label, cwd, program, argv, env: fields.collect() })
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

/// The job's name: one state directory's sessions never share a label
/// with another's, so a sandbox cannot touch the installed app's jobs.
fn label_for(state_root: &[u8], id: u64) -> String {
    let mut hash: u32 = 0x811c_9dc5;
    for b in state_root {
        hash = (hash ^ u32::from(*b)).wrapping_mul(0x0100_0193);
    }
    format!("com.marspot.pane.{hash:08x}.{id}")
}

/// `state_dir` is what a sandbox's tools know its processes by; the
/// host carries it and the shell does not.
fn plist(label: &str, exe: &str, spec: &str, state_dir: Option<&str>) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let env = state_dir.map_or(String::new(), |d| {
        format!("<key>EnvironmentVariables</key><dict><key>MARSPOT_STATE_DIR</key><string>{}</string></dict>\n", esc(d))
    });
    // interactive: launchd otherwise holds a job to less than an app gets.
    // the process group is left alone when the host goes: the shell and
    // its jobs are not the host's to end
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <plist version=\"1.0\"><dict>\n\
         <key>Label</key><string>{}</string>\n\
         <key>ProgramArguments</key><array><string>{}</string><string>{HOST_ARG}</string><string>{}</string></array>\n\
         <key>RunAtLoad</key><true/>\n\
         <key>ProcessType</key><string>Interactive</string>\n\
         <key>AbandonProcessGroup</key><true/>\n\
         {env}</dict></plist>\n",
        esc(label),
        esc(exe),
        esc(spec)
    )
}

/// The host's answer: the shell's pid, or why there is none.
fn parse_reply(line: &str) -> io::Result<i32> {
    match line.trim_end().split_once(' ') {
        Some(("pid", n)) => n.parse().map_err(|_| io::Error::other(format!("pane host said {line:?}"))),
        _ => Err(io::Error::other(format!("pane host: {}", line.trim_end()))),
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::CString;
    use std::fs;
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::OpenOptionsExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use marspot_term::pty::{Pty, PtyConfig};
    use marspot_term::{lx_event, lx_warn};

    use super::{Spec, child_env, label_for, parse_reply, plist};

    /// How long launchd and the host get to produce a shell before the
    /// pane is given one the old way.
    const HOST_WAIT: Duration = Duration::from_secs(5);
    pub(super) const LAUNCHCTL: &str = "/bin/launchctl";

    pub(crate) fn spawn_shell(id: u64, config: PtyConfig) -> io::Result<Pty> {
        if std::env::var("MARSPOT_PANE_JOB").as_deref() != Ok("1") {
            return Pty::spawn(config);
        }
        match spawn_as_job(id, &config) {
            Ok(pty) => Ok(pty),
            Err(e) => {
                lx_warn!("PANE_JOB_FALLBACK", "launchd did not start the pane's shell; forking it", session = id, err = e);
                Pty::spawn(config)
            }
        }
    }

    pub(super) fn domain() -> String {
        // SAFETY: getuid cannot fail and touches nothing.
        format!("gui/{}", unsafe { libc::getuid() })
    }

    fn spawn_as_job(id: u64, config: &PtyConfig) -> io::Result<Pty> {
        let (master, slave, tty) = open_terminal(config)?;
        let root = marspot_term::paths::state_root();
        let dir = root.join("pane-jobs");
        fs::create_dir_all(&dir)?;
        let label = label_for(root.as_os_str().as_bytes(), id);
        let (spec_path, reply_path, plist_path) =
            (dir.join(format!("{id}.spec")), dir.join(format!("{id}.reply")), dir.join(format!("{id}.plist")));

        let _ = fs::remove_file(&reply_path);
        let reply_c = CString::new(reply_path.as_os_str().as_bytes()).map_err(io::Error::other)?;
        // SAFETY: a valid C string; mkfifo only creates the node.
        if unsafe { libc::mkfifo(reply_c.as_ptr(), 0o600) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // both ends, before the host exists: its write never waits for
        // a reader, and our read never sees an end of file
        let mut reply = fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(&reply_path)?;

        let mut argv = vec![config.argv0.clone().unwrap_or_else(|| config.program.clone()).into_bytes()];
        argv.extend(config.args.iter().map(|a| a.clone().into_bytes()));
        let spec = Spec {
            session: std::process::id() as i32,
            tty,
            reply: reply_path.as_os_str().as_bytes().to_vec(),
            label: label.clone().into_bytes(),
            cwd: config.cwd.clone().unwrap_or_default().into_bytes(),
            program: config.program.clone().into_bytes(),
            argv,
            env: child_env(config, std::env::vars_os().map(|(k, v)| (k.into_vec(), v.into_vec()))),
        };
        // the environment can hold secrets: the file is ours alone and
        // the host removes it as soon as it has read it
        fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&spec_path)?.write_all(&spec.encode())?;
        let exe = std::env::current_exe()?;
        let state_dir = std::env::var("MARSPOT_STATE_DIR").ok();
        fs::write(&plist_path, plist(&label, &exe.to_string_lossy(), &spec_path.to_string_lossy(), state_dir.as_deref()))?;

        let begun = Instant::now();
        let out = Command::new(LAUNCHCTL).arg("bootstrap").arg(domain()).arg(&plist_path).stdin(Stdio::null()).output()?;
        let _ = fs::remove_file(&plist_path);
        let started = if out.status.success() {
            let pid = read_reply(&mut reply);
            if pid.is_err() {
                remove_job(&label);
            }
            pid
        } else {
            Err(io::Error::other(format!("launchctl bootstrap: {}", String::from_utf8_lossy(&out.stderr).trim())))
        };
        let _ = fs::remove_file(&reply_path);
        let shell = started.inspect_err(|_| {
            let _ = fs::remove_file(&spec_path);
        })?;
        // the shell holds the terminal open now; ours would keep a
        // hangup from ever reaching us
        drop(slave);
        lx_event!(
            "PANE_JOB_STARTED",
            "the pane's shell runs as a launchd job",
            session = id,
            label = label,
            shell_pid = shell,
            shell_coalition = coalition_of(shell),
            own_coalition = coalition_of(std::process::id() as i32),
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

    fn read_reply(reply: &mut fs::File) -> io::Result<i32> {
        let deadline = Instant::now() + HOST_WAIT;
        let mut line = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let mut pfd = libc::pollfd { fd: reply.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one pollfd, on a descriptor that is open.
            let n = unsafe { libc::poll(&mut pfd, 1, left.as_millis() as libc::c_int) };
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "the pane host did not answer"));
            }
            let mut buf = [0u8; 256];
            match reply.read(&mut buf) {
                Ok(k) => line.extend_from_slice(&buf[..k]),
                Err(e) if matches!(e.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => {}
                Err(e) => return Err(e),
            }
            if line.contains(&b'\n') {
                return parse_reply(&String::from_utf8_lossy(&line));
            }
        }
    }

    fn remove_job(label: &str) {
        let _ = Command::new(LAUNCHCTL).arg("bootout").arg(format!("{}/{label}", domain())).stdin(Stdio::null()).output();
    }

    /// The jetsam coalition `pid` is in, 0 when the system will not say.
    /// Read for the log line only.
    fn coalition_of(pid: i32) -> u64 {
        // `PROC_PIDCOALITIONINFO` and its struct, from the system's
        // private half of <sys/proc_info.h>
        const FLAVOR: libc::c_int = 20;
        #[repr(C)]
        #[derive(Default)]
        struct Info {
            id: [u64; 2],
            reserved: [u64; 3],
        }
        let mut info = Info::default();
        let size = std::mem::size_of::<Info>() as libc::c_int;
        // SAFETY: the buffer is `size` bytes; an unknown flavor fails
        // without writing.
        let n = unsafe { libc::proc_pidinfo(pid, FLAVOR, 0, (&raw mut info).cast(), size) };
        if n == size { info.id[1] } else { 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spec_reads_back_as_written() {
        let spec = Spec {
            session: 4242,
            tty: b"/dev/ttys012".to_vec(),
            reply: b"/tmp/x/7.reply".to_vec(),
            label: b"com.marspot.pane.0000beef.7".to_vec(),
            cwd: Vec::new(),
            program: b"/bin/zsh".to_vec(),
            argv: vec![b"-zsh".to_vec()],
            env: vec![b"A=1".to_vec(), b"MULTI=line one\nline two".to_vec(), b"EMPTY=".to_vec()],
        };
        assert_eq!(Spec::decode(&spec.encode()), Some(spec));
        assert_eq!(Spec::decode(&Spec::default().encode()), Some(Spec::default()));
    }

    #[test]
    fn a_file_that_is_not_a_spec_is_refused() {
        assert_eq!(Spec::decode(b""), None);
        assert_eq!(Spec::decode(b"marspot-pane-job 2\x007\0a\0b\0c\0d\0e\x000\0"), None);
        assert_eq!(Spec::decode(b"marspot-pane-job 1\x007\0a\0b\0c\0d\0e\x003\0only-one\0"), None);
        assert_eq!(Spec::decode(b"marspot-pane-job 1\0not-a-pid\0a\0b\0c\0d\0e\x000\0"), None);
    }

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

    #[test]
    fn two_state_directories_never_share_a_label() {
        assert_eq!(label_for(b"/tmp/a", 7), label_for(b"/tmp/a", 7));
        assert_ne!(label_for(b"/tmp/a", 7), label_for(b"/tmp/b", 7));
        assert_ne!(label_for(b"/tmp/a", 7), label_for(b"/tmp/a", 8));
        assert!(label_for(b"/tmp/a", 7).starts_with("com.marspot.pane."));
    }

    #[test]
    fn a_path_with_markup_in_it_stays_one_string_in_the_plist() {
        let p = plist("l", "/a&b/<c>", "/s", None);
        assert!(p.contains("<string>/a&amp;b/&lt;c&gt;</string>"));
        assert!(p.contains("<string>--pane-host</string>"));
        assert!(!p.contains("EnvironmentVariables"));
        assert!(plist("l", "/e", "/s", Some("/tmp/sb")).contains("<key>MARSPOT_STATE_DIR</key><string>/tmp/sb</string>"));
    }

    #[test]
    fn the_host_answers_with_a_pid_or_a_reason() {
        assert_eq!(parse_reply("pid 4242\n").unwrap(), 4242);
        assert!(parse_reply("err exec: No such file or directory\n").unwrap_err().to_string().contains("No such file"));
        assert!(parse_reply("").is_err());
        assert!(parse_reply("pid x\n").is_err());
    }
}

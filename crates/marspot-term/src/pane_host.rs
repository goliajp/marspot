//! The program a pane's launchd job runs: it starts the shell on the
//! pane's terminal, says which pid it is, and exits.
//!
//! Which executable this is matters more than what it does.  The
//! system charges every privacy request and every first run of a new
//! binary under a launchd job to the job's program, and goes on doing
//! so after that program has exited.  When the program is the app's
//! main executable the charge lands on the app, as it does for a shell
//! the app's own processes forked: one entry in the privacy settings,
//! and the grants already given to it.  So the app's executable is a
//! pane host too, and says where it is for the sessions to find.

use std::io;

/// The argument that makes an executable a pane host; the spec file
/// follows it.
pub const ARG: &str = "--pane-host";

/// Where the running app's main executable is, for the sessions it
/// starts.  Unset under an app from before it could be a pane host.
pub const PROGRAM_ENV: &str = "MARSPOT_PANE_HOST";

/// An environment entry that has the host start its program as an
/// application in front, in the scheduler's eyes.  A coalition that
/// holds such a task keeps its threads on the performance cores under
/// contention where one of its own, judged alone, is moved off them;
/// no thread's priority changes.  For a program that is to be measured
/// that way: no pane is started with it.
pub const FRONT_ROLE: &[u8] = b"MARSPOT_FRONT_ROLE=1";

/// Set by an app that replaces its own image with a newer one.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const SELF_UPDATE_ENV: &str = "MARSPOT_SHELL_SELF_UPDATE";

/// The executable a starting app names as the pane host, if any.
///
/// The system knows a process by the executable it was launched as,
/// also after it replaced its image: what it forks is still charged to
/// the bundle.  A job is charged to its own program, so the host has to
/// be that first executable.  An image that took over from one which
/// had named it leaves the name alone.  One that took over from an
/// image too old to name anything names nothing: the bundle's file is
/// that old image and cannot host, this one is outside the bundle, and
/// the panes are forked until the app is started afresh.
// off macOS nothing starts a job: this is built for its tests only
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn program_to_name(named: bool, replaced_an_image: bool, own: Option<std::path::PathBuf>) -> Option<std::path::PathBuf> {
    if named || replaced_an_image { None } else { own }
}

/// What the host needs to start the shell, as the file it is handed.
#[derive(Debug, PartialEq, Eq, Default)]
pub struct Spec {
    pub tty: Vec<u8>,
    pub reply: Vec<u8>,
    pub cwd: Vec<u8>,
    pub program: Vec<u8>,
    /// `resource:soft:hard`, comma separated: launchd starts a job
    /// with limits of its own (256 open files), not the session's.
    pub limits: Vec<u8>,
    pub argv: Vec<Vec<u8>>,
    pub env: Vec<Vec<u8>>,
}

const SPEC_MAGIC: &[u8] = b"marspot-pane-job 2";

impl Spec {
    /// NUL-terminated fields: none of them can hold a NUL, and an
    /// environment value can hold anything else.
    pub fn encode(&self) -> Vec<u8> {
        let argc = self.argv.len().to_string().into_bytes();
        let head = [SPEC_MAGIC, &self.tty, &self.reply, &self.cwd, &self.program, &self.limits, &argc];
        let mut out = Vec::new();
        for field in head.into_iter().chain(self.argv.iter().map(Vec::as_slice)).chain(self.env.iter().map(Vec::as_slice)) {
            out.extend_from_slice(field);
            out.push(0);
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Spec> {
        let mut fields = bytes.strip_suffix(&[0])?.split(|b| *b == 0).map(<[u8]>::to_vec);
        if fields.next()? != SPEC_MAGIC {
            return None;
        }
        let (tty, reply, cwd, program, limits) = (fields.next()?, fields.next()?, fields.next()?, fields.next()?, fields.next()?);
        let argc: usize = std::str::from_utf8(&fields.next()?).ok()?.parse().ok()?;
        let mut argv = Vec::new();
        for _ in 0..argc {
            argv.push(fields.next()?);
        }
        Some(Spec { tty, reply, cwd, program, limits, argv, env: fields.collect() })
    }
}

/// The host's answer: the shell's pid, or why there is none.
pub fn parse_reply(line: &str) -> io::Result<i32> {
    match line.trim_end().split_once(' ') {
        Some(("pid", n)) => n.parse().map_err(|_| io::Error::other(format!("pane host said {line:?}"))),
        _ => Err(io::Error::other(format!("pane host: {}", line.trim_end()))),
    }
}

// off macOS nothing starts a job: this is built for its tests only
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_limits(text: &[u8]) -> Vec<(i32, u64, u64)> {
    let field = |f: Option<&str>| f?.parse::<u64>().ok();
    String::from_utf8_lossy(text)
        .split(',')
        .filter_map(|one| {
            let mut f = one.split(':');
            Some((field(f.next())? as i32, field(f.next())?, field(f.next())?))
        })
        .collect()
}

#[cfg(target_os = "macos")]
pub use macos::{dispatch, own_limits, run};

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::CString;
    use std::fs;
    use std::io::{self, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    use super::{ARG, FRONT_ROLE, PROGRAM_ENV, SELF_UPDATE_ENV, Spec, parse_limits, program_to_name};

    const LIMITS: [libc::c_int; 9] = [
        libc::RLIMIT_CPU,
        libc::RLIMIT_FSIZE,
        libc::RLIMIT_DATA,
        libc::RLIMIT_STACK,
        libc::RLIMIT_CORE,
        libc::RLIMIT_AS,
        libc::RLIMIT_MEMLOCK,
        libc::RLIMIT_NPROC,
        libc::RLIMIT_NOFILE,
    ];

    /// This process's resource limits, as a spec carries them.
    pub fn own_limits() -> Vec<u8> {
        let mut out = Vec::new();
        for res in LIMITS {
            let mut l = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            // SAFETY: fills the struct it is given.
            if unsafe { libc::getrlimit(res, &mut l) } == 0 {
                out.push(format!("{res}:{}:{}", l.rlim_cur, l.rlim_max));
            }
        }
        out.join(",").into_bytes()
    }

    /// First thing in a program that can be a pane host: be one when
    /// asked to; otherwise leave word of where this program is for the
    /// sessions started under it.
    pub fn dispatch() {
        let mut args = std::env::args_os().skip(1);
        if args.next().is_some_and(|a| a == ARG) {
            if let Some(spec) = args.next() {
                run(Path::new(&spec));
            }
            std::process::exit(0);
        }
        let named = std::env::var_os(PROGRAM_ENV).is_some();
        let replaced = std::env::var_os(SELF_UPDATE_ENV).is_some();
        if let Some(exe) = program_to_name(named, replaced, std::env::current_exe().ok()) {
            // SAFETY: the first statement of main; no other thread exists.
            unsafe { std::env::set_var(PROGRAM_ENV, exe) };
        }
    }

    /// The job: start the shell on the pane's terminal and say which
    /// pid it is.
    pub fn run(spec_path: &Path) {
        let Ok(bytes) = fs::read(spec_path) else { return };
        let _ = fs::remove_file(spec_path);
        let Some(spec) = Spec::decode(&bytes) else { return };

        let mut ends = [0 as libc::c_int; 2];
        // SAFETY: pipe fills the two descriptors; the write end closes
        // on exec, which is how the parent learns the exec happened, and
        // the shell is not to inherit either.  The limits are this
        // process's own to set, and the shell inherits them.
        unsafe {
            for (res, cur, max) in parse_limits(&spec.limits) {
                libc::setrlimit(res, &libc::rlimit { rlim_cur: cur, rlim_max: max });
            }
            libc::pipe(ends.as_mut_ptr());
            libc::fcntl(ends[0], libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(ends[1], libc::F_SETFD, libc::FD_CLOEXEC);
        }
        // SAFETY: this process has one thread, so the child may do
        // anything a process may.
        let child = unsafe { libc::fork() };
        if child == 0 {
            let err = exec_shell(&spec);
            // SAFETY: writes the errno to a pipe this process owns.
            unsafe {
                libc::write(ends[1], err.to_ne_bytes().as_ptr().cast(), 4);
                libc::_exit(127);
            }
        }
        let answer = if child < 0 {
            format!("err fork: {}\n", io::Error::last_os_error())
        } else {
            let mut errno = [0u8; 4];
            // SAFETY: closes our copy of the write end, then reads at
            // most four bytes into a four-byte buffer.
            let n = unsafe {
                libc::close(ends[1]);
                libc::read(ends[0], errno.as_mut_ptr().cast(), 4)
            };
            if n == 4 { format!("err exec: {}\n", io::Error::from_raw_os_error(i32::from_ne_bytes(errno))) } else { format!("pid {child}\n") }
        };
        // without waiting: a session that is gone is not reading
        let reply = Path::new(std::ffi::OsStr::from_bytes(&spec.reply));
        if let Ok(mut reply) = fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(reply) {
            let _ = reply.write_all(answer.as_bytes());
        }
    }

    fn take_front_role() {
        // `TASK_CATEGORY_POLICY` and `TASK_FOREGROUND_APPLICATION` from
        // <mach/task_policy.h>
        const TASK_CATEGORY_POLICY: u32 = 1;
        const TASK_FOREGROUND_APPLICATION: i32 = 1;
        unsafe extern "C" {
            fn task_policy_set(task: libc::mach_port_t, flavor: u32, policy: *const i32, count: u32) -> i32;
        }
        let role = TASK_FOREGROUND_APPLICATION;
        // libc marks its Mach bindings deprecated in favour of the
        // mach2 crate; one call does not earn a dependency
        #[allow(deprecated)]
        // SAFETY: the flavor reads one integer, the role, from
        // `policy`; the task is this one.
        unsafe {
            task_policy_set(libc::mach_task_self(), TASK_CATEGORY_POLICY, &role, 1);
        }
    }

    /// In the forked child: become the shell.  Returns the errno of
    /// whatever stopped it.
    fn exec_shell(spec: &Spec) -> i32 {
        let c = |b: &[u8]| CString::new(b).unwrap_or_default();
        let (tty, program, cwd) = (c(&spec.tty), c(&spec.program), c(&spec.cwd));
        let argv: Vec<CString> = spec.argv.iter().map(|a| c(a)).collect();
        let env: Vec<CString> = spec.env.iter().map(|e| c(e)).collect();
        let mut argv_p: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
        argv_p.push(std::ptr::null());
        let mut env_p: Vec<*mut libc::c_char> = env.iter().map(|e| e.as_ptr().cast_mut()).collect();
        env_p.push(std::ptr::null_mut());
        // SAFETY: the single thread of a freshly forked process; every
        // pointer is to a NUL-terminated string or array that lives
        // until the exec.
        unsafe {
            libc::setsid();
            // the first terminal a session leader opens becomes its
            // controlling terminal
            let fd = libc::open(tty.as_ptr(), libc::O_RDWR);
            if fd < 0 {
                return *libc::__error();
            }
            for std_fd in 0..3 {
                libc::dup2(fd, std_fd);
            }
            if fd > 2 {
                libc::close(fd);
            }
            if !spec.cwd.is_empty() {
                libc::chdir(cwd.as_ptr());
            }
            // rust ignores SIGPIPE and an ignored signal survives exec
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
            // the role is the task's and stays with it through the exec
            if spec.env.iter().any(|e| e == FRONT_ROLE) {
                take_front_role();
            }
            // execvp searches the PATH of the environment it is given
            crate::platform::set_environ(env_p.as_mut_ptr());
            libc::execvp(program.as_ptr(), argv_p.as_ptr());
            *libc::__error()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spec_reads_back_as_written() {
        let spec = Spec {
            tty: b"/dev/ttys012".to_vec(),
            reply: b"/tmp/x/7.reply".to_vec(),
            cwd: Vec::new(),
            program: b"/bin/zsh".to_vec(),
            limits: b"8:2560:9223372036854775807".to_vec(),
            argv: vec![b"-zsh".to_vec()],
            env: vec![b"A=1".to_vec(), b"MULTI=line one\nline two".to_vec(), b"EMPTY=".to_vec()],
        };
        assert_eq!(Spec::decode(&spec.encode()), Some(spec));
        assert_eq!(Spec::decode(&Spec::default().encode()), Some(Spec::default()));
    }

    #[test]
    fn a_file_that_is_not_a_spec_is_refused() {
        assert_eq!(Spec::decode(b""), None);
        assert_eq!(Spec::decode(b"marspot-pane-job 1\0a\0b\0c\0d\x000\0"), None);
        assert_eq!(Spec::decode(b"marspot-pane-job 2\0a\0b\0c\0d\0\x003\0only-one\0"), None);
        assert_eq!(Spec::decode(b"marspot-pane-job 2\0a\0b\0c\0d\0\0not-a-count\0"), None);
    }

    #[test]
    fn limits_are_read_one_resource_at_a_time_and_a_bad_one_is_skipped() {
        assert_eq!(parse_limits(b"8:2560:9223372036854775807,3:8372224:67104768"), vec![(8, 2560, i64::MAX as u64), (3, 8372224, 67104768)]);
        assert_eq!(parse_limits(b"8:x:1,7:1:2"), vec![(7, 1, 2)]);
        assert!(parse_limits(b"").is_empty());
    }

    #[test]
    fn an_app_names_the_executable_it_was_launched_as_and_no_other() {
        let own = || Some(std::path::PathBuf::from("/A/Marspot.app/Contents/MacOS/marspot-shell"));
        assert_eq!(program_to_name(false, false, own()), own());
        assert_eq!(program_to_name(true, false, own()), None, "an earlier image's word stands");
        assert_eq!(program_to_name(true, true, own()), None);
        assert_eq!(program_to_name(false, true, own()), None, "the bundle's file is the old image");
    }

    #[test]
    fn the_host_answers_with_a_pid_or_a_reason() {
        assert_eq!(parse_reply("pid 4242\n").unwrap(), 4242);
        assert!(parse_reply("err exec: No such file or directory\n").unwrap_err().to_string().contains("No such file"));
        assert!(parse_reply("").is_err());
        assert!(parse_reply("pid x\n").is_err());
    }
}

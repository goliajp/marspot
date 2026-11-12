//! The process a pane's job runs: it starts the shell on the pane's
//! terminal, says which pid it is, outlives it, and takes the job off
//! launchd's list.

use std::ffi::CString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use super::Spec;
use super::macos::{LAUNCHCTL, domain};

/// The job: start the shell on the pane's terminal, say which pid
/// it is, outlive it, then take the job off launchd's list.
pub(crate) fn host_main() {
    let Some(spec_path) = std::env::args_os().nth(2) else { return };
    let Ok(bytes) = fs::read(&spec_path) else { return };
    let _ = fs::remove_file(&spec_path);
    let Some(spec) = Spec::decode(&bytes) else { return };
    ignore_strangers_sigterm();

    let mut ends = [0 as libc::c_int; 2];
    // SAFETY: pipe fills the two descriptors; the write end closes
    // on exec, which is how the parent learns the exec happened, and
    // the shell is not to inherit either.
    unsafe {
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
    if child > 0 {
        outlive(child, spec.session);
    }
    // a job stays on launchd's list after its process is gone;
    // asking for the removal is the last thing this process does
    use std::os::unix::process::CommandExt;
    let _ = Command::new(LAUNCHCTL).arg("bootout").arg(format!("{}/{}", domain(), String::from_utf8_lossy(&spec.label))).exec();
}

/// Return once the shell is gone.  A session that ends first has
/// hung the terminal up; a shell that does not leave on that is
/// killed, as the session did while the shell was its own child.
fn outlive(shell: i32, session: i32) {
    let watch = |pid: i32| libc::kevent {
        ident: pid as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD,
        fflags: libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    // SAFETY: kevent reads two changes and writes at most one
    // event, all in buffers of that size; waitpid and kill name the
    // one child this process has.
    unsafe {
        let kq = libc::kqueue();
        let changes = [watch(shell), watch(session)];
        let mut event = watch(0);
        // a session that cannot be watched is one that is gone
        let session_gone = libc::kevent(kq, changes.as_ptr(), 2, std::ptr::null_mut(), 0, std::ptr::null()) < 0
            || loop {
                if libc::kevent(kq, std::ptr::null(), 0, &mut event, 1, std::ptr::null()) == 1 {
                    break event.ident == session as libc::uintptr_t;
                }
            };
        let mut status = 0;
        if session_gone {
            let deadline = Instant::now() + Duration::from_millis(500);
            while libc::waitpid(shell, &mut status, libc::WNOHANG) == 0 {
                if Instant::now() >= deadline {
                    libc::kill(shell, libc::SIGKILL);
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        while libc::waitpid(shell, &mut status, 0) < 0 && *libc::__error() == libc::EINTR {}
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
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
        let mut none: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut none);
        libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
        // execvp searches the PATH of the environment it is given
        marspot_term::platform::set_environ(env_p.as_mut_ptr());
        libc::execvp(program.as_ptr(), argv_p.as_ptr());
        *libc::__error()
    }
}

/// A force quit of something this pane started sends SIGTERM to
/// everything in the pane's coalition, the host included.  Only
/// launchd's ends it: the job is being removed.
fn ignore_strangers_sigterm() {
    extern "C" fn on_term(_sig: libc::c_int, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
        // SAFETY: the kernel hands an SA_SIGINFO handler a valid
        // siginfo; _exit is async-signal-safe.
        unsafe {
            if (*info).si_pid() == 1 {
                libc::_exit(0);
            }
        }
    }
    // SAFETY: installs a handler that only reads its argument and
    // may call _exit.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_term as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESTART | libc::SA_SIGINFO;
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
    }
}

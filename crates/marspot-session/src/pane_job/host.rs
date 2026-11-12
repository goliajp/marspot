//! The process a pane's job runs: it starts the shell on the pane's
//! terminal, says which pid it is, and exits.

use std::ffi::CString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use super::Spec;

/// The job: start the shell on the pane's terminal and say which pid
/// it is.
pub(crate) fn host_main() {
    let Some(spec_path) = std::env::args_os().nth(2) else { return };
    let Ok(bytes) = fs::read(&spec_path) else { return };
    let _ = fs::remove_file(&spec_path);
    let Some(spec) = Spec::decode(&bytes) else { return };

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
        // execvp searches the PATH of the environment it is given
        marspot_term::platform::set_environ(env_p.as_mut_ptr());
        libc::execvp(program.as_ptr(), argv_p.as_ptr());
        *libc::__error()
    }
}

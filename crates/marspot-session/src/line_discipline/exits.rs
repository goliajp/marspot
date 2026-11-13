//! How each foreground job ended, from kqueue.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Ending {
    Clean,
    Signalled,
    Failed,
    Unknown,
}

/// `PROC_FLAG_EXEC` of <sys/proc_info.h>: the process has called exec.
const HAS_EXECED: u32 = 0x4000;

/// Whether `pid` is still the image its parent forked: it has called
/// exec at no time. The kernel keeps that per process, so it does not
/// matter which program the parent or the child is.
pub(super) fn not_execed(pid: libc::pid_t) -> bool {
    info(pid).is_some_and(|i| i.pbi_flags & HAS_EXECED == 0)
}

/// What the kernel keeps about `pid`; `None` once it is gone.
fn info(pid: libc::pid_t) -> Option<libc::proc_bsdinfo> {
    // SAFETY: an all-zero proc_bsdinfo is a valid value to fill.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the buffer is `size` bytes; a process that is gone fails
    // without writing.
    let got = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    (got == size).then_some(info)
}

/// A kqueue the foreground leaders' exits arrive on, and the last
/// leader's ending once it has been read from it.
#[derive(Default)]
pub(super) struct Exits(Option<OwnedFd>, Option<(libc::pid_t, Ending)>);

impl Exits {
    /// Whether `pid` is now watched: it had not exited yet.
    pub(super) fn watch(&mut self, pid: libc::pid_t) -> bool {
        if self.0.is_none() {
            // SAFETY: kqueue returns a new descriptor or -1.
            let kq = unsafe { libc::kqueue() };
            if kq < 0 {
                return false;
            }
            // SAFETY: `kq` is a descriptor nothing else owns, and from
            // here on this field does.
            self.0 = Some(unsafe {
                libc::fcntl(kq, libc::F_SETFD, libc::FD_CLOEXEC);
                OwnedFd::from_raw_fd(kq)
            });
        }
        let Some(kq) = self.0.as_ref() else { return false };
        // SAFETY: an all-zero kevent is a valid value.
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        ev.ident = pid as libc::uintptr_t;
        ev.filter = libc::EVFILT_PROC;
        ev.flags = libc::EV_ADD | libc::EV_ONESHOT;
        ev.fflags = libc::NOTE_EXIT | libc::NOTE_EXITSTATUS;
        // refused for a process this user may not signal, or one
        // already gone: its ending then reads as unknown
        // SAFETY: one change, no events asked for.
        unsafe { libc::kevent(kq.as_raw_fd(), &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) == 0 }
    }

    /// How `leader` ended, from the exits queued so far. The exit is
    /// queued before the shell can take the terminal back: the shell
    /// only does that after reaping it.
    pub(super) fn ending_of(&mut self, leader: libc::pid_t) -> Ending {
        let Some(kq) = self.0.as_ref() else { return Ending::Unknown };
        let zero = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        loop {
            // SAFETY: an all-zero kevent is a valid value.
            let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
            // SAFETY: room for the one event asked for, and a zero timeout.
            let n = unsafe { libc::kevent(kq.as_raw_fd(), std::ptr::null(), 0, &mut ev, 1, &zero) };
            if n != 1 {
                return self.1.filter(|(pid, _)| *pid == leader).map_or(Ending::Unknown, |(_, ending)| ending);
            }
            if ev.ident == leader as libc::uintptr_t {
                let status = ev.data as libc::c_int;
                let ending = if libc::WIFSIGNALED(status) {
                    Ending::Signalled
                } else if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
                    Ending::Clean
                } else {
                    Ending::Failed
                };
                self.1 = Some((leader, ending));
            }
        }
    }
}

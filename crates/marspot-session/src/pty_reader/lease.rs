//! How many panes read bulk output at the same moment, machine-wide.
//!
//! A pty on macOS hands over one kilobyte at a time: the writing
//! program sleeps after each and the reader wakes it.  With four or
//! more panes doing that at once the kernel's pty path stops scaling
//! and their combined rate falls -- nine sessions absorbing the same
//! corpus carried 110 MB/s together while the nine `cat`s spent 77 s
//! of system time, and 223 MB/s with 6.5 s once no more than two of
//! them were reading at any moment.  Two panes at once is also where
//! the combined rate peaks.
//!
//! So a reader that finds itself in bulk output takes one of two
//! leases, keeps it for a slice, and hands it on.  A lease is a byte
//! range locked in a file every session of this user opens; the kernel
//! drops it with the process, so a session that dies mid-slice does not
//! keep one.  Each lease has a gate in front of it: whoever waits for
//! the lease holds its gate, and a reader that has just given the lease
//! up has to pass the gate again, behind them.

use std::fs::File;
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;
use std::time::{Duration, Instant};

/// Readers in bulk output at once.
const SLOTS: usize = 2;

/// Longest a reader at the front of the queue waits for the lease.
/// Nobody can take it ahead of that reader, so this passing means the
/// holder kept it many slices long -- a stopped process -- and output
/// delayed further is worse than the rate the limit protects: the
/// reader goes on without a lease.
const TURN_WAIT: Duration = Duration::from_millis(50);

/// Longest a reader waits to get to the front.  The kernel wakes
/// everyone queued at a gate and one of them wins, in no order, so the
/// wait has a long tail and a short limit sends readers off without a
/// lease: with 50 ms for gate and lease together nine flooding panes
/// carried 120 to 131 MB/s, with this 223.
const GATE_WAIT: Duration = Duration::from_millis(250);

/// How long a reader that gave up waiting leaves the leases alone.
const BACK_OFF: Duration = Duration::from_secs(1);

/// `fcntl` command: `F_SETLKW` that gives up after a time.
const F_SETLKWTIMEOUT: libc::c_int = 10;

/// Argument of [`F_SETLKWTIMEOUT`], `struct flocktimeout`.
#[repr(C)]
struct FlockTimeout {
    fl: libc::flock,
    timeout: libc::timespec,
}

pub(crate) struct Leases {
    file: Option<File>,
    /// Which lease to queue for when none is free, moved on each time.
    next: usize,
    back_off_until: Option<Instant>,
}

/// One reader's turn; given up when dropped.
pub(crate) struct Lease {
    fd: RawFd,
    slot: usize,
}

impl Leases {
    /// The leases every session of this user shares.
    pub(crate) fn open() -> Self {
        Self::at(&std::env::temp_dir().join("marspot-bulk-read.lock"))
    }

    /// Without the file there is nothing to share and no limit.
    pub(crate) fn at(path: &Path) -> Self {
        let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path).ok();
        Self { file, next: std::process::id() as usize, back_off_until: None }
    }

    /// A lease if one is free right now.
    pub(crate) fn try_take(&mut self) -> Option<Lease> {
        let fd = self.usable()?;
        (0..SLOTS).find_map(|i| {
            let slot = (self.next + i) % SLOTS;
            if !lock(fd, gate(slot), None) {
                return None;
            }
            let got = lock(fd, turn(slot), None);
            unlock(fd, gate(slot));
            got.then_some(Lease { fd, slot })
        })
    }

    /// A lease, queueing for one if none is free.
    pub(crate) fn take(&mut self) -> Option<Lease> {
        if let Some(lease) = self.try_take() {
            return Some(lease);
        }
        let fd = self.usable()?;
        self.next = self.next.wrapping_add(1);
        let slot = self.next % SLOTS;
        if lock(fd, gate(slot), Some(GATE_WAIT)) {
            let got = lock(fd, turn(slot), Some(TURN_WAIT));
            unlock(fd, gate(slot));
            if got {
                return Some(Lease { fd, slot });
            }
        }
        self.back_off_until = Some(Instant::now() + BACK_OFF);
        None
    }

    fn usable(&self) -> Option<RawFd> {
        let fd = self.file.as_ref()?.as_raw_fd();
        match self.back_off_until {
            Some(until) if Instant::now() < until => None,
            _ => Some(fd),
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        unlock(self.fd, turn(self.slot));
    }
}

fn gate(slot: usize) -> libc::off_t {
    2 * slot as libc::off_t
}

fn turn(slot: usize) -> libc::off_t {
    2 * slot as libc::off_t + 1
}

fn range(byte: libc::off_t, kind: libc::c_int) -> libc::flock {
    libc::flock { l_start: byte, l_len: 1, l_pid: 0, l_type: kind as libc::c_short, l_whence: libc::SEEK_SET as libc::c_short }
}

/// Lock one byte of `fd` for this process; `wait` is how long to queue
/// for it, not at all when `None`.
fn lock(fd: RawFd, byte: libc::off_t, wait: Option<Duration>) -> bool {
    let fl = range(byte, libc::F_WRLCK as libc::c_int);
    let Some(wait) = wait.filter(|w| !w.is_zero()) else {
        // SAFETY: `fl` is a live flock for the call.
        return unsafe { libc::fcntl(fd, libc::F_SETLK, &fl) } == 0;
    };
    let req = FlockTimeout {
        fl,
        timeout: libc::timespec { tv_sec: wait.as_secs() as libc::time_t, tv_nsec: wait.subsec_nanos() as libc::c_long },
    };
    loop {
        // SAFETY: `req` is a live `struct flocktimeout` for the call.
        if unsafe { libc::fcntl(fd, F_SETLKWTIMEOUT, &req) } == 0 {
            return true;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return false;
        }
    }
}

fn unlock(fd: RawFd, byte: libc::off_t) {
    let fl = range(byte, libc::F_UNLCK as libc::c_int);
    // SAFETY: `fl` is a live flock for the call.
    unsafe { libc::fcntl(fd, libc::F_SETLK, &fl) };
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(crate) fn lock_file(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("marspot-bulk-lease-{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Another process holding every lease until the returned pipe end
    /// is closed.  Locks belong to a process, so it has to be another.
    pub(crate) fn holder(path: &Path) -> (libc::pid_t, RawFd) {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("a path without NUL");
        let (mut ready, mut done) = ([0; 2], [0; 2]);
        // SAFETY: two pipes; the child makes only async-signal-safe
        // calls (open, fcntl, write, read, _exit) before it exits.
        unsafe {
            assert_eq!(libc::pipe(ready.as_mut_ptr()), 0);
            assert_eq!(libc::pipe(done.as_mut_ptr()), 0);
            let pid = libc::fork();
            assert!(pid >= 0, "fork");
            if pid == 0 {
                libc::close(done[1]);
                let fd = libc::open(path.as_ptr(), libc::O_RDWR);
                for slot in 0..SLOTS {
                    lock(fd, turn(slot), None);
                }
                libc::write(ready[1], [1u8].as_ptr().cast(), 1);
                let mut byte = 0u8;
                libc::read(done[0], (&mut byte as *mut u8).cast(), 1);
                libc::_exit(0);
            }
            libc::close(done[0]);
            let mut byte = 0u8;
            assert_eq!(libc::read(ready[0], (&mut byte as *mut u8).cast(), 1), 1, "the holder never locked");
            libc::close(ready[0]);
            libc::close(ready[1]);
            (pid, done[1])
        }
    }

    pub(crate) fn release(holder: (libc::pid_t, RawFd)) {
        // SAFETY: the pipe end and the child are this test's own.
        unsafe {
            libc::close(holder.1);
            let mut status = 0;
            libc::waitpid(holder.0, &mut status, 0);
        }
    }

    #[test]
    fn a_free_lease_is_had_at_once() {
        let path = lock_file("free");
        let mut leases = Leases::at(&path);
        let started = Instant::now();
        assert!(leases.take().is_some());
        assert!(started.elapsed() < TURN_WAIT, "nothing to wait for");
        let _ = std::fs::remove_file(path);
    }

    /// Every lease held elsewhere: no lease without waiting, a bounded
    /// wait with it, and after giving up no more waiting for a while.
    #[test]
    fn a_reader_gives_up_on_leases_nobody_hands_on() {
        let path = lock_file("held");
        let mut leases = Leases::at(&path);
        let other = holder(&path);
        assert!(leases.try_take().is_none());
        let started = Instant::now();
        assert!(leases.take().is_none());
        let waited = started.elapsed();
        assert!(waited >= TURN_WAIT && waited < TURN_WAIT * 20, "waited {waited:?}");
        let started = Instant::now();
        assert!(leases.take().is_none());
        assert!(started.elapsed() < TURN_WAIT / 2, "backing off, not waiting again");
        release(other);
        let _ = std::fs::remove_file(path);
    }

    /// The kernel gives a dead holder's leases back.
    #[test]
    fn the_leases_of_a_process_that_ended_are_free() {
        let path = lock_file("ended");
        let mut leases = Leases::at(&path);
        let other = holder(&path);
        assert!(leases.try_take().is_none());
        release(other);
        assert!(leases.try_take().is_some());
        let _ = std::fs::remove_file(path);
    }

    /// Whoever waits for a lease holds its gate, so the reader that
    /// gives the lease up cannot take it again ahead of them.
    #[test]
    fn a_waiter_is_served_before_the_one_who_just_held() {
        use std::os::unix::ffi::OsStrExt;
        let path = lock_file("turns");
        let leases = Leases::at(&path);
        let fd = leases.file.as_ref().expect("the lock file").as_raw_fd();
        assert!(lock(fd, turn(0), None));
        let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("a path without NUL");
        let mut got = [0; 2];
        // SAFETY: as in `holder`.
        let pid = unsafe {
            assert_eq!(libc::pipe(got.as_mut_ptr()), 0);
            let pid = libc::fork();
            assert!(pid >= 0, "fork");
            if pid == 0 {
                let fd = libc::open(cpath.as_ptr(), libc::O_RDWR);
                let wait = Some(Duration::from_secs(5));
                let ok = lock(fd, gate(0), wait) && lock(fd, turn(0), wait);
                libc::write(got[1], [ok as u8].as_ptr().cast(), 1);
                libc::_exit(0);
            }
            libc::close(got[1]);
            pid
        };
        // until the other process is queued behind the gate
        let deadline = Instant::now() + Duration::from_secs(5);
        while lock(fd, gate(0), None) {
            unlock(fd, gate(0));
            assert!(Instant::now() < deadline, "the waiter never took the gate");
            std::thread::sleep(Duration::from_millis(2));
        }
        unlock(fd, turn(0));
        let mut byte = 0u8;
        // SAFETY: the pipe end and the child are this test's own.
        unsafe {
            assert_eq!(libc::read(got[0], (&mut byte as *mut u8).cast(), 1), 1);
            libc::close(got[0]);
            let mut status = 0;
            libc::waitpid(pid, &mut status, 0);
        }
        assert_eq!(byte, 1, "the waiter got its turn");
        let _ = std::fs::remove_file(path);
    }
}

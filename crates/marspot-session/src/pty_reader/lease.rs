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
//!
//! Queueing is a wait inside `fcntl`, and a key typed into the pane
//! must not wait behind it: [`hurry`] ends the wait of this process's
//! reader with a signal to its thread.

use std::cell::UnsafeCell;
use std::fs::File;
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;
use std::sync::Once;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
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

/// Argument of [`F_SETLKWTIMEOUT`], `struct flocktimeout`: a `flock`
/// and a `timespec`.  The one a queued reader waits with is a static
/// and its time limit is atomics, because [`on_hurry`] shortens it: a
/// signal that lands before the reader is inside `fcntl` interrupts
/// nothing, and the limit the kernel then reads has already passed.
#[repr(C)]
struct Queued {
    fl: UnsafeCell<libc::flock>,
    secs: AtomicI64,
    nanos: AtomicI64,
}

// SAFETY: `fl` is written only by the reader thread, between its waits.
unsafe impl Sync for Queued {}

const _: () = assert!(size_of::<Queued>() == size_of::<libc::flock>() + size_of::<libc::timespec>());

static QUEUED: Queued = Queued {
    fl: UnsafeCell::new(libc::flock { l_start: 0, l_len: 0, l_pid: 0, l_type: 0, l_whence: 0 }),
    secs: AtomicI64::new(0),
    nanos: AtomicI64::new(0),
};

/// The thread waiting with [`QUEUED`], a `pthread_t`; 0 while none is.
static WAITER: AtomicUsize = AtomicUsize::new(0);

/// What [`hurry`] sends.  Ignored by default, so an image that has no
/// handler for it yet loses nothing to one, and the pane's program
/// does not inherit the handler across its `exec`.
const HURRY: libc::c_int = libc::SIGURG;

/// A time limit that has passed when the kernel looks at it.
const NO_TIME: (i64, i64) = (0, 1);

extern "C" fn on_hurry(_: libc::c_int) {
    QUEUED.secs.store(NO_TIME.0, Ordering::SeqCst);
    QUEUED.nanos.store(NO_TIME.1, Ordering::SeqCst);
}

fn hurried() -> bool {
    (QUEUED.secs.load(Ordering::SeqCst), QUEUED.nanos.load(Ordering::SeqCst)) == NO_TIME
}

/// End the wait of this process's reader, if it is queued for a lease:
/// its [`Leases::take`] returns without one.  Called once what that
/// `take` was given as `leave` is true, by the thread that would stop
/// the reader.
pub(crate) fn hurry() {
    let waiter = WAITER.load(Ordering::SeqCst);
    if waiter != 0 {
        // SAFETY: a reader is the waiter only inside `take`, and the
        // one thread that could join it is this one.
        unsafe { libc::pthread_kill(waiter as libc::pthread_t, HURRY) };
    }
}

fn hear_hurry() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: a handler that only stores to atomics; without
        // SA_RESTART, so the `fcntl` it interrupts returns.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_hurry as *const () as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(HURRY, &sa, std::ptr::null_mut());
        }
    });
}

/// How a wait in the queue ended.
#[derive(PartialEq)]
enum Wait {
    Got,
    TimedOut,
    Left,
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
    #[cfg(not(test))]
    pub(crate) fn open() -> Self {
        Self::at(&std::env::temp_dir().join("marspot-bulk-read.lock"))
    }

    /// Leases of this reader's own.  A flood in a unit test is not one
    /// of the user's panes: sharing theirs, it queued behind every
    /// other test process in bulk output -- up to 146 ms with fourteen
    /// of them, measured -- and took turns from the panes of whoever
    /// ran the tests.
    #[cfg(test)]
    pub(crate) fn open() -> Self {
        let path = tests::lock_file("own");
        let leases = Self::at(&path);
        let _ = std::fs::remove_file(path);
        leases
    }

    /// Without the file there is nothing to share and no limit.
    pub(crate) fn at(path: &Path) -> Self {
        hear_hurry();
        let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path).ok();
        Self { file, next: std::process::id() as usize, back_off_until: None }
    }

    /// A lease if one is free right now.
    pub(crate) fn try_take(&mut self) -> Option<Lease> {
        let fd = self.usable()?;
        (0..SLOTS).find_map(|i| {
            let slot = (self.next + i) % SLOTS;
            if !lock(fd, gate(slot)) {
                return None;
            }
            let got = lock(fd, turn(slot));
            unlock(fd, gate(slot));
            got.then_some(Lease { fd, slot })
        })
    }

    /// A lease, queueing for one if none is free.  The queue is left
    /// without one once `leave` is true; whoever makes it true calls
    /// [`hurry`].
    pub(crate) fn take(&mut self, leave: impl Fn() -> bool) -> Option<Lease> {
        if let Some(lease) = self.try_take() {
            return Some(lease);
        }
        let fd = self.usable()?;
        self.next = self.next.wrapping_add(1);
        let slot = self.next % SLOTS;
        let mut wait = queue(fd, gate(slot), GATE_WAIT, &leave);
        if wait == Wait::Got {
            wait = queue(fd, turn(slot), TURN_WAIT, &leave);
            unlock(fd, gate(slot));
        }
        match wait {
            Wait::Got => return Some(Lease { fd, slot }),
            Wait::TimedOut => self.back_off_until = Some(Instant::now() + BACK_OFF),
            Wait::Left => {}
        }
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

/// Lock one byte of `fd` for this process if it is free right now.
fn lock(fd: RawFd, byte: libc::off_t) -> bool {
    let fl = range(byte, libc::F_WRLCK as libc::c_int);
    // SAFETY: `fl` is a live flock for the call.
    unsafe { libc::fcntl(fd, libc::F_SETLK, &fl) == 0 }
}

/// Lock one byte of `fd` for this process, waiting for it up to `wait`
/// and not past a [`hurry`] that finds `leave` true.
fn queue(fd: RawFd, byte: libc::off_t, wait: Duration, leave: impl Fn() -> bool) -> Wait {
    // SAFETY: this thread is the one reader, and it is not waiting.
    unsafe { *QUEUED.fl.get() = range(byte, libc::F_WRLCK as libc::c_int) };
    QUEUED.secs.store(wait.as_secs() as i64, Ordering::SeqCst);
    QUEUED.nanos.store(wait.subsec_nanos() as i64, Ordering::SeqCst);
    // SAFETY: names the calling thread.
    WAITER.store(unsafe { libc::pthread_self() } as usize, Ordering::SeqCst);
    // `leave` is looked at only now: a hurry from before this thread
    // was the waiter sent it nothing
    let ended = loop {
        if leave() {
            break Wait::Left;
        }
        // SAFETY: `QUEUED` is laid out as `struct flocktimeout`.
        if unsafe { libc::fcntl(fd, F_SETLKWTIMEOUT, &raw const QUEUED) } == 0 {
            break Wait::Got;
        }
        if hurried() {
            break Wait::Left;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            break Wait::TimedOut;
        }
    };
    WAITER.store(0, Ordering::SeqCst);
    ended
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
                    lock(fd, turn(slot));
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
        assert!(leases.take(|| false).is_some());
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
        assert!(leases.take(|| false).is_none());
        let waited = started.elapsed();
        assert!(waited >= TURN_WAIT && waited < TURN_WAIT * 20, "waited {waited:?}");
        let started = Instant::now();
        assert!(leases.take(|| false).is_none());
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
        assert!(lock(fd, turn(0)));
        let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("a path without NUL");
        let mut got = [0; 2];
        // SAFETY: as in `holder`.
        let pid = unsafe {
            assert_eq!(libc::pipe(got.as_mut_ptr()), 0);
            let pid = libc::fork();
            assert!(pid >= 0, "fork");
            if pid == 0 {
                let fd = libc::open(cpath.as_ptr(), libc::O_RDWR);
                let wait = Duration::from_secs(5);
                let ok = queue(fd, gate(0), wait, || false) == Wait::Got && queue(fd, turn(0), wait, || false) == Wait::Got;
                libc::write(got[1], [ok as u8].as_ptr().cast(), 1);
                libc::_exit(0);
            }
            libc::close(got[1]);
            pid
        };
        // until the other process is queued behind the gate
        let deadline = Instant::now() + Duration::from_secs(5);
        while lock(fd, gate(0)) {
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

    /// Far longer than a test may take: a wait that ends in `Left` was
    /// ended, it did not run out.
    const LONG: Duration = Duration::from_secs(60);

    /// A reader queued behind leases nobody hands on leaves the queue
    /// when it is hurried.
    #[test]
    fn a_hurry_ends_the_wait_of_a_queued_reader() {
        use std::sync::atomic::AtomicBool;
        let path = lock_file("hurried");
        let leases = Leases::at(&path);
        let fd = leases.file.as_ref().expect("the lock file").as_raw_fd();
        let other = holder(&path);
        static LEAVE: AtomicBool = AtomicBool::new(false);
        let waiter = std::thread::spawn(move || queue(fd, turn(0), LONG, || LEAVE.load(Ordering::SeqCst)));
        while WAITER.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        // most likely inside `fcntl` by now; right either way
        std::thread::sleep(Duration::from_millis(20));
        LEAVE.store(true, Ordering::SeqCst);
        hurry();
        assert!(waiter.join().expect("the waiter") == Wait::Left);
        release(other);
        let _ = std::fs::remove_file(path);
    }

    /// The signal can land after the reader last looked whether to
    /// leave and before it is inside `fcntl`, where there is nothing to
    /// interrupt: the wait that then begins is over at once.
    #[test]
    fn a_hurry_just_before_the_wait_begins_is_not_lost() {
        let path = lock_file("early");
        let leases = Leases::at(&path);
        let fd = leases.file.as_ref().expect("the lock file").as_raw_fd();
        let other = holder(&path);
        let landed_after_the_look = || {
            on_hurry(HURRY);
            false
        };
        assert!(queue(fd, turn(0), LONG, landed_after_the_look) == Wait::Left);
        release(other);
        let _ = std::fs::remove_file(path);
    }

    /// Leaving the queue for a key is not giving up on the leases: the
    /// reader queues again for its next turn.
    #[test]
    fn a_reader_that_left_the_queue_does_not_back_off() {
        let path = lock_file("left");
        let mut leases = Leases::at(&path);
        let other = holder(&path);
        assert!(leases.take(|| true).is_none());
        assert!(leases.back_off_until.is_none());
        release(other);
        let _ = std::fs::remove_file(path);
    }
}

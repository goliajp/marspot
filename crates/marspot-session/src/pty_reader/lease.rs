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
//! So a reader that finds itself in bulk output takes a turn, keeps it
//! for a slice, and gives it up.  Turns are had in the order they were
//! asked for.  They used not to be: readers waited at a gate the kernel
//! opened to whichever of them ran first, which is the one that had
//! just been running, and with nine panes flooding a reader waited 2 ms
//! at the median, 115 to 150 ms one time in a hundred and a second at
//! the worst, its pane standing still.
//!
//! A turn is a numbered ticket.  The number comes from a counter in a
//! file every session of this user opens, and holding the ticket is a
//! lock on the byte of that file the number names; the kernel drops it
//! with the process, so a session that dies does not keep a ticket.  A
//! reader may read once the ticket two before its own has been given
//! up -- two readers at once, each waiting for one other and nobody
//! else, so that giving a ticket up wakes exactly the reader that is
//! next.
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
const SLOTS: u64 = 2;

/// Longest a reader waits for its turn.  A hundred panes flooding at
/// once wait this long for theirs; past it the ticket it waits behind
/// is a stopped process's, and output delayed further is worse than
/// the rate the limit protects: the reader goes on without a turn.
const QUEUE_WAIT: Duration = Duration::from_millis(250);

/// How long a reader that gave up waiting leaves the turns alone.
const BACK_OFF: Duration = Duration::from_secs(1);

/// The byte locked while a ticket is issued.  Clear of bytes 0 to 3,
/// which sessions of before these tickets lock for their turns: until
/// they have all been replaced, they and these take turns apart.
const DESK: libc::off_t = 16;

/// Tickets hold a byte each, from here on, their numbers going round.
const RING_AT: libc::off_t = 64;

/// More bytes than there are ever tickets out.
const RING: u64 = 1024;

/// The byte ticket `number` holds.
fn place(number: u64) -> libc::off_t {
    RING_AT + (number % RING) as libc::off_t
}

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

/// End the wait of this process's reader, if it is queued for a turn:
/// its [`Leases::take`] returns without one.  Called once what that
/// `take` was given as `leave` is true, by the thread that stops the
/// reader -- which is why it does this for a key, and when it stops it.
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
    back_off_until: Option<Instant>,
    /// [`QUEUE_WAIT`], unless a test waits longer.
    wait: Duration,
}

/// One reader's turn; given up when dropped.
pub(crate) struct Lease {
    fd: RawFd,
    place: libc::off_t,
}

impl Leases {
    /// The turns every session of this user shares.
    #[cfg(not(test))]
    pub(crate) fn open() -> Self {
        Self::at(&std::env::temp_dir().join("marspot-bulk-read.lock"))
    }

    /// Turns of this reader's own.  A flood in a unit test is not one
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
        let file = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(path).ok();
        Self { file, back_off_until: None, wait: QUEUE_WAIT }
    }

    /// Waits no test outlasts: one that ends was ended, it did not run
    /// out.
    #[cfg(test)]
    pub(crate) fn patient(mut self) -> Self {
        self.wait = tests::LONG;
        self
    }

    /// A turn if one is free right now.
    pub(crate) fn try_take(&mut self) -> Option<Lease> {
        let fd = self.usable()?;
        // whoever is at the desk is there for a few system calls
        if !(0..3).any(|_| lock(fd, DESK)) {
            return None;
        }
        let number = issued(fd);
        let lease = (given_up(fd, place(number.wrapping_sub(SLOTS))) && lock(fd, place(number))).then(|| {
            issue(fd, number);
            Lease { fd, place: place(number) }
        });
        unlock(fd, DESK);
        lease
    }

    /// A turn, queueing for one if none is free.  The queue is left
    /// without one once `leave` is true; whoever makes it true calls
    /// [`hurry`].
    pub(crate) fn take(&mut self, leave: impl Fn() -> bool) -> Option<Lease> {
        if let Some(lease) = self.try_take() {
            return Some(lease);
        }
        let fd = self.usable()?;
        match queue_up(fd, self.wait, &leave) {
            Ok(place) => return Some(Lease { fd, place }),
            Err(Wait::TimedOut) => self.back_off_until = Some(Instant::now() + BACK_OFF),
            Err(_) => {}
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
        unlock(self.fd, self.place);
    }
}

/// Take the next ticket and wait until the ticket two before it has
/// been given up: the byte to hold for the turn, or how the wait ended
/// without one.
fn queue_up(fd: RawFd, wait: Duration, leave: impl Fn() -> bool) -> Result<libc::off_t, Wait> {
    match queue(fd, DESK, wait, &leave) {
        Wait::Got => {}
        ended => return Err(ended),
    }
    let number = issued(fd);
    let mine = place(number);
    // held only by a ticket from once round the ring ago, whose
    // process has stopped
    if !lock(fd, mine) {
        unlock(fd, DESK);
        return Err(Wait::TimedOut);
    }
    issue(fd, number);
    unlock(fd, DESK);
    let before = place(number.wrapping_sub(SLOTS));
    match queue(fd, before, wait, &leave) {
        Wait::Got => {
            unlock(fd, before);
            Ok(mine)
        }
        ended => {
            unlock(fd, mine);
            Err(ended)
        }
    }
}

/// How many tickets have been issued: the number of the next.  Read
/// with the desk held.
fn issued(fd: RawFd) -> u64 {
    let mut number = [0u8; 8];
    // SAFETY: a read into a live buffer of this length; a file that
    // holds no count yet leaves it at zero.
    unsafe { libc::pread(fd, number.as_mut_ptr().cast(), number.len(), 0) };
    u64::from_le_bytes(number)
}

/// Ticket `number` is out.  With the desk held.
fn issue(fd: RawFd, number: u64) {
    let next = (number + 1).to_le_bytes();
    // SAFETY: a write from a live buffer of this length.
    unsafe { libc::pwrite(fd, next.as_ptr().cast(), next.len(), 0) };
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

/// No other process holds this byte of `fd`.
fn given_up(fd: RawFd, byte: libc::off_t) -> bool {
    let mut fl = range(byte, libc::F_WRLCK as libc::c_int);
    // SAFETY: `fl` is a live flock for the call, which writes to it.
    unsafe { libc::fcntl(fd, libc::F_GETLK, &mut fl) == 0 && fl.l_type == libc::F_UNLCK as libc::c_short }
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

    /// Run `turn` in another process on the file at `path` -- locks
    /// belong to a process -- which then tells the returned pipe so and
    /// stays until [`release`]d.  `turn` makes only async-signal-safe
    /// calls: this is the far side of a `fork`.
    fn in_another_process(path: &Path, turn: impl FnOnce(RawFd)) -> (libc::pid_t, RawFd, RawFd) {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("a path without NUL");
        let (mut told, mut done) = ([0; 2], [0; 2]);
        // SAFETY: two pipes; the child makes only async-signal-safe
        // calls (open, fcntl, pread, pwrite, write, read, _exit).
        unsafe {
            assert_eq!(libc::pipe(told.as_mut_ptr()), 0);
            assert_eq!(libc::pipe(done.as_mut_ptr()), 0);
            let pid = libc::fork();
            assert!(pid >= 0, "fork");
            if pid == 0 {
                libc::close(done[1]);
                turn(libc::open(path.as_ptr(), libc::O_RDWR));
                libc::write(told[1], [1u8].as_ptr().cast(), 1);
                let mut byte = 0u8;
                libc::read(done[0], (&mut byte as *mut u8).cast(), 1);
                libc::_exit(0);
            }
            libc::close(done[0]);
            libc::close(told[1]);
            (pid, done[1], told[0])
        }
    }

    /// Until the process on the far end of `told` has had its turn.
    fn had_its_turn(told: RawFd) {
        let mut byte = 0u8;
        // SAFETY: a read of one byte from a pipe end this test owns.
        assert_eq!(unsafe { libc::read(told, (&mut byte as *mut u8).cast(), 1) }, 1, "the other process never had its turn");
    }

    /// What a reader does in a process of its own: queue for a turn,
    /// however long it takes.
    fn queued_reader(fd: RawFd) {
        if queue_up(fd, LONG, || false).is_err() {
            // SAFETY: ends the forked child, which never had its turn.
            unsafe { libc::_exit(1) };
        }
    }

    /// Another process holding every turn until [`release`]d.
    pub(crate) fn holder(path: &Path) -> (libc::pid_t, RawFd) {
        let (pid, done, told) = in_another_process(path, |fd| {
            for _ in 0..SLOTS {
                while !lock(fd, DESK) {}
                let number = issued(fd);
                lock(fd, place(number));
                issue(fd, number);
                unlock(fd, DESK);
            }
        });
        had_its_turn(told);
        // SAFETY: a pipe end this test owns.
        unsafe { libc::close(told) };
        (pid, done)
    }

    pub(crate) fn release(holder: (libc::pid_t, RawFd)) {
        // SAFETY: the pipe end and the child are this test's own.  A
        // byte and not only the end of the pipe: a process forked
        // since holds this end open too.
        unsafe {
            libc::write(holder.1, [1u8].as_ptr().cast(), 1);
            libc::close(holder.1);
            let mut status = 0;
            libc::waitpid(holder.0, &mut status, 0);
        }
    }

    #[test]
    fn a_free_turn_is_had_at_once() {
        let path = lock_file("free");
        let mut leases = Leases::at(&path);
        let started = Instant::now();
        assert!(leases.take(|| false).is_some());
        assert!(started.elapsed() < QUEUE_WAIT, "nothing to wait for");
        let _ = std::fs::remove_file(path);
    }

    /// Two readers at once and no more: the third to ask waits.
    #[test]
    fn two_turns_are_free_and_a_third_is_not() {
        let path = lock_file("two");
        let mut leases = Leases::at(&path);
        // each in a process of its own, as readers are
        let first = in_another_process(&path, queued_reader);
        had_its_turn(first.2);
        let second = in_another_process(&path, queued_reader);
        had_its_turn(second.2);
        assert!(leases.try_take().is_none(), "a third turn was there to take");
        release((first.0, first.1));
        assert!(leases.try_take().is_some(), "a turn given up was not there to take");
        release((second.0, second.1));
        let _ = std::fs::remove_file(path);
    }

    /// Every turn held elsewhere: no turn without waiting, a bounded
    /// wait with it, and after giving up no more waiting for a while.
    #[test]
    fn a_reader_gives_up_on_turns_nobody_gives_up() {
        let path = lock_file("held");
        let mut leases = Leases::at(&path);
        let other = holder(&path);
        assert!(leases.try_take().is_none());
        let started = Instant::now();
        assert!(leases.take(|| false).is_none());
        let waited = started.elapsed();
        assert!(waited >= QUEUE_WAIT && waited < QUEUE_WAIT * 20, "waited {waited:?}");
        let started = Instant::now();
        assert!(leases.take(|| false).is_none());
        assert!(started.elapsed() < QUEUE_WAIT / 2, "backing off, not waiting again");
        release(other);
        let _ = std::fs::remove_file(path);
    }

    /// The kernel gives a dead holder's turns back.
    #[test]
    fn the_turns_of_a_process_that_ended_are_free() {
        let path = lock_file("ended");
        let mut leases = Leases::at(&path);
        let other = holder(&path);
        assert!(leases.try_take().is_none());
        release(other);
        assert!(leases.try_take().is_some());
        let _ = std::fs::remove_file(path);
    }

    /// Readers that queue have their turns in the order they queued,
    /// however many of them there are and whichever of them the kernel
    /// would rather run: each waits for the ticket two before its own
    /// and for nothing else.
    #[test]
    fn turns_are_had_in_the_order_they_were_asked_for() {
        let path = lock_file("order");
        let leases = Leases::at(&path);
        let fd = leases.file.as_ref().expect("the lock file").as_raw_fd();
        let other = holder(&path);
        // six readers queue one after another, behind the two turns
        // held and behind each other
        let readers: Vec<_> = (0..6u64)
            .map(|n| {
                let reader = in_another_process(&path, queued_reader);
                while issued(fd) < SLOTS + n + 1 {
                    std::thread::yield_now();
                }
                reader
            })
            .collect();
        let waiting = |told: RawFd| {
            let mut fds = [libc::pollfd { fd: told, events: libc::POLLIN, revents: 0 }];
            // SAFETY: one valid pollfd on a pipe end this test owns.
            unsafe { libc::poll(fds.as_mut_ptr(), 1, 0) == 0 }
        };
        release(other);
        // the two that asked first read; each one that gives its turn
        // up lets in the one that asked two after it, and nobody else
        let mut seen = vec![false; readers.len()];
        for n in 0..readers.len() {
            for now in n..(n + 2).min(readers.len()) {
                if !std::mem::replace(&mut seen[now], true) {
                    had_its_turn(readers[now].2);
                }
            }
            for later in &readers[(n + 2).min(readers.len())..] {
                assert!(waiting(later.2), "a reader had its turn ahead of reader {n}");
            }
            release((readers[n].0, readers[n].1));
        }
        let _ = std::fs::remove_file(path);
    }

    /// Far longer than a test may take: a wait that ends in `Left` was
    /// ended, it did not run out.
    pub(crate) const LONG: Duration = Duration::from_secs(60);

    /// A reader of this process is in the queue.
    pub(crate) fn someone_queued() -> bool {
        WAITER.load(Ordering::SeqCst) != 0
    }

    /// A reader queued behind turns nobody gives up leaves the queue
    /// when it is hurried.
    #[test]
    fn a_hurry_ends_the_wait_of_a_queued_reader() {
        use std::sync::atomic::AtomicBool;
        let path = lock_file("hurried");
        let leases = Leases::at(&path);
        let fd = leases.file.as_ref().expect("the lock file").as_raw_fd();
        let other = holder(&path);
        static LEAVE: AtomicBool = AtomicBool::new(false);
        let waiter = std::thread::spawn(move || queue(fd, place(0), LONG, || LEAVE.load(Ordering::SeqCst)));
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
        assert!(queue(fd, place(0), LONG, landed_after_the_look) == Wait::Left);
        release(other);
        let _ = std::fs::remove_file(path);
    }

    /// Leaving the queue for a key is not giving up on the turns: the
    /// reader queues again for its next one.
    #[test]
    fn a_reader_that_left_the_queue_does_not_back_off() {
        let path = lock_file("left");
        let mut leases = Leases::at(&path).patient();
        let other = holder(&path);
        // not while its ticket is issued: while it waits with it
        let looked = std::cell::Cell::new(0);
        let leave_behind_a_ticket = || looked.replace(looked.get() + 1) > 0;
        assert!(leases.take(leave_behind_a_ticket).is_none());
        assert!(looked.get() > 1, "the fixture has to leave from behind a ticket");
        assert!(leases.back_off_until.is_none());
        release(other);
        assert!(leases.take(|| false).is_some());
        let _ = std::fs::remove_file(path);
    }
}

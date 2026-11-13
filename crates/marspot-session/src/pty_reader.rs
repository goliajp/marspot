//! The thread that reads a pane's PTY output for the main loop.
//!
//! macOS hands back at most 1024 bytes per `read` of a pty master,
//! whatever the buffer, and the writing program sleeps after every one
//! of them: a bulk `cat` is 130 000 hand-overs for 134 MB, and how the
//! reader takes its side of each sets the pace more than the parser
//! does.  What was measured, one corpus into N sessions at once, MB/s
//! of all of them together:
//!
//! - wait, read once, queue it (`poll` then `read`): 154 / 159 / 162 /
//!   134 for N = 1 / 2 / 4 / 9, with a parser woken for every kilobyte;
//! - read until dry, ask a dry PTY sixteen more times, then wait:
//!   184 / 210 / 114 / 91 -- every read that finds nothing still goes
//!   through the kernel's pty path, and with four panes doing it the
//!   writers spend twice as long in the kernel;
//! - wait in a kqueue, which says how many bytes are there, and read
//!   exactly those: 152 / 187 / 198 / 110.
//!
//! So the reader waits in a kqueue and never reads a PTY it has not
//! been told has bytes, except right after a full kilobyte, when the
//! next one is a few microseconds away and asking again is cheaper
//! than sleeping -- and, while no other pane floods, after any read of
//! a flood.  That asking again is what two panes gain from and four
//! lose to, which is one of the two reasons readers in bulk output take
//! turns, two at a time -- see [`lease`].

use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use marspot_term::pty::Pty;

mod echo;
mod lease;
mod turn;
mod wakes;
use echo::Echo;
use lease::Leases;
use turn::Turn;
use wakes::{Event, Wakes};

/// Most one chunk carries.
const READ_BUF_BYTES: usize = 64 * 1024;

/// Chunks that exist at all: the reader fills one only when the loop
/// has handed one back, so a program that writes faster than the
/// parser reads waits in the PTY instead of filling this process.
const CHUNKS: usize = 4;

/// What one `read` of a pty master returns at most on macOS.
const KERNEL_READ_BYTES: usize = 1024;

/// How many times a reader that has just taken a full kilobyte asks for
/// the next before it goes back to waiting.
const DRY_RETRIES: u32 = 16;

/// Spin-loop hints between two of those asks: about as long as the
/// writer needs to be woken and fill the PTY again.  Asking with no
/// pause carried the same bytes with three times the empty reads.
const RETRY_PAUSE_SPINS: u32 = 64;

/// Longest a chunk is kept open for more.
const GATHER_FOR: Duration = Duration::from_millis(3);

/// How long one wait for more lasts while the parser is busy.
const GATHER_WAIT: Duration = Duration::from_millis(1);

/// How long bulk output may pause before what has been gathered is
/// handed to an idle parser.  Output shorter than one full read never
/// waits.
///
/// A chunk of bulk output is otherwise handed over full, however idle
/// the parser: every chunk is a wake, a parse and a published frame.
/// Nine panes writing at once leave each writer waiting for a core,
/// and their output arrives a hundred bytes at a time -- handed over as
/// it came the nine cost 405 instructions a byte against 200.  And a
/// parser that keeps up with one stream is idle at every hand-over:
/// handing it 8 KiB at a time, a parser made faster made cat-ascii
/// cost more a byte, not less.
const TAIL_WAIT: Duration = Duration::from_micros(200);

/// The keys typed into this pane.  One pane to a process, so one of
/// these.
static ECHO: Echo = Echo::new();

/// A key is on its way to the pane's program.
///
/// A flood is gathered a full chunk at a time, up to 3 ms of it, and
/// what a program prints about a key waited in that chunk with the
/// rest: a key typed into a flooding pane showed 0.8 to 0.9 ms later.
/// And a reader queued for its turn among flooding panes read nothing
/// until the turn came: with nine of them the key showed 3 to 5 ms
/// later at the median and 150 ms at the worst.  For a moment after a
/// key the reader hands over whatever it has each time the pty runs
/// dry, and waits for no turn.
///
/// Called by the thread that stops the reader.
pub(crate) fn typed() {
    ECHO.typed();
    lease::hurry();
}

/// PTY output on its way to the parser: a buffer that keeps its size
/// from one use to the next, and how much of it this read filled.
pub(crate) struct Chunk {
    buf: Vec<u8>,
    len: usize,
}

impl Chunk {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl From<Vec<u8>> for Chunk {
    fn from(buf: Vec<u8>) -> Self {
        let len = buf.len();
        Self { buf, len }
    }
}

pub(crate) struct PtyReader {
    stop: UnixStream,
    stopping: Arc<AtomicBool>,
    empties: SyncSender<Chunk>,
    /// Chunks handed to the loop and not yet handed back.
    unparsed: Arc<AtomicUsize>,
    thread: JoinHandle<()>,
}

/// The queue the reader fills: every chunk in flight fits.
pub(crate) fn channel() -> (SyncSender<Chunk>, Receiver<Chunk>) {
    mpsc::sync_channel(CHUNKS)
}

enum Gathered {
    /// The PTY may have more; `dry` when the output had stopped.
    Open { dry: bool },
    /// The PTY has ended.
    Ended,
    /// The reader was told to stop.
    Stopped,
}

impl PtyReader {
    /// Queue what the PTY gives on `tx`, calling `wake` after each
    /// chunk; at the PTY's end, set `exited` and wake once more.
    pub(crate) fn spawn<W>(
        id: u64,
        pty: Arc<Pty>,
        exited: Arc<AtomicBool>,
        tx: SyncSender<Chunk>,
        wake: W,
    ) -> Self
    where
        W: Fn() + Send + 'static,
    {
        let (stop, stop_rx) = UnixStream::pair().expect("a socket pair to stop the pty reader with");
        let stopping = Arc::new(AtomicBool::new(false));
        let unparsed = Arc::new(AtomicUsize::new(0));
        // one more than the chunks, so `stop` can always add its own
        let (empties, empties_rx) = mpsc::sync_channel::<Chunk>(CHUNKS + 1);
        for _ in 0..CHUNKS {
            // zeroed pages are not resident until a read lands in them
            let _ = empties.try_send(Chunk { buf: vec![0; READ_BUF_BYTES], len: 0 });
        }
        let stop_seen = Arc::clone(&stopping);
        let in_flight = Arc::clone(&unparsed);
        let thread = thread::Builder::new()
            .name(format!("l3-pty-reader-{id}"))
            .spawn(move || {
                let master = pty.raw_master();
                set_nonblocking(master, true);
                let wakes = Wakes::new(master, stop_rx.as_raw_fd());
                let mut turn = Turn::new(Leases::open());
                loop {
                    let mut chunk = match empties_rx.try_recv() {
                        Ok(chunk) => chunk,
                        Err(TryRecvError::Disconnected) => break,
                        Err(TryRecvError::Empty) => {
                            // the loop holds every chunk: no reading
                            // until one comes back, so no turn either
                            turn.pass();
                            match empties_rx.recv() {
                                Ok(chunk) => chunk,
                                Err(_) => break,
                            }
                        }
                    };
                    if stop_seen.load(Ordering::SeqCst) {
                        break;
                    }
                    // until this chunk has something in it
                    let end = loop {
                        // a flood that pauses this long is over, and
                        // its turn is not kept through the silence
                        let there = match wakes.wait(turn.in_flood().then_some(TAIL_WAIT)) {
                            Event::Stop => {
                                chunk.len = 0;
                                break Gathered::Stopped;
                            }
                            Event::Neither => {
                                turn.end();
                                continue;
                            }
                            Event::Data(there) => there,
                        };
                        let (len, end) = gather(&pty, &mut chunk.buf, &wakes, &in_flight, &mut turn, &ECHO, there);
                        chunk.len = len;
                        if len > 0 || !matches!(end, Gathered::Open { .. }) {
                            break end;
                        }
                    };
                    if chunk.len > 0 {
                        in_flight.fetch_add(1, Ordering::SeqCst);
                        if tx.send(chunk).is_err() {
                            // main loop is gone -- stop forwarding
                            break;
                        }
                        wake();
                    }
                    match end {
                        Gathered::Open { dry: true } => turn.end(),
                        Gathered::Open { dry: false } => turn.pass_if_spent(),
                        Gathered::Ended => {
                            exited.store(true, Ordering::SeqCst);
                            wake();
                            break;
                        }
                        Gathered::Stopped => break,
                    }
                }
                // whoever holds the master next -- the image after an
                // execv -- reads and writes it blocking
                set_nonblocking(master, false);
            })
            .expect("spawn l3-pty-reader thread");
        Self { stop, stopping, empties, unparsed, thread }
    }

    /// Hand a parsed chunk back to be filled again.
    pub(crate) fn recycle(&self, chunk: Chunk) {
        self.unparsed.fetch_sub(1, Ordering::SeqCst);
        let _ = self.empties.try_send(chunk);
    }

    /// Stop between two reads and wait for the thread.  A chunk it has
    /// read is queued; what it has not read stays in the PTY.
    pub(crate) fn stop(self) {
        self.stopping.store(true, Ordering::SeqCst);
        let _ = (&self.stop).write_all(&[0]);
        // a reader waiting for a chunk wakes for this one
        let _ = self.empties.try_send(Chunk::from(Vec::new()));
        let _ = self.thread.join();
    }
}

/// Read `pty` into `buf` until it is full or what is in it should not
/// wait any longer.  `there` is how many bytes the kernel said it
/// holds.  Returns the bytes read and how the read ended.
///
/// Output that comes in less than one full read is handed over at
/// once.  After a full read the program is writing in bulk, and a dry
/// PTY is not the end of it: the chunk stays open while the parser is
/// still on an earlier one -- handing it over sooner would not get it
/// parsed sooner, and a reader that sits waiting for a chunk to come
/// back lets the writer fall asleep -- and, while the parser is idle,
/// until the output pauses for [`TAIL_WAIT`].
fn gather(
    pty: &Pty,
    buf: &mut [u8],
    wakes: &Wakes,
    unparsed: &AtomicUsize,
    turn: &mut Turn,
    echo: &Echo,
    mut there: usize,
) -> (usize, Gathered) {
    let mut started = Instant::now();
    let mut len = 0;
    // a short read in the middle of a flood is a writer short of a
    // core, not a keystroke's echo
    let mut bulk = turn.in_flood();
    // the last read was a full one: the writer keeps up with this reader
    let mut full = false;
    loop {
        while there > 0 && len < buf.len() {
            let space = buf.len() - len;
            match pty.read_shared(&mut buf[len..]) {
                // macOS gives EIO when the slave side fully closes
                Ok(0) => return (len, Gathered::Ended),
                Ok(n) => {
                    len += n;
                    there = there.saturating_sub(n);
                    full = n >= KERNEL_READ_BYTES.min(space);
                    bulk |= full;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => there = 0,
                Err(_) => return (len, Gathered::Ended),
            }
        }
        if !bulk {
            return (len, Gathered::Open { dry: true });
        }
        if len >= buf.len() || (len > 0 && echo.expected()) {
            break;
        }
        let queued = turn.enter(len, || echo.expected());
        // a key was typed while this waited for its turn
        if len > 0 && echo.expected() {
            break;
        }
        if queued {
            started = Instant::now();
        }
        let idle = unparsed.load(Ordering::SeqCst) == 0;
        if started.elapsed() >= GATHER_FOR {
            break;
        }
        // A short read while this pane floods alone is the end of one
        // of the program's writes, and its next begins some ten
        // microseconds later: asking through that gap carried 2 to 3 %
        // more than sleeping through it.  With other panes flooding a
        // short read is a writer short of a core, and asking again is
        // the empty reads that cost them all.
        if full || turn.alone() {
            match ask_again(pty, &mut buf[len..]) {
                Asked::Got(n) => {
                    full = n >= KERNEL_READ_BYTES.min(buf.len() - len);
                    len += n;
                    continue;
                }
                Asked::Dry => full = false,
                Asked::Ended => return (len, Gathered::Ended),
            }
        }
        match wakes.wait(Some(if idle { TAIL_WAIT } else { GATHER_WAIT })) {
            Event::Stop => return (len, Gathered::Stopped),
            Event::Data(n) => there = n,
            Event::Neither if idle => return (len, Gathered::Open { dry: true }),
            Event::Neither => {}
        }
    }
    (len, Gathered::Open { dry: false })
}

enum Asked {
    Got(usize),
    Dry,
    Ended,
}

/// Right after a full read: the writer was woken by it and its next
/// kilobyte lands within microseconds, sooner than a wait would return.
fn ask_again(pty: &Pty, buf: &mut [u8]) -> Asked {
    for _ in 0..DRY_RETRIES {
        for _ in 0..RETRY_PAUSE_SPINS {
            std::hint::spin_loop();
        }
        match pty.read_shared(buf) {
            Ok(0) => return Asked::Ended,
            Ok(n) => return Asked::Got(n),
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
            Err(_) => return Asked::Ended,
        }
    }
    Asked::Dry
}

fn set_nonblocking(fd: RawFd, on: bool) {
    // SAFETY: fcntl on the pty master this session owns; a failure
    // leaves the flag as it was, and both read paths work either way.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return;
        }
        let flags = if on { flags | libc::O_NONBLOCK } else { flags & !libc::O_NONBLOCK };
        libc::fcntl(fd, libc::F_SETFL, flags);
    }
}

/// Wait until `fd` has something to read, data or its end; `false`
/// once `stop` has, whatever `fd` holds.
pub(crate) fn readable_before_stop(fd: RawFd, stop: &UnixStream) -> bool {
    let mut fds = [
        libc::pollfd { fd, events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: stop.as_raw_fd(), events: libc::POLLIN, revents: 0 },
    ];
    loop {
        // SAFETY: two valid pollfds, both fds open for the caller's lifetime.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        // an error is left for the read to report
        return fds[1].revents == 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_term::pty::{PtyConfig, TerminalSize};

    fn spawn(script: &str) -> Arc<Pty> {
        Arc::new(
            Pty::spawn(PtyConfig {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), script.into()],
                size: TerminalSize { cols: 80, rows: 24, pixel_width: 0, pixel_height: 0 },
                argv0: None,
                cwd: None,
                ..Default::default()
            })
            .expect("spawn /bin/sh"),
        )
    }

    fn nonblocking(fd: RawFd) -> bool {
        // SAFETY: reading the flags of a pty master the test owns.
        unsafe { libc::fcntl(fd, libc::F_GETFL) & libc::O_NONBLOCK != 0 }
    }

    /// A flood comes in full chunks, and for a moment after a key in
    /// whatever pieces the pty gave: the program's answer to the key is
    /// somewhere in them and must not wait for a chunk to fill.
    #[test]
    fn a_flood_is_handed_over_as_it_comes_while_an_echo_is_expected() {
        let pty = spawn("yes 0123456789012345678901234567890123456789");
        let (_stop, stop_rx) = UnixStream::pair().expect("a socket pair");
        set_nonblocking(pty.raw_master(), true);
        let wakes = Wakes::new(pty.raw_master(), stop_rx.as_raw_fd());
        let parser_idle = AtomicUsize::new(0);
        let echo = Echo::always();
        let mut turn = Turn::new(Leases::open());
        let mut buf = vec![0; READ_BUF_BYTES];
        // What one hand-over may carry is what the pty held when the
        // reader woke and at most one more read: a count of bytes, the
        // same however long either end is kept off its core.
        let mut of_the_flood = 0;
        while of_the_flood < 200 {
            let Event::Data(there) = wakes.wait(None) else { panic!("the flood stopped") };
            let (len, end) = gather(&pty, &mut buf, &wakes, &parser_idle, &mut turn, &echo, there);
            assert!(len < there + KERNEL_READ_BYTES, "{len} bytes handed over at once, the pty held {there}");
            match end {
                Gathered::Open { dry: false } => of_the_flood += 1,
                Gathered::Open { dry: true } => {}
                _ => panic!("the flood ended"),
            }
        }
    }

    /// Once a flood has stopped the reader sleeps: asking a pty that
    /// has nothing more to say is for the microseconds between two
    /// kilobytes, not for the silence after the last.
    #[test]
    fn a_reader_whose_flood_has_ended_sleeps() {
        fn cpu_used() -> Duration {
            // SAFETY: getrusage fills the struct it is given.
            let ru = unsafe {
                let mut ru: libc::rusage = std::mem::zeroed();
                libc::getrusage(libc::RUSAGE_SELF, &mut ru);
                ru
            };
            let of = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
            of(ru.ru_utime) + of(ru.ru_stime)
        }
        let pty = spawn("head -c 2000000 /dev/zero | tr '\\0' a; sleep 30");
        let exited = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let reader = PtyReader::spawn(1, Arc::clone(&pty), exited, tx, || {});
        let mut total = 0;
        while total < 2_000_000 {
            let chunk = rx.recv_timeout(Duration::from_secs(30)).expect("the flood stopped short");
            total += chunk.bytes().len();
            reader.recycle(chunk);
        }
        // long enough for the flood's last wait to have run out
        thread::sleep(Duration::from_millis(50));
        let before = cpu_used();
        thread::sleep(Duration::from_millis(250));
        let spent = cpu_used() - before;
        reader.stop();
        assert!(spent < Duration::from_millis(5), "the reader used {spent:?} of cpu in a quarter second of silence");
    }

    /// Two megabytes written in one go come through complete, and in
    /// chunks bigger than the kernel's one kilobyte.
    #[test]
    fn bulk_output_arrives_whole_in_chunks_larger_than_one_read() {
        let pty = spawn("head -c 2000000 /dev/zero | tr '\\0' a");
        let exited = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let reader = PtyReader::spawn(1, Arc::clone(&pty), Arc::clone(&exited), tx, || {});
        let (mut total, mut largest) = (0usize, 0usize);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(chunk) => {
                    assert!(chunk.bytes().iter().all(|b| *b == b'a'), "only what was written");
                    total += chunk.bytes().len();
                    largest = largest.max(chunk.bytes().len());
                    reader.recycle(chunk);
                }
                Err(_) if exited.load(Ordering::SeqCst) => break,
                Err(_) => assert!(Instant::now() < deadline, "the output never ended; got {total}"),
            }
        }
        reader.stop();
        assert_eq!(total, 2_000_000);
        assert!(largest > KERNEL_READ_BYTES, "largest chunk was {largest}");
    }

    /// A reader that is never handed a chunk back stops reading: what
    /// the program writes waits in the PTY, not in this process.
    #[test]
    fn output_waits_in_the_pty_while_no_chunk_comes_back() {
        let pty = spawn("head -c 2000000 /dev/zero | tr '\\0' a; sleep 30");
        let exited = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let reader = PtyReader::spawn(1, Arc::clone(&pty), exited, tx, || {});
        let mut held = Vec::new();
        while let Ok(chunk) = rx.recv_timeout(Duration::from_millis(500)) {
            held.push(chunk);
        }
        assert_eq!(held.len(), CHUNKS, "every chunk is out and nothing more arrives");
        reader.stop();
    }

    /// The image after an execv inherits the master; it gets it the way
    /// it has always been.
    #[test]
    fn the_master_blocks_again_once_the_reader_has_stopped() {
        let pty = spawn("sleep 30");
        let exited = Arc::new(AtomicBool::new(false));
        let (tx, _rx) = channel();
        let reader = PtyReader::spawn(1, Arc::clone(&pty), exited, tx, || {});
        let deadline = Instant::now() + Duration::from_secs(5);
        while !nonblocking(pty.raw_master()) {
            assert!(Instant::now() < deadline, "the reader never started");
            thread::sleep(Duration::from_millis(5));
        }
        reader.stop();
        assert!(!nonblocking(pty.raw_master()));
    }

    /// A byte into an idle PTY is in the parser's queue about as soon
    /// as a plain `poll` and `read` would have it: nothing on the way
    /// waits for more output or for a turn.
    #[test]
    fn a_lone_byte_is_not_held_back() {
        const ROUNDS: usize = 40;
        // the tty echoes what is typed at `cat`
        let plain = spawn("cat");
        let mut plain_best = Duration::MAX;
        for _ in 0..ROUNDS {
            let started = Instant::now();
            plain.write_shared(b"x").expect("type at the plain pty");
            let mut fds = [libc::pollfd { fd: plain.raw_master(), events: libc::POLLIN, revents: 0 }];
            // SAFETY: one valid pollfd on a master the test owns.
            assert_eq!(unsafe { libc::poll(fds.as_mut_ptr(), 1, 5000) }, 1, "no echo");
            let mut byte = [0u8; 16];
            assert_eq!(plain.read_shared(&mut byte).expect("read the echo"), 1);
            plain_best = plain_best.min(started.elapsed());
        }

        let pty = spawn("cat");
        let exited = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let reader = PtyReader::spawn(1, Arc::clone(&pty), exited, tx, || {});
        let mut best = Duration::MAX;
        for _ in 0..ROUNDS {
            let started = Instant::now();
            pty.write_shared(b"x").expect("type at the pty");
            let chunk = rx.recv_timeout(Duration::from_secs(5)).expect("no echo");
            best = best.min(started.elapsed());
            assert_eq!(chunk.bytes(), b"x");
            reader.recycle(chunk);
        }
        reader.stop();
        assert!(
            best < plain_best + TAIL_WAIT / 2,
            "a lone byte took {best:?} through the reader and {plain_best:?} through a plain read"
        );
    }
}

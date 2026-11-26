// CARVE-OUT: pre-existing file: the PTY reader thread and its measured hand-off policy; shrink on every touch
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
use std::time::Duration;

use marspot_term::idle_pages::Given;
use marspot_term::pty::Pty;

mod carried;
mod echo;
mod gather;
mod lease;
mod lend;
mod rest;
mod turn;
mod wakes;
use carried::Carried;
use echo::Echo;
use gather::{Gathered, gather};
use lease::Leases;
pub(crate) use lend::Lend;
use rest::Rest;
use turn::Turn;
use wakes::{Event, Wakes};

/// Most one chunk carries.
const READ_BUF_BYTES: usize = 64 * 1024;

/// Chunks that exist at all: the reader fills one only when the loop
/// has handed one back, so a program that writes faster than the
/// parser reads waits in the PTY instead of filling this process.
///
/// What a parser that cannot keep up has queued is also what stands
/// between a key and its answer on screen: with four chunks a key typed
/// into such a flood was answered 0.93 ms later at the median, with
/// three 0.67 ms and with two 0.43 ms.  Two left the parser waiting for
/// the reader now and then, 1 to 1.5 % of one stream's throughput;
/// three cost 0.4 %, inside what one run differs from the next by.
const CHUNKS: usize = 3;

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
/// key the reader hands over what it has as soon as the parser is idle
/// or the pty runs dry, and for a longer moment waits for no turn.
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
    /// Most a read has filled since the buffer's pages were last given
    /// back: see [`rest`].
    high: usize,
    given: Given,
}

impl Chunk {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl From<Vec<u8>> for Chunk {
    fn from(buf: Vec<u8>) -> Self {
        let len = buf.len();
        Self { buf, len, high: 0, given: Given::default() }
    }
}

pub(crate) struct PtyReader {
    stop: UnixStream,
    stopping: Arc<AtomicBool>,
    empties: SyncSender<Chunk>,
    /// Chunks handed to the loop and not yet handed back.
    unparsed: Arc<AtomicUsize>,
    lend: Arc<Lend>,
    /// See [`rest`].
    resting: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

/// The queue the reader fills: every chunk in flight fits.
pub(crate) fn channel() -> (SyncSender<Chunk>, Receiver<Chunk>) {
    mpsc::sync_channel(CHUNKS)
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
        Self::spawn_among(id, pty, exited, tx, wake, Leases::open())
    }

    /// [`Self::spawn`], taking its turns at bulk output from `leases`.
    fn spawn_among<W>(id: u64, pty: Arc<Pty>, exited: Arc<AtomicBool>, tx: SyncSender<Chunk>, wake: W, leases: Leases) -> Self
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
            let _ = empties.try_send(Chunk { len: 0, ..Chunk::from(vec![0; READ_BUF_BYTES]) });
        }
        let stop_seen = Arc::clone(&stopping);
        let in_flight = Arc::clone(&unparsed);
        let lend = Arc::new(Lend::new());
        let lending = Arc::clone(&lend);
        let resting = Arc::new(AtomicBool::new(false));
        let mut rest = Rest::new(Arc::clone(&resting));
        let thread = thread::Builder::new()
            .name(format!("l3-pty-reader-{id}"))
            .spawn(move || {
                let master = pty.raw_master();
                set_nonblocking(master, true);
                let wakes = Wakes::new(master, stop_rx.as_raw_fd(), stop_seen);
                let mut turn = Turn::new(leases);
                let mut carried = Carried::new();
                loop {
                    let mut chunk = match rest.take().map_or_else(|| empties_rx.try_recv(), Ok) {
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
                    if wakes.stopping() {
                        break;
                    }
                    // until this chunk has something in it
                    let end = loop {
                        // a flood that pauses this long is over, and
                        // its turn is not kept through the silence
                        let deadline = turn.in_flood().then_some(TAIL_WAIT);
                        rest.before_wait(&mut chunk, &empties_rx, deadline.is_some());
                        let there = match wakes.wait(deadline) {
                            Event::Stop => {
                                chunk.len = 0;
                                break Gathered::Stopped;
                            }
                            Event::Neither => {
                                turn.end();
                                carried.flood_over(&lending);
                                continue;
                            }
                            Event::Data(there) => there,
                        };
                        rest.woke();
                        let room = chunk.room();
                        let (len, end) = gather(&pty, &mut chunk.buf[..room], &wakes, &in_flight, &mut turn, &ECHO, there);
                        chunk.len = len;
                        debug_assert!(len <= chunk.room(), "read into pages given back");
                        chunk.take_back_if_full();
                        rest.filled(&mut chunk);
                        if len > 0 || !matches!(end, Gathered::Open { .. }) {
                            break end;
                        }
                    };
                    let len = chunk.len;
                    if chunk.len > 0 {
                        in_flight.fetch_add(1, Ordering::SeqCst);
                        if tx.send(chunk).is_err() {
                            // main loop is gone -- stop forwarding
                            break;
                        }
                        wake();
                    }
                    match end {
                        Gathered::Open { dry: true } => {
                            turn.end();
                            carried.flood_over(&lending);
                        }
                        Gathered::Open { dry: false } => {
                            // see `lend`: a pane flooding alone
                            let rated = carried.chunk(len, &lending);
                            #[cfg(test)]
                            if rated {
                                tests::note_rated(lending.is_welcome(), turn.alone(), ECHO.expected());
                            }
                            // A spent turn is renewed here when no pane
                            // waits for it, and handed on only otherwise.
                            // Handed on first, `renew` found no turn to
                            // renew: a writer that never paused, with the
                            // parser behind it, filled every chunk past a
                            // turn's 2 ms and the pty was never lent.
                            let may_lend = rated && lending.is_welcome() && turn.alone();
                            let renewed = may_lend && turn.renew();
                            if !may_lend {
                                turn.pass_if_spent();
                            }
                            if renewed && !ECHO.expected() {
                                lending.offer();
                                // the loop looks again, now that the
                                // pty is its to read
                                wake();
                                if !lending.wait_back(stop_rx.as_raw_fd(), &mut turn) {
                                    break;
                                }
                                if lending.came_back_paused() {
                                    carried.resume();
                                } else {
                                    carried.restart();
                                }
                            }
                        }
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
        Self { stop, stopping, empties, unparsed, lend, resting, thread }
    }

    /// The pty's loan to the loop: see [`lend`].
    pub(crate) fn lend(&self) -> &Arc<Lend> {
        &self.lend
    }

    /// Chunks handed to the loop and not yet handed back.
    pub(crate) fn in_flight(&self) -> usize {
        self.unparsed.load(Ordering::SeqCst)
    }

    /// Hand a parsed chunk back to be filled again.
    /// Once output has stopped it gives its pages back first: see
    /// [`rest`].
    pub(crate) fn recycle(&self, mut chunk: Chunk) {
        if self.resting.load(Ordering::SeqCst) {
            chunk.give_back_if_far();
        }
        self.unparsed.fetch_sub(1, Ordering::SeqCst);
        let _ = self.empties.try_send(chunk);
    }

    /// Stop between two reads and wait for the thread.  A chunk it has
    /// read is queued; what it has not read stays in the PTY.
    ///
    /// A reader queued for its turn among flooding panes waits inside
    /// `fcntl`, for up to 300 ms, and an update or a closing pane
    /// waited that long with it: the queue is left as it is for a key.
    pub(crate) fn stop(self) {
        self.stopping.store(true, Ordering::SeqCst);
        let _ = (&self.stop).write_all(&[0]);
        lease::hurry();
        // a reader waiting for a chunk wakes for this one
        let _ = self.empties.try_send(Chunk::from(Vec::new()));
        let _ = self.thread.join();
    }
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
pub(crate) mod tests {
    /// How the rates a flood's reader took came out, for a test that
    /// finds the pty never lent: `(rates, not welcome, not alone, echo
    /// expected)`.
    static RATED: [std::sync::atomic::AtomicUsize; 4] = [const { std::sync::atomic::AtomicUsize::new(0) }; 4];

    pub(crate) fn note_rated(welcome: bool, alone: bool, echo: bool) {
        use std::sync::atomic::Ordering::Relaxed;
        RATED[0].fetch_add(1, Relaxed);
        RATED[1].fetch_add(usize::from(!welcome), Relaxed);
        RATED[2].fetch_add(usize::from(!alone), Relaxed);
        RATED[3].fetch_add(usize::from(echo), Relaxed);
    }

    /// Floods this process's readers saw end.
    static FLOODS_OVER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    pub(crate) fn note_flood_over() {
        FLOODS_OVER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn rated() -> String {
        let n = |i: usize| RATED[i].load(std::sync::atomic::Ordering::Relaxed);
        format!(
            "{} rates: {} not welcome, {} not alone, {} with an echo expected; {} floods ended",
            n(0),
            n(1),
            n(2),
            n(3),
            FLOODS_OVER.load(std::sync::atomic::Ordering::Relaxed)
        )
    }

    use super::gather::KERNEL_READ_BYTES;
    use super::*;
    use marspot_term::pty::{PtyConfig, TerminalSize};
    use std::time::Instant;

    pub(super) fn spawn(script: &str) -> Arc<Pty> {
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

    /// What an awaited answer may be in is not handed to a parser that
    /// is still on an earlier chunk one dry read at a time: the reader
    /// reads on, and what it hands over is more than the pty held when
    /// it woke.
    #[test]
    fn a_busy_parser_is_not_handed_an_awaited_answer_in_pieces() {
        let pty = spawn("yes 0123456789012345678901234567890123456789");
        let (_stop, stop_rx) = UnixStream::pair().expect("a socket pair");
        set_nonblocking(pty.raw_master(), true);
        let wakes = Wakes::new(pty.raw_master(), stop_rx.as_raw_fd(), Arc::default());
        let parser_busy = AtomicUsize::new(1);
        let echo = Echo::later();
        let mut turn = Turn::new(Leases::open());
        let mut buf = vec![0; READ_BUF_BYTES];
        // The reader reads on only for as long as the pty is not dry,
        // and a writer kept off its core leaves it dry between any two
        // kilobytes: how often it reads on is the machine's business.
        // That it does at all is the reader's, so this looks until it
        // has seen one; the time limit only tells never from not yet.
        let give_up = Instant::now() + Duration::from_secs(60);
        let mut largest_over = 0;
        while largest_over < 2 * KERNEL_READ_BYTES {
            assert!(Instant::now() < give_up, "no hand-over carried more than one read past what the pty held: {largest_over}");
            let Event::Data(there) = wakes.wait(None) else { panic!("the flood stopped") };
            let (len, end) = gather(&pty, &mut buf, &wakes, &parser_busy, &mut turn, &echo, there);
            assert!(matches!(end, Gathered::Open { .. }), "the flood ended");
            largest_over = largest_over.max(len.saturating_sub(there));
        }
    }

    /// Right after a key the parser is handed what the pty gave each
    /// time it runs dry, busy or not: the answer of a program that read
    /// the key at once then sits in a chunk of its own.
    #[test]
    fn right_after_a_key_a_busy_parser_is_handed_each_piece_too() {
        let pty = spawn("yes 0123456789012345678901234567890123456789");
        let (_stop, stop_rx) = UnixStream::pair().expect("a socket pair");
        set_nonblocking(pty.raw_master(), true);
        let wakes = Wakes::new(pty.raw_master(), stop_rx.as_raw_fd(), Arc::default());
        let parser_busy = AtomicUsize::new(1);
        let echo = Echo::always();
        let mut turn = Turn::new(Leases::open());
        let mut buf = vec![0; READ_BUF_BYTES];
        for _ in 0..200 {
            let Event::Data(there) = wakes.wait(None) else { panic!("the flood stopped") };
            let (len, end) = gather(&pty, &mut buf, &wakes, &parser_busy, &mut turn, &echo, there);
            assert!(matches!(end, Gathered::Open { .. }), "the flood ended");
            assert!(len < there + KERNEL_READ_BYTES, "{len} bytes handed over at once, the pty held {there}");
        }
    }

    /// A flood comes in full chunks, and for a moment after a key in
    /// whatever pieces the pty gave while the parser is idle: the
    /// program's answer to the key is somewhere in them and must not
    /// wait for a chunk to fill.
    #[test]
    fn a_flood_is_handed_over_as_it_comes_while_an_echo_is_expected() {
        let pty = spawn("yes 0123456789012345678901234567890123456789");
        let (_stop, stop_rx) = UnixStream::pair().expect("a socket pair");
        set_nonblocking(pty.raw_master(), true);
        let wakes = Wakes::new(pty.raw_master(), stop_rx.as_raw_fd(), Arc::default());
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
        // However long the program is kept from writing: the minute
        // only tells a chunk that never comes from one not here yet.
        let held: Vec<Chunk> =
            (0..CHUNKS).map(|n| rx.recv_timeout(Duration::from_secs(60)).unwrap_or_else(|_| panic!("chunk {n} never came"))).collect();
        // every chunk is out: a reader that read on would be late with
        // the next on a busy machine, never early
        assert!(rx.recv_timeout(Duration::from_millis(500)).is_err(), "a chunk more than there are came");
        drop(held);
        reader.stop();
    }

    /// A reader told to stop just before its flood queues for a turn,
    /// where no signal reaches it yet, does not begin the wait.
    #[test]
    fn a_reader_told_to_stop_does_not_queue_for_a_turn() {
        let path = lease::tests::lock_file("stop-early");
        let mut turn = Turn::new(Leases::at(&path).patient());
        // a flood that has used up a turn waits for its next one
        turn.enter(KERNEL_READ_BYTES, || false);
        turn.pass();
        let other = lease::tests::holder(&path);
        let pty = spawn("yes 0123456789012345678901234567890123456789");
        let (_stop, stop_rx) = UnixStream::pair().expect("a socket pair");
        set_nonblocking(pty.raw_master(), true);
        let wakes = Wakes::new(pty.raw_master(), stop_rx.as_raw_fd(), Arc::new(AtomicBool::new(true)));
        let parser_busy = AtomicUsize::new(1);
        let mut buf = vec![0; READ_BUF_BYTES];
        let Event::Data(there) = wakes.wait(None) else { panic!("the flood stopped") };
        let started = Instant::now();
        let (_, end) = gather(&pty, &mut buf, &wakes, &parser_busy, &mut turn, &Echo::new(), there);
        assert!(matches!(end, Gathered::Stopped), "the reader went on reading");
        assert!(started.elapsed() < lease::tests::LONG, "the reader waited for a turn nobody hands on");
        lease::tests::release(other);
        let _ = std::fs::remove_file(path);
    }

    /// Stopping a reader that is queued for its turn ends the wait.
    #[test]
    fn a_queued_reader_stops_when_told() {
        let path = lease::tests::lock_file("stop-queued");
        let leases = Leases::at(&path).patient();
        let other = lease::tests::holder(&path);
        let pty = spawn("yes 0123456789012345678901234567890123456789");
        let exited = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let reader = PtyReader::spawn_among(1, Arc::clone(&pty), exited, tx, || {}, leases);
        let deadline = Instant::now() + lease::tests::LONG / 2;
        while !lease::tests::someone_queued() {
            // the flood goes on for as long as its chunks come back
            if let Ok(chunk) = rx.try_recv() {
                reader.recycle(chunk);
            }
            assert!(Instant::now() < deadline, "the reader never queued for a turn");
            thread::yield_now();
        }
        // most likely inside `fcntl` by now; right either way
        thread::sleep(Duration::from_millis(20));
        let started = Instant::now();
        reader.stop();
        assert!(started.elapsed() < lease::tests::LONG / 2, "the reader stopped when its wait for a turn ran out");
        lease::tests::release(other);
        let _ = std::fs::remove_file(path);
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
}

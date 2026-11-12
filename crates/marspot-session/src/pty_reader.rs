//! The thread that reads a pane's PTY output for the main loop.
//!
//! macOS hands back at most 1024 bytes per `read` of a pty master,
//! whatever the buffer, so a bulk `cat` is 130 000 reads for 134 MB.
//! Waiting in `poll` before each one put the reader to sleep between
//! every two kilobytes: it and the writing program took turns being
//! woken, and that hand-over, not the parser, set the pace -- a reader
//! that throws the bytes away carries 158 MB/s that way and 390 MB/s
//! when it asks again instead of sleeping.  So the master is read
//! without blocking until it runs dry, the kilobytes are gathered into
//! one chunk, and a reader that has seen a full read asks again a few
//! times before it goes back to `poll`.

use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use marspot_term::pty::Pty;

/// Most one chunk carries.
const READ_BUF_BYTES: usize = 64 * 1024;

/// Chunks that exist at all: the reader fills one only when the loop
/// has handed one back, so a program that writes faster than the
/// parser reads waits in the PTY instead of filling this process.
const CHUNKS: usize = 4;

/// What one `read` of a pty master returns at most on macOS.
const KERNEL_READ_BYTES: usize = 1024;

/// How many times a reader that found the PTY dry right after a full
/// read asks again before it stops to wait.  A full read is a program
/// writing in bulk with a core of its own, and its next kilobyte is a
/// few microseconds away: eight retries already carry what sixty-four
/// do, with four a third of the dry spells end in a wait and the rate
/// falls by a fifth.  After a short read there is no asking again: the
/// writer is slower than this reader, and every retry would be a system
/// call that finds nothing.
const DRY_RETRIES: u32 = 16;

/// Longest a chunk the parser is not waiting for is kept open for more.
const GATHER_FOR: Duration = Duration::from_millis(3);

/// How long one wait for more lasts while the parser is busy.
const GATHER_WAIT: Duration = Duration::from_millis(1);

/// Bulk output worth handing to an idle parser at once.  Less than
/// this is held for [`TAIL_WAIT`] in case more is on its way.
///
/// Nine panes writing at once leave each writer waiting for a core, and
/// their output arrives a hundred bytes at a time.  Handed over as it
/// came, every scrap was a wake, a parse and a published frame: the
/// nine cost 405 instructions a byte against 200 before.
const WORTH_PARSING: usize = 8 * 1024;

/// How long the tail of a bulk write waits for more before it is handed
/// to an idle parser.  Output shorter than one full read never waits.
const TAIL_WAIT: Duration = Duration::from_micros(200);

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
    /// The PTY may have more.
    Open,
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
                // waits here while the loop holds every chunk
                while let Ok(mut chunk) = empties_rx.recv() {
                    if stop_seen.load(Ordering::SeqCst) {
                        break;
                    }
                    // until this chunk has something in it
                    let end = loop {
                        if wait_readable(master, &stop_rx, -1) == Readable::Stop {
                            chunk.len = 0;
                            break Gathered::Stopped;
                        }
                        let (len, end) = gather(&pty, &mut chunk.buf, &stop_rx, &in_flight);
                        chunk.len = len;
                        if len > 0 || !matches!(end, Gathered::Open) {
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
                        Gathered::Open => {}
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
/// wait any longer.  Returns the bytes read and how the read ended.
///
/// Output that comes in less than one full read is handed over at
/// once.  After a full read the program is writing in bulk, and a dry
/// PTY is not the end of it: the chunk stays open while the parser is
/// still on an earlier one -- handing it over sooner would not get it
/// parsed sooner, and a reader that sits waiting for a chunk to come
/// back lets the writer fall asleep -- and, for [`TAIL_WAIT`], while it
/// holds too little to be worth a parse of its own.
fn gather(pty: &Pty, buf: &mut [u8], stop: &UnixStream, unparsed: &AtomicUsize) -> (usize, Gathered) {
    let started = Instant::now();
    let mut len = 0;
    let mut bulk = false;
    let mut retried = 0;
    // a short read, or retries that found nothing, say the PTY is dry
    let mut dry = false;
    while len < buf.len() {
        if !dry {
            let space = buf.len() - len;
            match pty.read_shared(&mut buf[len..]) {
                // macOS gives EIO when the slave side fully closes
                Ok(0) => return (len, Gathered::Ended),
                Ok(n) => {
                    len += n;
                    if n >= KERNEL_READ_BYTES.min(space) {
                        bulk = true;
                        retried = 0;
                        continue;
                    }
                    if !bulk {
                        break;
                    }
                    dry = true;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if !bulk {
                        break;
                    }
                    if retried < DRY_RETRIES {
                        retried += 1;
                        std::hint::spin_loop();
                        continue;
                    }
                    dry = true;
                }
                Err(_) => return (len, Gathered::Ended),
            }
        }
        let idle = unparsed.load(Ordering::SeqCst) == 0;
        if started.elapsed() >= GATHER_FOR || (idle && len >= WORTH_PARSING) {
            break;
        }
        match wait_briefly(pty.raw_master(), stop, if idle { TAIL_WAIT } else { GATHER_WAIT }) {
            Readable::Stop => return (len, Gathered::Stopped),
            Readable::Data => {
                dry = false;
                // no asking again until a full read says the writer is fast
                retried = DRY_RETRIES;
            }
            Readable::Neither if idle => break,
            Readable::Neither => {}
        }
    }
    (len, Gathered::Open)
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

#[derive(PartialEq)]
enum Readable {
    Data,
    Stop,
    Neither,
}

/// Wait until `fd` has something to read, data or its end, `stop` has,
/// or `timeout_ms` has passed (never, when negative).  `stop` wins.
fn wait_readable(fd: RawFd, stop: &UnixStream, timeout_ms: i32) -> Readable {
    let mut fds = [
        libc::pollfd { fd, events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: stop.as_raw_fd(), events: libc::POLLIN, revents: 0 },
    ];
    loop {
        // SAFETY: two valid pollfds, both fds open for the caller's lifetime.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout_ms) };
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            // the read reports it
            return Readable::Data;
        }
        if fds[1].revents != 0 {
            return Readable::Stop;
        }
        if fds[0].revents != 0 {
            return Readable::Data;
        }
        if n == 0 {
            return Readable::Neither;
        }
    }
}

/// [`wait_readable`] for a wait shorter than `poll` can express.
fn wait_briefly(fd: RawFd, stop: &UnixStream, wait: Duration) -> Readable {
    let stop_fd = stop.as_raw_fd();
    let highest = fd.max(stop_fd);
    if highest < 0 || highest as usize >= libc::FD_SETSIZE {
        return wait_readable(fd, stop, wait.as_millis().max(1) as i32);
    }
    // SAFETY: an fd_set is plain bits and all-zero is its empty value;
    // both descriptors are open for the caller's lifetime and below
    // FD_SETSIZE, checked above, so FD_SET and FD_ISSET stay inside it.
    unsafe {
        let mut set: libc::fd_set = std::mem::zeroed();
        libc::FD_SET(fd, &mut set);
        libc::FD_SET(stop_fd, &mut set);
        let mut timeout = libc::timeval { tv_sec: 0, tv_usec: wait.subsec_micros() as libc::suseconds_t };
        let n = libc::select(highest + 1, &mut set, std::ptr::null_mut(), std::ptr::null_mut(), &mut timeout);
        if n > 0 {
            if libc::FD_ISSET(stop_fd, &set) {
                return Readable::Stop;
            }
            return Readable::Data;
        }
        if n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            // the read reports it
            return Readable::Data;
        }
        Readable::Neither
    }
}

/// Wait until `fd` has something to read, data or its end; `false`
/// once `stop` has, whatever `fd` holds.
pub(crate) fn readable_before_stop(fd: RawFd, stop: &UnixStream) -> bool {
    wait_readable(fd, stop, -1) != Readable::Stop
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
}

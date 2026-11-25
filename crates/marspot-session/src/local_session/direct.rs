//! Reading a lent pty on the thread that parses: see
//! [`crate::pty_reader::Lend`].

use std::io;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::LocalSession;
use super::pump::{Pace, SLICE_BYTES, SLICE_SLACK};
use crate::publish_gate::Follow;

/// How long the pty may stay dry before it goes back to the reader.
/// The writing program's own pauses inside a flood are some ten
/// microseconds; past this the output has stopped, and an idle pty is
/// not this thread's to ask.
const DRY_FOR: Duration = Duration::from_micros(50);

/// Spin-loop hints between two asks of a dry pty.
const DRY_PAUSE_SPINS: u32 = 64;

/// More than the kernel hands over in one read of a pty.
const READ_BYTES: usize = 4096;

/// Reads between two looks at the clock.
const READS_A_LOOK: u32 = 8;

#[cfg(test)]
thread_local! {
    /// Bytes this thread has read off a lent pty.
    static READ_HERE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl LocalSession {
    /// Whether this turn may read a lent pty itself.  The reader lends
    /// only while it may.
    pub(super) fn welcomes_the_pty(&self, pace: Option<&Pace>, raw: bool) -> bool {
        pace.is_some() && !self.hold && !raw
    }

    /// Read the lent pty and parse what it gives, a read at a time,
    /// until the turn is over or the output pauses.  Returns the bytes
    /// parsed and whether the flood goes on.
    ///
    /// The pty is given back when it stays dry, when it has ended, and
    /// when the reader asks for it; it stays lent across the end of a
    /// turn, which comes straight back here.
    ///
    /// It stays lent across a key too.  Given back for the reader to
    /// bring the answer, the answer waited for the reader to wake: the
    /// 99th percentile of a key typed into a pane streaming alone went
    /// from 0.8-1.2 ms to 1.6-2.7.  While an answer is awaited the turn
    /// ends after as much output as a frame follows, as it does for
    /// queued chunks.
    pub(super) fn read_in_place(&mut self, pace: &Pace, deadline: Instant) -> (usize, bool) {
        let Some(lend) = self.reader.as_ref().map(|r| std::sync::Arc::clone(r.lend())) else {
            return (0, false);
        };
        let mut buf = [0u8; READ_BYTES];
        let mut total = 0;
        let mut dry_since: Option<Instant> = None;
        let mut reads = 0u32;
        // since the last look at the clock
        let (mut looked, mut unlooked) = (Instant::now(), 0usize);
        // how much may hold a key's answer before a frame is due
        let a_frame = pace.follow.map(|follow| match follow {
            Follow::Slice => SLICE_BYTES - SLICE_SLACK,
            Follow::Chunk => crate::publish_gate::FloodGate::BULK_BYTES,
        });
        loop {
            if lend.wanted_back() {
                lend.give_back();
                return (total, false);
            }
            if pace.key_waiting.load(Ordering::Acquire) {
                return (total, true);
            }
            // the reader keeps the master non-blocking for as long as
            // it lives, and it lives for as long as it lends
            match self.pty.read_shared(&mut buf) {
                Ok(n) if n > 0 => {
                    self.take_in(&buf[..n]);
                    #[cfg(test)]
                    READ_HERE.set(READ_HERE.get() + n);
                    total += n;
                    unlooked += n;
                    dry_since = None;
                    reads += 1;
                    if a_frame.is_some_and(|enough| total >= enough) {
                        return (total, true);
                    }
                    if reads.is_multiple_of(READS_A_LOOK) {
                        let now = Instant::now();
                        // slower here than the reader was by itself:
                        // the rest of this flood is the reader's
                        if !lend.carried_here(unlooked, now.duration_since(looked)) {
                            return (total, false);
                        }
                        (looked, unlooked) = (now, 0);
                        if now >= deadline {
                            return (total, true);
                        }
                    }
                }
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {
                    let now = Instant::now();
                    if dry_since.is_some_and(|since| now.duration_since(since) >= DRY_FOR) {
                        lend.give_back();
                        return (total, false);
                    }
                    dry_since.get_or_insert(now);
                    if now >= deadline {
                        return (total, true);
                    }
                    for _ in 0..DRY_PAUSE_SPINS {
                        std::hint::spin_loop();
                    }
                }
                // the end of the pty is the reader's to report
                _ => {
                    lend.give_back();
                    return (total, false);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::RawFd;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::thread;

    use marspot_term::pty::Pty;
    use marspot_term::terminal::Terminal;

    use crate::pty_reader::PtyReader;

    const COLS: u16 = 122;
    const LINES: usize = 120_000;
    const FRAME: Duration = Duration::from_millis(8);
    /// Under nextest's one-minute limit, so a wait that never ends says
    /// why before the runner kills it.
    const LONG: Duration = Duration::from_secs(45);
    const PAD: &str = "0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";

    /// Bytes a `Feed::Lines` writes at a time.
    const CHUNK: usize = 64 << 10;
    /// A `Feed::Lines` stops here, lent or not: the lines eight times.
    const MOST_BYTES: usize = 8 * LINES * 109;

    /// `LINES` numbered lines of 109 bytes, thirteen megabytes in all.
    fn numbered() -> Vec<u8> {
        (0..LINES).flat_map(|i| format!("{i:07} {PAD}\n").into_bytes()).collect()
    }

    /// The first `n` bytes of the numbered lines written over and over.
    fn repeated(n: usize) -> Vec<u8> {
        numbered().into_iter().cycle().take(n).collect()
    }

    fn taken() -> Vec<u8> {
        super::super::pump::TAKEN.take()
    }

    const ENDLESS: &[u8] = b"0123456789012345678901234567890123456789\n";

    /// What the pane is fed: numbered lines until the test says stop,
    /// then the end of the pty or a pty left open; or a line again and
    /// again.
    enum Feed {
        Lines { then_open: bool },
        Endless(&'static [u8]),
    }

    /// Writes a pane's output from a thread of this process, in place of
    /// a program.
    ///
    /// What these tests check is what the loop does once a flood has the
    /// pty lent to it, and while its output keeps coming.  A program in
    /// the pane is a process the scheduler can take off its core: with
    /// every core busy (load1 25-31 on mini) `cat` paused often enough
    /// that no flood lasted the four megabytes the reader rates before
    /// it lends -- rightly, a paused writer is not a flood -- and `yes`
    /// never kept a lent pty full for the slice a key's answer is read
    /// in.  The tests then waited out their minute.  This thread runs at
    /// the user-interactive QoS, above any of that load, so the output
    /// is as steady as the tests need it to be on any machine.  The
    /// pty's own output processing is off: the bytes come out as they
    /// were written.
    ///
    /// No QoS keeps a thread on its core inside a virtual machine: the
    /// hypervisor can take the vCPU away mid-flood, and the reader rightly
    /// takes such a pause for the end of a flood.  So the flood tests feed
    /// lines until the loop has read some itself.
    struct Feeder {
        slave: Option<std::os::fd::OwnedFd>,
        stop: Arc<AtomicBool>,
        /// Bytes a `Feed::Lines` has written, a chunk at a time.
        bytes: Arc<AtomicUsize>,
        /// A `Feed::Lines` has written its last chunk.
        done: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl Feeder {
        /// Stops a `Feed::Lines` once the loop has read some of it
        /// itself since `before` and at least `LINES` lines have gone
        /// out.
        fn stop_once_lent(&self, before: usize) {
            if READ_HERE.get() > before && self.bytes.load(Ordering::SeqCst) >= LINES * 109 {
                self.stop.store(true, Ordering::SeqCst);
            }
        }

        /// The bytes written in all, once the last chunk is out.
        fn written(&self) -> Option<usize> {
            self.done.load(Ordering::SeqCst).then(|| self.bytes.load(Ordering::SeqCst))
        }
    }

    impl Drop for Feeder {
        fn drop(&mut self) {
            // the session, dropped first, closed the master: a write
            // blocked on a full pty has failed by now
            self.stop.store(true, Ordering::SeqCst);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
            drop(self.slave.take());
        }
    }

    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    }

    /// A session whose pane is fed `feed`, read by a reader of its own.
    /// The feeder is declared first by the caller, so it outlives the
    /// session.
    fn fed(feed: Feed) -> (Feeder, LocalSession) {
        use std::io::Write;
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
        let (mut master, mut slave) = (0, 0);
        let mut size = libc::winsize { ws_row: 24, ws_col: COLS, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: openpty fills the two descriptors; no name or settings asked for.
        assert_eq!(unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), &mut size) }, 0);
        // SAFETY: just opened, owned by nothing else.
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        // SAFETY: a termios read and written back on the slave just opened.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(slave.as_raw_fd(), &mut t), 0);
            t.c_oflag &= !libc::OPOST;
            assert_eq!(libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &t), 0);
        }
        // the writer's own descriptor: closing it on its way out is the
        // end of the pty unless the feeder keeps the other one open
        // SAFETY: dup of a descriptor this function owns.
        let mut out = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(libc::dup(slave.as_raw_fd())) });
        let keep = matches!(feed, Feed::Lines { then_open: true } | Feed::Endless(_));
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let (bytes, done) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(false)));
        let (written, finished) = (Arc::clone(&bytes), Arc::clone(&done));
        let thread = thread::spawn(move || {
            // SAFETY: sets this thread's own QoS.
            assert_eq!(unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) }, 0);
            match feed {
                // an error is the session gone: nobody reads any more
                Feed::Lines { .. } => {
                    // made before the first write: a pause to make more
                    // is the end of a flood to the reader
                    let lines = numbered();
                    let mut sent = 0;
                    while !stopping.load(Ordering::SeqCst) && sent < MOST_BYTES {
                        let at = sent % lines.len();
                        let chunk = &lines[at..(at + CHUNK).min(lines.len())];
                        if out.write_all(chunk).is_err() {
                            break;
                        }
                        sent += chunk.len();
                        written.store(sent, Ordering::SeqCst);
                    }
                    finished.store(true, Ordering::SeqCst);
                }
                Feed::Endless(line) => {
                    while !stopping.load(Ordering::SeqCst) && out.write_all(line).is_ok() {}
                }
            }
        });
        let pty = Arc::new(Pty::from_raw_master(master, 0));
        let exited = Arc::new(AtomicBool::new(false));
        let (tx, rx) = crate::pty_reader::channel();
        let reader = PtyReader::spawn(1, Arc::clone(&pty), Arc::clone(&exited), tx, || {});
        let session = LocalSession {
            id: 1,
            pty,
            child_pid: 0,
            terminal: Terminal::new(COLS, 24),
            bytelog: None,
            out: Default::default(),
            rx,
            reader: Some(reader),
            writer: super::super::PtyWriter::new("l3-pty-writer-1", std::io::sink(), marspot_term::frame_writer::cap::PTY),
            replies: Default::default(),
            line: Default::default(),
            exited,
            last_output: None,
            pending_scrollback_pages: Vec::new(),
            hold: false,
            held: Vec::new(),
            held_bytes: 0,
            unfinished: None,
            before_key: 0,
            cols: COLS,
            rows: 24,
        };
        (Feeder { slave: keep.then_some(slave), stop, bytes, done, thread: Some(thread) }, session)
    }

    fn pace(follow: Option<Follow>, key_waiting: &AtomicBool) -> Pace<'_> {
        Pace { budget: FRAME, follow, key_waiting }
    }

    fn lent(s: &LocalSession) -> bool {
        s.reader.as_ref().is_some_and(|r| r.lend().lent())
    }

    fn nonblocking(fd: RawFd) -> bool {
        // SAFETY: reading the flags of a pty master the test owns.
        unsafe { libc::fcntl(fd, libc::F_GETFL) & libc::O_NONBLOCK != 0 }
    }

    /// Turn the loop until `done`; the bytes it read off the pty
    /// itself.
    fn turn_until(s: &mut LocalSession, follow: Option<Follow>, mut done: impl FnMut(&LocalSession, usize) -> bool) -> usize {
        let no_key = AtomicBool::new(false);
        let deadline = Instant::now() + LONG;
        let mut total = 0;
        let before = READ_HERE.get();
        loop {
            let (n, more) = s.pump_within(pace(follow, &no_key));
            total += n;
            if done(s, total) {
                return READ_HERE.get() - before;
            }
            assert!(
                Instant::now() < deadline,
                "what this waited for never came, after {total} bytes; {}",
                crate::pty_reader::tests::rated()
            );
            if !more {
                thread::sleep(Duration::from_micros(200));
            }
        }
    }

    /// A flood in a pane alone is read by the loop, and every byte of
    /// it is parsed once and in the order it was written, whichever
    /// thread read it.
    #[test]
    fn a_flood_read_by_two_threads_in_turn_is_parsed_whole_and_in_order() {
        let (feeder, mut s) = fed(Feed::Lines { then_open: false });
        taken();
        let before = READ_HERE.get();
        turn_until(&mut s, None, |s, _| {
            feeder.stop_once_lent(before);
            s.exited.load(Ordering::SeqCst) && !lent(s)
        });
        lent_since(before);
        s.pump();
        let got = taken();
        let want = repeated(feeder.written().expect("the feed ended with the pty"));
        let same = got.iter().zip(&want).take_while(|(a, b)| a == b).count();
        let near = |v: &[u8]| String::from_utf8_lossy(&v[same.saturating_sub(120)..(same + 240).min(v.len())]).into_owned();
        assert!(
            got == want,
            "{} bytes parsed of {}, the same up to byte {same}\n got: {:?}\nwant: {:?}",
            got.len(),
            want.len(),
            near(&got),
            near(&want)
        );
    }

    /// When the flood pauses the pty goes back to the reader, which
    /// brings what comes next.
    #[test]
    fn a_pause_gives_the_pty_back_to_the_reader() {
        let (feeder, mut s) = fed(Feed::Lines { then_open: true });
        taken();
        let before = READ_HERE.get();
        let mut all = None;
        turn_until(&mut s, None, |s, total| {
            feeder.stop_once_lent(before);
            if all.is_none() {
                all = feeder.written();
            }
            all == Some(total) && !lent(s)
        });
        lent_since(before);
        // the tty echoes what is typed
        s.pty.write_shared(b"after\n").expect("type at the pty");
        turn_until(&mut s, None, |_, total| total >= b"after\n".len());
        assert!(taken().ends_with(b"after\n"));
        assert!(!lent(&s), "a line of echo is not a flood");
    }

    fn lent_since(before: usize) {
        assert!(READ_HERE.get() > before, "the loop was never lent the pty: {}", crate::pty_reader::tests::rated());
    }

    /// A key ends the turn before another byte is read; the pty stays
    /// with the loop, which reads the answer itself and ends its turn
    /// after every slice of output that may hold it.
    #[test]
    fn a_key_ends_the_turn_and_the_loop_reads_its_answer_a_slice_at_a_time() {
        let (_feeder, mut s) = fed(Feed::Endless(ENDLESS));
        turn_until(&mut s, None, |s, _| lent(s));
        let key = AtomicBool::new(true);
        let before = READ_HERE.get();
        let (_, more) = s.pump_within(pace(None, &key));
        assert!(more, "the flood was taken to be over");
        assert_eq!(READ_HERE.get(), before, "the turn went on reading with a key waiting");
        // lent, and what the reader queued before it lent the pty
        // parsed: on a busy machine the pty can come back to the reader
        // and be lent again in between, with more queued
        turn_until(&mut s, None, |s, _| lent(s) && s.reader.as_ref().is_some_and(|r| r.in_flight() == 0));
        s.typed();
        assert!(lent(&s), "the pty went back to the reader at a key");
        assert_eq!(s.before_key, 0, "nothing was in the reader's hand or queued");
        let no_key = AtomicBool::new(false);
        // a pump that found the pty dry for a moment read what there
        // was, and one that ran out its turn -- this thread taken off
        // its core for a frame on a busy machine -- what it got to; the
        // slice is told by one that stopped inside its turn with more to
        // come
        let deadline = Instant::now() + LONG;
        loop {
            let before = READ_HERE.get();
            let started = Instant::now();
            let (n, more) = s.pump_within(pace(Some(Follow::Slice), &no_key));
            let took = started.elapsed();
            assert_eq!(READ_HERE.get() - before, n, "the answer was not read by the loop");
            assert!(n <= SLICE_BYTES, "{n} bytes in a turn while an answer was awaited");
            if more && took < FRAME {
                assert!(n >= SLICE_BYTES - SLICE_SLACK, "{n} bytes before a frame could follow, in {took:?}");
                break;
            }
            assert!(Instant::now() < deadline, "the output never came faster than the loop read it");
            if !lent(&s) {
                turn_until(&mut s, None, |s, _| lent(s) && s.reader.as_ref().is_some_and(|r| r.in_flight() == 0));
            }
        }
    }

    /// Stopping the reader while the loop has its pty -- before an
    /// update, or with the pane closing -- does not wait for the flood,
    /// and leaves the pty to nobody: the master blocks again, as the
    /// next image expects it.
    #[test]
    fn a_reader_that_has_lent_its_pty_stops_when_told() {
        let (_feeder, mut s) = fed(Feed::Endless(ENDLESS));
        turn_until(&mut s, None, |s, _| lent(s));
        let lend = Arc::clone(s.reader.as_ref().expect("the reader").lend());
        let started = Instant::now();
        s.quiesce_for_handoff();
        assert!(started.elapsed() < Duration::from_secs(5), "stopping waited: {:?}", started.elapsed());
        assert!(!lend.lent(), "a stopped reader's pty is still lent");
        assert!(!nonblocking(s.pty.raw_master()));
        let no_key = AtomicBool::new(false);
        assert_eq!(s.pump_within(pace(None, &no_key)), (0, false), "the loop read a pty that has no reader");
    }

    /// While the picture is held the loop does not read the pty itself:
    /// held output is the reader's to bring.
    #[test]
    fn a_held_pane_is_not_lent_its_pty() {
        let (_feeder, mut s) = fed(Feed::Endless(ENDLESS));
        s.hold_grid(true);
        let read_here = turn_until(&mut s, None, |_, total| total > 1 << 20);
        assert_eq!(read_here, 0, "the loop read the pty of a held pane");
        s.hold_grid(false);
        turn_until(&mut s, None, |s, _| lent(s));
    }
}

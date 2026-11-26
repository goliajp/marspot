//! The pty lent to the thread that parses.
//!
//! One pane flooding alone is paced by the pty, a kilobyte at a time,
//! and a second thread that parses while the first reads slows the pair
//! down: a thread spinning beside a reader that threw its chunks away
//! took it from 336-361 MB/s to 317-331, and chunks parsed on another
//! core took it to 301-313.  Read and parsed by one thread, a kilobyte
//! at a time, the same corpus went at 325-332 with half the CPU time.
//!
//! So once a flood has filled a chunk the reader lends the pty to the
//! loop, which reads a little and parses it in place, and waits here.
//! The loop gives the pty back as soon as the output pauses or a key is
//! typed: an idle pty is watched by the reader's kqueue and by nothing
//! else.
//!
//! Which of the two is faster depends on the output and on the machine.
//! Text that parses faster than the pty hands it over went 3.5 % faster
//! read in place (321-337 MB/s against 287-328); text that parses
//! slower went 5 to 11 % slower (260-289 against 265-306, 267-277
//! against 297-314), because two threads parse while they read and one
//! cannot; two panes flooding side by side carried 306-327 together
//! against 266-300.  How often the loop finds the pty dry does not tell
//! the cases apart -- once in twenty reads in all of them -- so both
//! ways are timed: the reader carries the start of a flood itself and
//! notes its rate, the loop compares its own with that, and a flood it
//! carries slower goes back to the reader for a while.  A flood it
//! keeps goes back now and then too, for the reader's rate to be taken
//! again: compared once, against the rate of a flood's first
//! megabyte, half the floods of slow-parsing text stayed with the loop
//! for good and went 10 % slower.
//!
//! A pty the loop found dry is not the end of a flood.  With the pane's
//! process on efficiency cores the writer is scheduled out for longer
//! than the loop waits for more, every few megabytes, and a reader that
//! took two rates afresh after each carried a third to nine tenths of a
//! 64 MiB flood itself, at 13-54 % more CPU than a loop carrying nearly
//! all of it.  So a pty given back dry is offered again on the reader's
//! next full chunk, on the rate taken before, and what the loop carries
//! in those loans adds up to its verdicts: the same floods went with
//! 5-25 % less CPU, as fast to within 5 % either way.  A pause the
//! reader sees itself still ends the flood.  With two busy programs
//! beside it on the same cores floods end every few megabytes, and lent
//! through those pauses too, text that parses slowly went 9-12 % slower.
//!
//! Order.  The reader lends only after it has queued everything it
//! read, and reads nothing until the pty is given back; the loop reads
//! only once that queue is empty, and has parsed all it read before it
//! gives the pty back.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use super::turn::Turn;

/// How long the reader waits before it looks at its turn again.
const LOOK_MS: libc::c_int = 2;

/// What a rate is taken over: two megabytes, some seven milliseconds.
pub(super) const RATED_BYTES: u64 = 2 << 20;

/// Verdicts in the loop's favour after which the pty goes back for the
/// reader's rate to be taken again, the first time in a flood; twice
/// as many each time after.
pub(super) const KEPT_FOR: u64 = 16;

/// The loop keeps a flood it carries no more than this many percent
/// slower than the reader did: two rates of the same flood taken
/// milliseconds apart differ by about as much.
const SLOWER_PERCENT: u64 = 3;

/// Rates in a row the loop must carry slower before it gives the flood
/// back.  One alone is as often the noise between two rates of the same
/// flood: text that parses fast was given back on one, carried by the
/// reader for the next 64 MiB, and cost 15-20 % more CPU than never
/// giving it back.  Text that really parses slower is slower every time,
/// and pays one window more.
const SLOWER_IN_A_ROW: u64 = 2;

/// Bytes a second, from bytes carried and how long it took.
pub(super) fn rate(bytes: u64, took: Duration) -> u64 {
    (bytes as u128 * 1_000_000_000 / took.as_nanos().max(1)) as u64
}

pub(crate) struct Lend {
    /// The loop may read the pty; the reader does not.
    lent: AtomicBool,
    /// The reader asks for the pty back.
    wanted: AtomicBool,
    /// The loop would read a lent pty now.
    welcome: AtomicBool,
    /// The loop gave the pty back because the reader carried this flood
    /// faster.
    pub(super) declined: AtomicBool,
    /// The loop gave the pty back because the output paused.
    paused: AtomicBool,
    /// The next offer goes on with a flood the loop gave back dry: what
    /// it carried in the loans before still counts towards its next
    /// verdict.  Those loans end at every dry pty, and counted from none
    /// at each, a flood cut into loans shorter than a rate could never
    /// come to one.
    pub(super) resumed: AtomicBool,
    /// Bytes a second the reader last carried this flood at by itself.
    pub(super) two_rate: AtomicU64,
    /// What the loop has carried since its last verdict: bytes, and
    /// nanoseconds; the verdicts in its favour since the offer, or since
    /// the first of the loans a dry pty cut a flood into; and the rates
    /// in a row it carried slower.
    here: [AtomicU64; 4],
    /// Verdicts in the loop's favour it may keep the pty for.
    pub(super) keep_for: AtomicU64,
    /// Written by the loop when it gives the pty back.
    back: UnixStream,
    back_rx: UnixStream,
}

impl Lend {
    pub(super) fn new() -> Self {
        let (back, back_rx) = UnixStream::pair().expect("a socket pair to give the pty back with");
        back_rx.set_nonblocking(true).expect("a non-blocking socket");
        Self {
            lent: AtomicBool::new(false),
            wanted: AtomicBool::new(false),
            welcome: AtomicBool::new(false),
            declined: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            resumed: AtomicBool::new(false),
            two_rate: AtomicU64::new(0),
            here: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
            keep_for: AtomicU64::new(KEPT_FOR),
            back,
            back_rx,
        }
    }

    /// Whether the loop reads the pty itself when it is lent one.
    pub(crate) fn welcome(&self, yes: bool) {
        self.welcome.store(yes, Ordering::SeqCst);
    }

    pub(super) fn is_welcome(&self) -> bool {
        self.welcome.load(Ordering::SeqCst) && !self.declined.load(Ordering::SeqCst)
    }

    /// The loop has read and parsed `bytes` more in `took`.  `false`
    /// when the pty has been given back: the last two megabytes went
    /// slower than the reader carried the same flood, and it is not
    /// offered again for a while; or the loop has kept it long enough
    /// for the reader's rate to be due again.
    pub(crate) fn carried_here(&self, bytes: usize, took: Duration) -> bool {
        let carried = self.here[0].fetch_add(bytes as u64, Ordering::Relaxed) + bytes as u64;
        let nanos = self.here[1].fetch_add(took.as_nanos() as u64, Ordering::Relaxed) + took.as_nanos() as u64;
        if carried < RATED_BYTES {
            return true;
        }
        self.here[0].store(0, Ordering::Relaxed);
        self.here[1].store(0, Ordering::Relaxed);
        let here = rate(carried, Duration::from_nanos(nanos));
        let slower = here * 100 < self.two_rate.load(Ordering::SeqCst) * (100 - SLOWER_PERCENT);
        if !slower {
            self.here[3].store(0, Ordering::Relaxed);
        }
        if slower {
            if self.here[3].fetch_add(1, Ordering::Relaxed) + 1 < SLOWER_IN_A_ROW {
                return true;
            }
            self.declined.store(true, Ordering::SeqCst);
            self.keep_for.store(KEPT_FOR, Ordering::Relaxed);
        } else if self.here[2].fetch_add(1, Ordering::Relaxed) + 1 < self.keep_for.load(Ordering::Relaxed) {
            return true;
        } else {
            let kept = self.keep_for.load(Ordering::Relaxed);
            self.keep_for.store(kept.saturating_mul(2), Ordering::Relaxed);
        }
        self.give_back();
        false
    }

    /// The pty is the loop's to read.
    pub(crate) fn lent(&self) -> bool {
        self.lent.load(Ordering::SeqCst)
    }

    /// The reader's turn among flooding panes has gone to another.
    pub(crate) fn wanted_back(&self) -> bool {
        self.wanted.load(Ordering::SeqCst)
    }

    /// The loop reads no more: the reader goes on.  Called by the loop
    /// with everything it read parsed.
    pub(crate) fn give_back(&self) {
        if self.lent.swap(false, Ordering::SeqCst) {
            let _ = (&self.back).write_all(&[0]);
        }
    }

    /// The pty ran dry after the loop read `bytes` more in `took` since
    /// it last counted: those count too, or loans always shorter than
    /// the loop's look at the clock would never come to a verdict, and
    /// the pty would be lent again and again on an ever older rate.
    /// The reader goes on with the flood, unless they made a verdict
    /// that gave the pty back already.
    pub(crate) fn ran_dry(&self, bytes: usize, took: Duration) {
        if bytes == 0 || self.carried_here(bytes, took) {
            self.give_back_paused();
        }
    }

    fn give_back_paused(&self) {
        self.paused.store(true, Ordering::SeqCst);
        self.give_back();
    }

    /// Whether the pty came back from its last loan because it ran dry.
    pub(super) fn came_back_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// Lend the pty.  Called by the reader with nothing in hand and
    /// everything it read queued.
    pub(super) fn offer(&self) {
        if !self.resumed.swap(false, Ordering::SeqCst) {
            for count in &self.here {
                count.store(0, Ordering::Relaxed);
            }
        }
        self.paused.store(false, Ordering::SeqCst);
        self.wanted.store(false, Ordering::SeqCst);
        self.lent.store(true, Ordering::SeqCst);
    }

    /// Wait for the pty to be given back, handing the turn on as it is
    /// spent; `false` once `stop` has something to read.
    pub(super) fn wait_back(&self, stop: RawFd, turn: &mut Turn) -> bool {
        let mut fds = [
            libc::pollfd { fd: self.back_rx.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: stop, events: libc::POLLIN, revents: 0 },
        ];
        loop {
            // SAFETY: two valid pollfds, both fds open for the call.
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, LOOK_MS) };
            if n > 0 && fds[1].revents != 0 {
                // whoever stops the reader is the loop's thread, and
                // is not reading
                self.lent.store(false, Ordering::SeqCst);
                return false;
            }
            if n > 0 && fds[0].revents != 0 {
                let mut gone = [0u8; 8];
                let _ = (&self.back_rx).read(&mut gone);
                return true;
            }
            // other panes wait for this one's turn like for any other
            if n == 0 && !turn.renew() {
                self.wanted.store(true, Ordering::SeqCst);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::carried::{Carried, DECLINED_FOR};
    use super::super::lease::Leases;
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    fn stop_pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().expect("a socket pair")
    }

    /// A pty given back ends the reader's wait, and only then.
    #[test]
    fn the_reader_waits_until_the_pty_is_given_back() {
        let lend = Arc::new(Lend::new());
        let (_stop, stop_rx) = stop_pair();
        let mut turn = Turn::new(Leases::open());
        lend.offer();
        assert!(lend.lent());
        let theirs = Arc::clone(&lend);
        let giver = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            assert!(theirs.lent(), "the reader took the pty back by itself");
            theirs.give_back();
        });
        let started = Instant::now();
        assert!(lend.wait_back(stop_rx.as_raw_fd(), &mut turn));
        assert!(started.elapsed() >= Duration::from_millis(30), "the wait ended before the pty came back");
        assert!(!lend.lent());
        giver.join().expect("the giver");
    }

    /// Giving back a pty that is not lent leaves nothing behind for the
    /// next wait to mistake for its own.
    #[test]
    fn giving_back_twice_does_not_end_the_next_wait() {
        let lend = Arc::new(Lend::new());
        let (stop, stop_rx) = stop_pair();
        let mut turn = Turn::new(Leases::open());
        lend.offer();
        lend.give_back();
        lend.give_back();
        assert!(lend.wait_back(stop_rx.as_raw_fd(), &mut turn));
        lend.offer();
        // only the stop ends this one
        let stopper = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            (&stop).write_all(&[0]).expect("stop");
            stop
        });
        let started = Instant::now();
        assert!(!lend.wait_back(stop_rx.as_raw_fd(), &mut turn));
        assert!(started.elapsed() >= Duration::from_millis(30), "a stale byte ended the wait");
        assert!(!lend.lent(), "a stopped reader's pty is nobody's to read");
        stopper.join().expect("the stopper");
    }

    /// A flood the loop carries slower than the reader did goes back
    /// to the reader, which offers it again only much later; one it
    /// carries as fast stays, until the reader's rate is due again.
    #[test]
    fn a_flood_the_loop_carries_slower_goes_back_for_a_while() {
        const WINDOW: usize = RATED_BYTES as usize;
        let lend = Lend::new();
        lend.welcome(true);
        let mut carried = Carried::new();
        // a window carried in two milliseconds
        let window = |carried: &mut Carried, lend: &Lend| {
            std::thread::sleep(Duration::from_millis(2));
            carried.chunk(WINDOW, lend)
        };
        let took = |lend: &Lend, percent: u64| {
            let two = lend.two_rate.load(Ordering::SeqCst);
            Duration::from_nanos(WINDOW as u64 * 1_000_000_000 / (two * percent / 100))
        };
        assert!(!window(&mut carried, &lend), "the first chunk only starts the clock");
        assert!(!window(&mut carried, &lend), "the first rate is of a flood getting under way");
        assert!(window(&mut carried, &lend));
        assert!(lend.two_rate.load(Ordering::SeqCst) > 0);

        lend.offer();
        assert!(lend.carried_here(WINDOW / 2, took(&lend, 100) / 2), "half a window is no verdict");
        assert!(lend.carried_here(WINDOW / 2, took(&lend, 100) / 2), "as fast as the reader");
        assert!(lend.carried_here(WINDOW, took(&lend, 98)), "inside what two rates differ by");
        assert!(lend.lent());
        assert!(lend.carried_here(WINDOW, took(&lend, 90)), "one rate slower alone is no verdict");
        assert!(!lend.carried_here(WINDOW, took(&lend, 90)), "ten percent slower");
        assert!(!lend.lent());
        assert!(!lend.is_welcome(), "the same flood was offered straight away");
        lend.welcome(true);
        assert!(!lend.is_welcome(), "the loop's next turn undid the verdict");

        for rates in 1..DECLINED_FOR {
            assert!(!window(&mut carried, &lend), "offered again after {rates} rates");
        }
        assert!(window(&mut carried, &lend));
        assert!(lend.is_welcome());

        // slower again: twice as long before the next offer
        lend.offer();
        assert!(lend.carried_here(WINDOW, took(&lend, 90)), "one rate slower alone is no verdict");
        assert!(!lend.carried_here(WINDOW, took(&lend, 90)));
        for rates in 1..2 * DECLINED_FOR {
            assert!(!window(&mut carried, &lend), "offered a second time after {rates} rates");
        }
        assert!(window(&mut carried, &lend));

        // kept, and still given back for the reader's rate
        lend.offer();
        for verdict in 1..KEPT_FOR {
            assert!(lend.carried_here(WINDOW, took(&lend, 110)), "given back after {verdict} verdicts");
        }
        assert!(!lend.carried_here(WINDOW, took(&lend, 110)));
        assert!(!lend.lent());
        assert!(lend.is_welcome(), "a flood the loop kept counts as one it carried slower");
        // kept again: for twice as long
        lend.offer();
        for verdict in 1..2 * KEPT_FOR {
            assert!(lend.carried_here(WINDOW, took(&lend, 110)), "given back a second time after {verdict} verdicts");
        }
        assert!(!lend.carried_here(WINDOW, took(&lend, 110)));
        lend.offer();
        assert!(lend.carried_here(WINDOW, took(&lend, 90)), "one rate slower alone is no verdict");
        assert!(!lend.carried_here(WINDOW, took(&lend, 90)));
        carried.flood_over(&lend);
        assert!(lend.is_welcome(), "the next flood is offered afresh");
        assert!(!window(&mut carried, &lend), "the next flood's first chunk only starts the clock");
        assert!(!window(&mut carried, &lend), "the next flood's first rate is of it getting under way");
        lend.offer();
        for verdict in 1..KEPT_FOR {
            assert!(lend.carried_here(WINDOW, took(&lend, 110)), "the next flood given back after {verdict} verdicts");
        }
        assert!(!lend.carried_here(WINDOW, took(&lend, 110)));
        lend.offer();
        assert!(lend.carried_here(WINDOW, took(&lend, 90)), "one rate slower alone is no verdict");
        assert!(!lend.carried_here(WINDOW, took(&lend, 90)));
        for rates in 1..DECLINED_FOR {
            assert!(!window(&mut carried, &lend), "the next flood offered again after {rates} rates");
        }
        assert!(window(&mut carried, &lend), "the next flood waits as long as the first did");
    }

    /// A pty back from a loop that found it dry is offered on the
    /// reader's next full chunk, on the rate taken before the loan; not
    /// before the flood has its rates, not after it ended or the pty came
    /// back for another reason, and not while the loop is declined.
    #[test]
    fn a_pty_back_dry_is_offered_on_the_next_chunk() {
        const WINDOW: usize = RATED_BYTES as usize;
        let ms = Duration::from_millis;
        let lend = Lend::new();
        lend.welcome(true);
        let mut carried = Carried::new();
        let mut now = Instant::now();
        let mut window = |carried: &mut Carried, lend: &Lend, after: Duration| {
            now += after;
            carried.chunk_at(WINDOW, lend, now)
        };
        assert!(!window(&mut carried, &lend, ms(2)));
        assert!(!window(&mut carried, &lend, ms(2)));
        // back before the flood had its rates: they are still to take
        carried.resume();
        assert!(!window(&mut carried, &lend, ms(2)), "offered on the first rate");
        assert!(window(&mut carried, &lend, ms(2)), "the rate before the loan was not counted");
        let two = lend.two_rate.load(Ordering::SeqCst);
        carried.resume();
        assert!(window(&mut carried, &lend, ms(1)), "not offered on the next chunk");
        assert_eq!(lend.two_rate.load(Ordering::SeqCst), two, "a rate was taken across the loan");

        for (ended, how) in [(true, "the flood ended"), (false, "the pty came back for another reason")] {
            carried.resume();
            if ended {
                carried.flood_over(&lend);
            } else {
                carried.restart();
            }
            assert!(!window(&mut carried, &lend, ms(2)), "offered on the next chunk after {how}");
            assert!(!window(&mut carried, &lend, ms(2)), "offered on one rate after {how}");
            assert!(window(&mut carried, &lend, ms(2)));
        }

        // given back as slower, it stays the reader's
        lend.offer();
        let slow = Duration::from_nanos(WINDOW as u64 * 1_000_000_000 / (lend.two_rate.load(Ordering::SeqCst) * 90 / 100));
        assert!(lend.carried_here(WINDOW, slow));
        assert!(!lend.carried_here(WINDOW, slow));
        assert!(!lend.came_back_paused());
        carried.resume();
        assert!(!window(&mut carried, &lend, ms(1)), "a declined flood offered again");
        assert!(!window(&mut carried, &lend, ms(2)), "a declined flood offered on a rate");
    }

    /// Loans that a dry pty cuts a flood into add up to the loop's verdicts:
    /// slower over two windows spread across four loans, the flood goes
    /// back to the reader.  A loan on a fresh rate counts from none.
    #[test]
    fn loans_a_pause_cuts_add_up_to_a_verdict() {
        const WINDOW: usize = RATED_BYTES as usize;
        let ms = Duration::from_millis;
        let lend = Lend::new();
        lend.welcome(true);
        let mut carried = Carried::new();
        let mut now = Instant::now();
        let mut chunk = |carried: &mut Carried, lend: &Lend, after: Duration| {
            now += after;
            carried.chunk_at(WINDOW, lend, now)
        };
        assert!(!chunk(&mut carried, &lend, ms(2)));
        assert!(!chunk(&mut carried, &lend, ms(2)));
        assert!(chunk(&mut carried, &lend, ms(2)));
        let slow = Duration::from_nanos(WINDOW as u64 * 1_000_000_000 / (lend.two_rate.load(Ordering::SeqCst) * 90 / 100));

        lend.offer();
        for loan in 1..4 {
            assert!(lend.carried_here(WINDOW / 2, slow / 2), "given back after loan {loan}");
            lend.give_back_paused();
            carried.resume();
            assert!(chunk(&mut carried, &lend, ms(1)), "not offered again after loan {loan}");
            lend.offer();
        }
        assert!(!lend.carried_here(WINDOW / 2, slow / 2), "two slower windows across four loans came to no verdict");
        assert!(!lend.is_welcome());

        // a loan on a fresh rate forgets what the loans before it carried
        carried.flood_over(&lend);
        assert!(!chunk(&mut carried, &lend, ms(2)));
        assert!(!chunk(&mut carried, &lend, ms(2)));
        assert!(chunk(&mut carried, &lend, ms(2)));
        lend.offer();
        assert!(lend.carried_here(WINDOW / 2, slow / 2));
        lend.give_back_paused();
        carried.resume();
        assert!(chunk(&mut carried, &lend, ms(1)));
        carried.restart();
        assert!(!chunk(&mut carried, &lend, ms(2)));
        assert!(!chunk(&mut carried, &lend, ms(2)));
        assert!(chunk(&mut carried, &lend, ms(2)));
        lend.offer();
        assert_eq!(lend.here[0].load(Ordering::Relaxed), 0, "the half window of an earlier loan was counted");
    }

    /// Loans of seven 4 KiB reads each, every one ending at a dry pty
    /// before the loop looks at the clock, still come to the verdict
    /// that the loop is slower, two windows in.
    #[test]
    fn loans_shorter_than_a_look_add_up_to_a_verdict() {
        const WINDOW: usize = RATED_BYTES as usize;
        const LOAN: usize = 7 * 4096;
        let ms = Duration::from_millis;
        let lend = Lend::new();
        lend.welcome(true);
        let mut carried = Carried::new();
        let mut now = Instant::now();
        let mut chunk = |carried: &mut Carried, lend: &Lend, after: Duration| {
            now += after;
            carried.chunk_at(WINDOW, lend, now)
        };
        assert!(!chunk(&mut carried, &lend, ms(2)));
        assert!(!chunk(&mut carried, &lend, ms(2)));
        assert!(chunk(&mut carried, &lend, ms(2)));
        let slow = Duration::from_nanos(LOAN as u64 * 1_000_000_000 / (lend.two_rate.load(Ordering::SeqCst) * 90 / 100));
        let to_verdict = 2 * WINDOW.div_ceil(LOAN);

        lend.offer();
        for loan in 1..=to_verdict {
            lend.ran_dry(LOAN, slow);
            if loan < to_verdict {
                assert!(lend.came_back_paused(), "given back as slower after loan {loan} of {to_verdict}");
                carried.resume();
                assert!(chunk(&mut carried, &lend, ms(1)), "not offered again after loan {loan}");
                lend.offer();
            }
        }
        assert!(!lend.lent());
        assert!(!lend.came_back_paused(), "{to_verdict} loans, two windows slower, came to no verdict");
        assert!(!lend.is_welcome());
    }

    /// Only a pty the loop found dry comes back paused, and the next
    /// loan starts without it.
    #[test]
    fn the_pty_comes_back_paused_only_when_the_output_paused() {
        let lend = Lend::new();
        lend.offer();
        lend.give_back();
        assert!(!lend.came_back_paused());
        lend.offer();
        lend.give_back_paused();
        assert!(lend.came_back_paused());
        assert!(!lend.lent());
        lend.offer();
        assert!(!lend.came_back_paused(), "an old pause outlived the next loan");
    }

    /// A slower rate between two that are not is the noise between two
    /// rates of one flood: the pty stays lent.
    #[test]
    fn slower_rates_count_only_in_a_row() {
        const WINDOW: usize = RATED_BYTES as usize;
        let lend = Lend::new();
        lend.welcome(true);
        lend.two_rate.store(300 << 20, Ordering::SeqCst);
        let took = |percent: u64| Duration::from_nanos(WINDOW as u64 * 1_000_000_000 / ((300 << 20) * percent / 100));
        lend.offer();
        for _ in 0..4 {
            assert!(lend.carried_here(WINDOW, took(90)), "slower once");
            assert!(lend.carried_here(WINDOW, took(100)), "then not");
        }
        assert!(lend.lent());
        assert!(lend.carried_here(WINDOW, took(90)));
        assert!(!lend.carried_here(WINDOW, took(90)), "slower twice in a row");
        assert!(!lend.lent());
        // a new offer counts from none
        lend.offer();
        assert!(lend.carried_here(WINDOW, took(90)), "the count went with the last offer");
    }

    /// A reader whose turn has gone to another pane asks for its pty
    /// back, and has it back when the loop gives it.
    #[test]
    fn a_reader_without_a_turn_asks_for_the_pty_back() {
        // a flood that never had a turn, or handed its last one on
        let mut turn = Turn::new(Leases::open());
        assert!(!turn.renew());
        let lend = Arc::new(Lend::new());
        let (_stop, stop_rx) = stop_pair();
        lend.offer();
        assert!(!lend.wanted_back());
        let theirs = Arc::clone(&lend);
        let giver = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !theirs.wanted_back() {
                assert!(Instant::now() < deadline, "the reader never asked for its pty back");
                thread::sleep(Duration::from_millis(1));
            }
            theirs.give_back();
        });
        assert!(lend.wait_back(stop_rx.as_raw_fd(), &mut turn));
        giver.join().expect("the giver");
        lend.offer();
        assert!(!lend.wanted_back(), "an old request outlived the pty's return");
    }
}

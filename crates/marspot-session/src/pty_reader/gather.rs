//! Filling one chunk from the PTY.

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use marspot_term::pty::Pty;

use super::TAIL_WAIT;
use super::echo::Echo;
use super::turn::Turn;
use super::wakes::{Event, Wakes};

/// What one `read` of a pty master returns at most on macOS.
pub(super) const KERNEL_READ_BYTES: usize = 1024;

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

pub(super) enum Gathered {
    /// The PTY may have more; `dry` when the output had stopped.
    Open { dry: bool },
    /// The PTY has ended.
    Ended,
    /// The reader was told to stop.
    Stopped,
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
pub(super) fn gather(
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
        if len >= buf.len() {
            break;
        }
        if let Some(at_once) = echo.awaited().filter(|_| len > 0) {
            // A key's answer may be in this chunk.  Right after the key
            // it goes over as it is, so that the answer of a program
            // that reads the key at once is in a chunk of its own, and
            // an idle parser gets it now at any time.  Later, a busy
            // parser would not parse it sooner for having it sooner,
            // and a chunk handed over at every dry read is what typing
            // into a flood used to cost it: so this reads on until the
            // parser is idle, or the pty is dry.
            if at_once || unparsed.load(Ordering::SeqCst) == 0 || !(full || turn.alone()) {
                break;
            }
            match ask_again(pty, &mut buf[len..]) {
                Asked::Got(n) => {
                    full = n >= KERNEL_READ_BYTES.min(buf.len() - len);
                    len += n;
                    continue;
                }
                Asked::Dry => break,
                Asked::Ended => return (len, Gathered::Ended),
            }
        }
        let queued = turn.enter(len, || echo.expected() || wakes.stopping());
        if wakes.stopping() {
            return (len, Gathered::Stopped);
        }
        // a key was typed while this waited for its turn
        if len > 0 && echo.awaited().is_some() {
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
        if (full || turn.alone()) && turn.asking.now() {
            let asked = ask_again(pty, &mut buf[len..]);
            turn.asking.answered(&asked);
            match asked {
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

/// Most chances to ask again a reader lets go after asks that found
/// the pty dry.
const MOST_LET_GO: u32 = 16;

/// How asking again has gone lately for one reader.
///
/// A writer kept off its core leaves the pty dry for longer than the
/// sixteen reads of an ask: the session and the writer in the
/// background with six spinning programs, 32 MB of cat-ascii took
/// 32 000 asks, 98 % of them dry, half a million empty reads -- 48 for
/// each piece the flood came in.  Each dry ask lets the next one, two,
/// four, up to sixteen chances go, and the reader waits on its kqueue
/// instead: 2 300 asks, 35 000 empty reads, 39 % less CPU and 20 %
/// fewer instructions a byte, in the same time.  A writer that keeps
/// up answers nearly every ask, and loses nothing.
#[derive(Default)]
pub(super) struct Asking {
    let_go: u32,
    owed: u32,
}

impl Asking {
    /// Whether to ask now, rather than let this chance go.
    fn now(&mut self) -> bool {
        if self.owed == 0 {
            return true;
        }
        self.owed -= 1;
        false
    }

    fn answered(&mut self, asked: &Asked) {
        if matches!(asked, Asked::Dry) {
            self.let_go = (self.let_go * 2).clamp(1, MOST_LET_GO);
            self.owed = self.let_go;
        } else {
            *self = Self::default();
        }
    }
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

#[cfg(test)]
mod tests {
    /// Dry asks let ever more chances go, up to a limit; one that is
    /// answered asks again at every chance.
    #[test]
    fn dry_asks_let_more_chances_go_until_one_is_answered() {
        let mut asking = Asking::default();
        let let_go = |asking: &mut Asking| (0..64).take_while(|_| !asking.now()).count();
        let mut seen = Vec::new();
        for _ in 0..7 {
            assert!(asking.now());
            asking.answered(&Asked::Dry);
            seen.push(let_go(&mut asking));
        }
        assert_eq!(seen, [1, 2, 4, 8, 16, 16, 16]);
        assert!(asking.now());
        asking.answered(&Asked::Got(1));
        assert!(asking.now() && asking.now());
        asking.answered(&Asked::Dry);
        assert_eq!(let_go(&mut asking), 1, "an answer starts the count over");
    }

    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use super::super::tests::spawn;
    use super::super::{PtyReader, channel};
    use super::*;

    /// A byte into an idle PTY is in the parser's queue about as soon
    /// as a plain `poll` and `read` would have it: nothing on the way
    /// waits for more output or for a turn.
    #[test]
    fn a_lone_byte_is_not_held_back() {
        const ROUNDS: usize = 40;
        // The tty echoes what is typed, and the program takes each byte
        // as it comes: a line with no end holds only MAX_CANON bytes,
        // and past that the tty answers with a bell.
        const TYPED_AT: &str = "stty -icanon; cat >/dev/null";
        let plain = spawn(TYPED_AT);
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

        let pty = spawn(TYPED_AT);
        let exited = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let reader = PtyReader::spawn(1, Arc::clone(&pty), exited, tx, || {});
        // A byte held back waits at least `TAIL_WAIT` every time, while
        // the two threads it crosses may each wait for a core on a busy
        // machine: this tries until one round is quick, and the time
        // limit only tells never from not yet.  However slow the plain
        // read was, a quick round stays under what holding back costs.
        let quick = (plain_best + TAIL_WAIT / 2).min(TAIL_WAIT * 3 / 4);
        let give_up = Instant::now() + Duration::from_secs(30);
        let (mut best, mut rounds) = (Duration::MAX, 0);
        while best >= quick {
            assert!(
                rounds < ROUNDS || Instant::now() < give_up,
                "in {rounds} rounds a lone byte took {best:?} at best through the reader and {plain_best:?} through a plain read"
            );
            let started = Instant::now();
            pty.write_shared(b"x").expect("type at the pty");
            let chunk = rx.recv_timeout(Duration::from_secs(5)).expect("no echo");
            best = best.min(started.elapsed());
            rounds += 1;
            assert_eq!(chunk.bytes(), b"x");
            reader.recycle(chunk);
        }
        reader.stop();
    }
}

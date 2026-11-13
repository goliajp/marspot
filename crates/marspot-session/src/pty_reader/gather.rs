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

//! A reader whose output has stopped gives its chunks' pages back.
//!
//! Three 64 KiB chunks that a flood filled stayed resident for the rest
//! of the pane's life.  When the reader is about to wait with no
//! deadline after a read that filled far into a chunk, it gives back
//! the pages of the chunk in its hand and of those queued back to it;
//! a chunk the parser still holds then gives its own back as it is
//! handed back, on the parse thread.
//!
//! The first 16 KiB of each chunk keep their pages, and a chunk whose
//! pages are given back is read into only that far: a keystroke's echo
//! or a prompt fits, and takes nothing back.  A read that fills it is a
//! flood starting, and the chunk takes the rest of its pages back for
//! its next use.  Taking them back before every read, the first three
//! keystrokes after a flood took back all three chunks for good.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;

use marspot_term::idle_pages::{KEPT, WORTH_GIVING_BACK};

use super::{CHUNKS, Chunk};

impl Chunk {
    /// Give the pages back if a read filled far into this chunk.
    pub(super) fn give_back_if_far(&mut self) {
        if self.high >= WORTH_GIVING_BACK {
            self.given.give_back(&mut self.buf, KEPT, self.high);
            self.high = 0;
        }
    }

    /// How much of the chunk a read may fill.
    pub(super) fn room(&self) -> usize {
        self.given.room(self.buf.len())
    }

    /// A read filled what this chunk kept: it takes the rest back.
    pub(super) fn take_back_if_full(&mut self) {
        if self.given.is_given() && self.len >= self.room() {
            self.given.take_back(&mut self.buf);
        }
    }
}

/// The reader thread's side.
pub(super) struct Rest {
    /// Set while the reader waits after a flood: chunks handed back
    /// then give their pages back.  Shared with the parse thread.
    resting: Arc<AtomicBool>,
    /// Chunks taken back from the queue to give their pages back, to be
    /// read into before any other.
    rested: Vec<Chunk>,
    /// A read has filled far into a chunk since the reader last rested.
    far: bool,
}

impl Rest {
    pub(super) fn new(resting: Arc<AtomicBool>) -> Self {
        // the queue holds the chunks and the one `stop` adds
        Self {
            resting,
            rested: Vec::with_capacity(CHUNKS + 1),
            far: false,
        }
    }

    /// A chunk taken back while resting, if any is left.
    pub(super) fn take(&mut self) -> Option<Chunk> {
        self.rested.pop()
    }

    /// A read filled `chunk` this far.
    pub(super) fn filled(&mut self, chunk: &mut Chunk) {
        chunk.high = chunk.high.max(chunk.len);
        self.far |= chunk.len >= WORTH_GIVING_BACK;
    }

    /// The reader is about to wait, with no deadline when `deadline` is
    /// false: after a flood, `chunk` and the chunks queued back give
    /// their pages back.  A chunk not in hand by then keeps this owed
    /// until the next wait, in case it slipped past both looks.
    pub(super) fn before_wait(
        &mut self,
        chunk: &mut Chunk,
        empties: &Receiver<Chunk>,
        deadline: bool,
    ) {
        if !self.far || deadline {
            return;
        }
        // before the queue is looked at: a chunk handed back after the
        // look gives its pages back on the way
        self.resting.store(true, Ordering::SeqCst);
        chunk.give_back_if_far();
        while let Ok(mut queued) = empties.try_recv() {
            queued.give_back_if_far();
            self.rested.push(queued);
        }
        self.far = 1 + self.rested.len() < CHUNKS;
    }

    /// Output again.
    pub(super) fn woke(&self) {
        // looked at first: during a flood it is never set, and the
        // parse thread reads it for every chunk
        if self.resting.load(Ordering::Relaxed) {
            self.resting.store(false, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use marspot_term::idle_pages::{times_given_back, times_taken_back};

    use super::super::tests::spawn;
    use super::super::{PtyReader, channel};
    use super::*;

    /// Wait until `done` holds; the time limit only tells never from
    /// not yet.
    fn until(what: &str, done: impl Fn() -> bool) {
        let give_up = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < give_up, "{what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// After a flood, the chunks the reader holds give their pages back
    /// when it goes to sleep, the one the parser still holds when it is
    /// handed back; keystrokes' echoes after it take nothing back, and
    /// the next flood takes them back.
    #[test]
    fn a_flood_s_chunks_give_their_pages_back_and_the_next_flood_takes_them_back() {
        const FLOOD: usize = 8_000_000;
        const NEXT: usize = 1_000_000;
        let pty = spawn(
            "head -c 8000000 /dev/zero | tr '\\0' a; sleep 5; for i in 1 2 3; do printf hello; sleep 0.3; done; \
             sleep 0.5; head -c 1000000 /dev/zero | tr '\\0' b; sleep 30",
        );
        let (tx, rx) = channel();
        let reader = PtyReader::spawn(1, Arc::clone(&pty), Arc::default(), tx, || {});
        let given = times_given_back();
        let mut total = 0;
        // The last chunk the flood filled far stays with this "parser"
        // until the reader has gone to sleep.  While one is out the
        // reader never holds all its chunks, so it still owes a rest
        // when the flood ends, however small the reads after it were.
        let mut held: Option<Chunk> = None;
        while total < FLOOD {
            let chunk = rx.recv_timeout(Duration::from_secs(30)).expect("the flood stopped short");
            total += chunk.bytes().len();
            if chunk.bytes().len() >= WORTH_GIVING_BACK {
                if let Some(earlier) = held.replace(chunk) {
                    reader.recycle(earlier);
                }
            } else {
                reader.recycle(chunk);
            }
        }
        assert_eq!(total, FLOOD);
        let held = held.expect("no chunk of the flood was filled far");
        until("the reader gave back none of the chunks it holds", || times_given_back() >= given + CHUNKS - 1);
        // a reader that may still be at it is not done
        let mut settled = times_given_back();
        loop {
            std::thread::sleep(Duration::from_millis(200));
            let now = times_given_back();
            if now == settled {
                break;
            }
            settled = now;
        }
        reader.recycle(held);
        assert_eq!(times_given_back(), settled + 1, "the chunk handed back after the flood kept its pages");

        // Three reads go round the three chunks, so one of them goes
        // into the chunk just handed back, whose pages are given back.
        let taken = times_taken_back();
        let mut after = Vec::new();
        while after.len() < 15 {
            let chunk = rx.recv_timeout(Duration::from_secs(30)).expect("no output after the flood");
            after.extend_from_slice(chunk.bytes());
            reader.recycle(chunk);
        }
        assert_eq!(after, b"hellohellohello");
        assert_eq!(times_taken_back(), taken, "a keystroke's echo took pages back");

        let mut next = 0;
        while next < NEXT {
            let chunk = rx.recv_timeout(Duration::from_secs(30)).expect("the next flood stopped short");
            assert!(chunk.bytes().iter().all(|&b| b == b'b'), "only what was written");
            next += chunk.bytes().len();
            reader.recycle(chunk);
        }
        assert_eq!(next, NEXT);
        assert!(times_taken_back() > taken, "the next flood was read into what the chunks kept, and no more");
        reader.stop();
    }

    /// Output that never fills far into a chunk -- keystrokes' echoes,
    /// a prompt, a repaint -- never gives pages back, however often it
    /// stops.
    #[test]
    fn bursts_that_fill_little_of_a_chunk_give_nothing_back() {
        const BURST: usize = 20_000;
        const BURSTS: usize = 10;
        let pty = spawn("for i in 1 2 3 4 5 6 7 8 9 10; do head -c 20000 /dev/zero | tr '\\0' b; sleep 0.05; done; sleep 30");
        let (tx, rx) = channel();
        let reader = PtyReader::spawn(1, Arc::clone(&pty), Arc::default(), tx, || {});
        let given = times_given_back();
        let mut total = 0;
        while total < BURST * BURSTS {
            let chunk = rx.recv_timeout(Duration::from_secs(30)).expect("the bursts stopped short");
            assert!(chunk.bytes().len() < WORTH_GIVING_BACK, "a chunk of {} bytes", chunk.bytes().len());
            total += chunk.bytes().len();
            reader.recycle(chunk);
        }
        // long enough for the last burst's wait to have run out
        std::thread::sleep(Duration::from_millis(100));
        reader.stop();
        assert_eq!(times_given_back(), given);
    }
}

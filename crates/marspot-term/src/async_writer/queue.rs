//! What the producer hands to the writer thread, and the queue between
//! the two.

use std::collections::VecDeque;
use std::sync::mpsc::SyncSender;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use crate::idle_pages::{Given, KEPT, WORTH_GIVING_BACK};

/// How long the writer leaves buffers queued in case more follow.
///
/// Every buffer written as it arrived is a thread woken, and a flood
/// hands over thousands a second: a `cat` of 134 MB woke the three
/// writers of a pane 9 000 times in half a second, and the program
/// writing into the pty -- which sleeps and is woken once a kilobyte
/// -- was a tenth slower for it.  A writer that has just written waits
/// this long for its queue to fill and writes the lot on one wake.
/// Output that has stopped does not wait: see [`super::AsyncWriter::hand_off`].
const LINGER: Duration = Duration::from_millis(2);

/// What is handed over at once: bytes for the file and, for a writer
/// with a side file, the bytes that go with them.
pub(super) struct Buf {
    pub(super) main: Vec<u8>,
    pub(super) side: Vec<u8>,
    /// A change of files at a point of this buffer.
    pub(super) cut: Option<Cut>,
    /// Furthest the main bytes have reached since the buffer's pages
    /// were last given back.
    pub(super) high: usize,
    pub(super) given: Given,
}

/// What the writer does to its files once it has written a buffer up to
/// `main_at` and `side_at`.
pub(super) struct Cut {
    pub(super) main_at: usize,
    pub(super) side_at: usize,
    pub(super) then: Box<dyn FnOnce(&mut super::Files) + Send>,
}

impl Buf {
    pub(super) fn sized(cap: usize) -> Self {
        Self { main: Vec::with_capacity(cap), side: Vec::new(), cut: None, high: 0, given: Given::default() }
    }

    pub(super) fn clear(&mut self) {
        self.high = self.high.max(self.main.len());
        self.main.clear();
        self.side.clear();
        self.cut = None;
    }

    pub(super) fn is_empty(&self) -> bool {
        self.main.is_empty() && self.side.is_empty() && self.cut.is_none()
    }

    /// An empty buffer that a burst wrote far into.
    pub(super) fn worth_giving_back(&self) -> bool {
        self.main.is_empty() && !self.given.is_given() && self.high >= WORTH_GIVING_BACK
    }

    /// Give back the pages a burst wrote, all but the first [`KEPT`]
    /// bytes.
    pub(super) fn give_back(&mut self) {
        self.given.give_back(&mut self.main, KEPT, self.high);
        self.high = 0;
    }
}

pub(super) enum Msg {
    /// A full buffer to append.  Returned to the free list afterwards.
    Data(Buf),
    /// Append this, then acknowledge — the barrier `flush()` waits on.
    Barrier(Buf, SyncSender<()>),
    /// From here on a buffer's side bytes are appended to this file,
    /// after its main bytes.
    Side(super::Target),
}

/// What the producer and the writer thread share.
pub(super) struct Queue {
    state: Mutex<State>,
    /// The writer waits here: for work, or for more of it.
    work: Condvar,
    /// The producer waits here while `depth` buffers are in flight.
    room: Condvar,
    depth: usize,
    /// Most buffers the free list keeps.
    keep_free: usize,
}

#[derive(Default)]
struct State {
    queued: VecDeque<Msg>,
    /// Taken off the queue and not yet written.
    writing: usize,
    /// Something queued must not wait for company.
    now: bool,
    /// The writer is asleep with nothing to do and no deadline.
    idle: bool,
    closed: bool,
    /// A hand-over was refused while the writer was busy, and the
    /// producer has not been told that it can be made again.
    owed: bool,
    /// Written buffers, for the producer to fill again.  Here rather
    /// than in a channel so that the writer can give their pages back
    /// once it has nothing to do.
    free: Vec<Buf>,
}

impl Queue {
    /// A queue for `depth` buffers in flight, with `seed` free ones of
    /// `cap` bytes so that the first hand-overs allocate nothing.
    pub(super) fn new(depth: usize, seed: usize, cap: usize) -> Self {
        let keep_free = depth + 2;
        let mut free = Vec::with_capacity(keep_free);
        free.extend((0..seed.min(keep_free)).map(|_| Buf::sized(cap)));
        let state = State { free, ..State::default() };
        Self { state: Mutex::new(state), work: Condvar::new(), room: Condvar::new(), depth, keep_free }
    }

    /// A written buffer to fill again, if the writer has one.
    pub(super) fn take_free(&self) -> Option<Buf> {
        self.state.lock().expect("the writer queue").free.pop()
    }

    /// One of the taken messages is done with, and this is its buffer,
    /// cleared.
    pub(super) fn wrote_back(&self, buf: Buf) {
        let mut st = self.state.lock().expect("the writer queue");
        st.writing -= 1;
        // more than the free list keeps is dropped once the lock is
        // let go: the next hand-over allocates
        let extra = if st.free.len() < self.keep_free {
            st.free.push(buf);
            None
        } else {
            Some(buf)
        };
        drop(st);
        self.room.notify_one();
        drop(extra);
    }

    /// Free buffers, those of them whose pages are given back, and
    /// whether the writer is asleep with nothing left to do: until then
    /// it may hold a buffer it is writing or giving back, which is in
    /// neither count.
    #[cfg(test)]
    pub(super) fn free_pages(&self) -> (usize, usize, bool) {
        let st = self.state.lock().expect("the writer queue");
        (
            st.free.len(),
            st.free.iter().filter(|b| b.given.is_given()).count(),
            st.idle && st.queued.is_empty(),
        )
    }

    /// The producer is gone: the writer ends once it has written what
    /// is queued.
    pub(super) fn close(&self) {
        self.state.lock().expect("the writer queue").closed = true;
        self.work.notify_one();
    }

    /// Queue `msg`, waiting for room; `now` when it must not linger.
    pub(super) fn push(&self, msg: Msg, now: bool) {
        let mut st = self.state.lock().expect("the writer queue");
        while st.queued.len() + st.writing >= self.depth {
            self.work.notify_one();
            st = self.room.wait(st).expect("the writer queue");
        }
        self.put(&mut st, msg, now);
    }

    /// Queue `msg` if there is room, without waiting.  A refusal is
    /// owed an answer: see [`Self::owe`].
    pub(super) fn try_push(&self, msg: Msg) -> Result<(), Msg> {
        let mut st = self.state.lock().expect("the writer queue");
        if st.queued.len() + st.writing >= self.depth {
            st.owed = true;
            return Err(msg);
        }
        self.put(&mut st, msg, true);
        Ok(())
    }

    /// The producer has bytes it could not hand over because the
    /// writer holds every buffer.  `true` when the writer is at work:
    /// what it has is written without waiting for more, and
    /// [`Self::settle`] says so once.  `false` when it has nothing, and
    /// so holds no buffer either.
    pub(super) fn owe(&self) -> bool {
        let mut st = self.state.lock().expect("the writer queue");
        if st.queued.is_empty() && st.writing == 0 {
            return false;
        }
        st.owed = true;
        st.now = true;
        self.work.notify_one();
        true
    }

    /// The writer's side, between two batches: whether a hand-over was
    /// refused since it last asked.
    pub(super) fn settle(&self) -> bool {
        std::mem::take(&mut self.state.lock().expect("the writer queue").owed)
    }

    fn put(&self, st: &mut State, msg: Msg, now: bool) {
        st.queued.push_back(msg);
        st.now |= now;
        if st.now || st.idle || st.queued.len() + st.writing >= self.depth {
            self.work.notify_one();
        }
    }

    /// Whatever is queued is written without waiting for more.
    pub(super) fn hurry(&self) {
        let mut st = self.state.lock().expect("the writer queue");
        if !st.queued.is_empty() {
            st.now = true;
            self.work.notify_one();
        }
    }

    /// The writer's side: everything queued, once it is time to write
    /// it; `None` when the producer is gone and nothing is left.
    pub(super) fn take(&self) -> Option<VecDeque<Msg>> {
        let mut st = self.state.lock().expect("the writer queue");
        // it has just written: more may be a moment away
        let mut lingering = true;
        loop {
            let full = st.queued.len() >= self.depth;
            if !st.queued.is_empty() && (st.now || full || st.closed || !lingering) {
                break;
            }
            if st.queued.is_empty() && st.closed {
                return None;
            }
            if lingering {
                let (guard, waited) = self.work.wait_timeout(st, LINGER).expect("the writer queue");
                st = guard;
                lingering = !waited.timed_out();
            } else {
                // Nothing to do and no deadline: output has stopped.
                // What a burst wrote far into is given back, a buffer
                // at a time, so that a producer starting again finds
                // the rest of the free list there.
                if let Some(i) = st.free.iter().position(Buf::worth_giving_back) {
                    let mut buf = st.free.swap_remove(i);
                    drop(st);
                    buf.give_back();
                    st = self.state.lock().expect("the writer queue");
                    st.free.push(buf);
                    continue;
                }
                st.idle = true;
                st = self.work.wait(st).expect("the writer queue");
                st.idle = false;
                // the first of a burst: see whether more follow
                lingering = true;
            }
        }
        st.now = false;
        st.writing = st.queued.len();
        Some(std::mem::take(&mut st.queued))
    }

    /// One of the taken messages is done with.
    pub(super) fn wrote(&self) {
        self.state.lock().expect("the writer queue").writing -= 1;
        self.room.notify_one();
    }
}

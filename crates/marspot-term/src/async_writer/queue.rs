//! What the producer hands to the writer thread, and the queue between
//! the two.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::SyncSender;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

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
}

impl Buf {
    pub(super) fn sized(cap: usize) -> Self {
        Self { main: Vec::with_capacity(cap), side: Vec::new() }
    }

    pub(super) fn clear(&mut self) {
        self.main.clear();
        self.side.clear();
    }

    pub(super) fn is_empty(&self) -> bool {
        self.main.is_empty() && self.side.is_empty()
    }
}

pub(super) enum Msg {
    /// A full buffer to append.  Returned to the free list afterwards.
    Data(Buf),
    /// Append this, then acknowledge — the barrier `flush()` waits on.
    Barrier(Buf, SyncSender<()>),
    /// Cut the file back to `len` and the side file to nothing (a
    /// clear / restart).  Goes through the queue so it cannot race the
    /// writes ahead of it.
    Truncate(u64, SyncSender<()>),
    /// From here on a buffer's side bytes are appended to this file,
    /// after its main bytes.
    Side(std::fs::File),
    /// Replace the file being written (rotation).  Anything queued
    /// ahead of it still lands in the old file, which is what makes a
    /// rotation atomic from the producer's point of view.
    Swap(std::fs::File, SyncSender<()>),
    /// Rename the file being written from `live` to `retired` and go
    /// on in a fresh file at `live`.  Nobody waits for it: the writer
    /// does it when it gets here, after everything queued ahead.
    Rotate { live: PathBuf, retired: PathBuf },
}

/// What the producer and the writer thread share.
pub(super) struct Queue {
    state: Mutex<State>,
    /// The writer waits here: for work, or for more of it.
    work: Condvar,
    /// The producer waits here while `depth` buffers are in flight.
    room: Condvar,
    depth: usize,
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
}

impl Queue {
    pub(super) fn new(depth: usize) -> Self {
        Self { state: Mutex::new(State::default()), work: Condvar::new(), room: Condvar::new(), depth }
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

    /// Queue `msg` if there is room, without waiting.
    pub(super) fn try_push(&self, msg: Msg) -> Result<(), Msg> {
        let mut st = self.state.lock().expect("the writer queue");
        if st.queued.len() + st.writing >= self.depth {
            return Err(msg);
        }
        self.put(&mut st, msg, true);
        Ok(())
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
